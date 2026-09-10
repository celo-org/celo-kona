//! Canonical EIP-2718 decoding.
//!
//! Ported from `op-alloy-consensus` (celo-org/optimism@85f7c13d7470a66254081f0177fc297201c07459,
//! upstream ethereum-optimism/optimism#22778 "reject non-canonical transaction encodings"). The
//! Celo copy exists because Celo decodes consensus transaction bytes as
//! [`CeloTxEnvelope`](crate::CeloTxEnvelope), which carries the CIP-64 (`0x7b`) type, rather than
//! `OpTxEnvelope`.

use alloy_eips::{
    Decodable2718, Encodable2718,
    eip2718::{Eip2718Error, Eip2718Result},
};

/// Decodes an EIP-2718 transaction and requires `bytes` to be its canonical encoding.
///
/// Plain `decode_2718_exact` accepts inputs that re-encode differently, such as a typed
/// transaction body without its type byte or a legacy transaction carrying a `0x00` tag. Where the
/// bytes are consensus input they must be rejected rather than normalised, so anything that does
/// not round-trip is an error. The re-encode is one linear pass per transaction, far cheaper than
/// the signer recovery that follows it.
pub fn decode_2718_canonical<T: Decodable2718 + Encodable2718>(bytes: &[u8]) -> Eip2718Result<T> {
    let tx = T::decode_2718_exact(bytes)?;
    // Length first: it is free and skips the re-encode allocation on a mismatch.
    if tx.encode_2718_len() != bytes.len() || tx.encoded_2718() != bytes {
        return Err(Eip2718Error::RlpError(alloy_rlp::Error::Custom(
            "non-canonical transaction encoding",
        )));
    }
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CeloTxEnvelope, CeloTxType, TxCip64};
    use alloy_consensus::{
        SignableTransaction, TxEip1559, TxEip2930, TxEip7702, TxLegacy, Typed2718,
    };
    use alloy_eips::Encodable2718;
    use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256};
    use op_alloy_consensus::TxDeposit;
    use std::{vec, vec::Vec};

    /// Canonical encodings of every [`CeloTxEnvelope`] variant.
    fn canonical_encodings() -> Vec<(CeloTxType, Vec<u8>)> {
        let sig = Signature::test_signature();
        let to = Address::repeat_byte(0x11);
        vec![
            (
                CeloTxType::Legacy,
                TxLegacy {
                    chain_id: Some(42220),
                    nonce: 1,
                    gas_price: 2,
                    gas_limit: 21_000,
                    to: to.into(),
                    value: U256::from(1u64),
                    input: Bytes::new(),
                }
                .into_signed(sig)
                .encoded_2718(),
            ),
            (
                CeloTxType::Eip2930,
                TxEip2930 {
                    chain_id: 42220,
                    nonce: 1,
                    gas_price: 2,
                    gas_limit: 21_000,
                    to: to.into(),
                    value: U256::from(1u64),
                    access_list: Default::default(),
                    input: Bytes::new(),
                }
                .into_signed(sig)
                .encoded_2718(),
            ),
            (
                CeloTxType::Eip1559,
                TxEip1559 {
                    chain_id: 42220,
                    nonce: 1,
                    gas_limit: 21_000,
                    max_fee_per_gas: 2,
                    max_priority_fee_per_gas: 1,
                    to: to.into(),
                    value: U256::from(1u64),
                    access_list: Default::default(),
                    input: Bytes::new(),
                }
                .into_signed(sig)
                .encoded_2718(),
            ),
            (
                CeloTxType::Eip7702,
                TxEip7702 {
                    chain_id: 42220,
                    nonce: 1,
                    gas_limit: 21_000,
                    max_fee_per_gas: 2,
                    max_priority_fee_per_gas: 1,
                    to,
                    value: U256::from(1u64),
                    access_list: Default::default(),
                    authorization_list: vec![],
                    input: Bytes::new(),
                }
                .into_signed(sig)
                .encoded_2718(),
            ),
            (
                CeloTxType::Cip64,
                TxCip64 {
                    chain_id: 42220,
                    nonce: 1,
                    gas_limit: 100_000,
                    max_fee_per_gas: 2,
                    max_priority_fee_per_gas: 1,
                    to: TxKind::Call(to),
                    value: U256::ZERO,
                    access_list: Default::default(),
                    fee_currency: Some(Address::with_last_byte(0x42)),
                    input: Bytes::new(),
                }
                .into_signed(sig)
                .encoded_2718(),
            ),
            (
                CeloTxType::Deposit,
                TxDeposit {
                    source_hash: B256::with_last_byte(2),
                    from: Address::repeat_byte(0x42),
                    to: TxKind::Call(to),
                    mint: 1,
                    value: U256::from(1u64),
                    gas_limit: 50_000,
                    is_system_transaction: false,
                    input: Bytes::new(),
                }
                .encoded_2718(),
            ),
        ]
    }

    #[test]
    fn accepts_canonical_encodings() {
        for (ty, encoded) in canonical_encodings() {
            let tx: CeloTxEnvelope =
                decode_2718_canonical(&encoded).unwrap_or_else(|e| panic!("{ty:?}: {e}"));
            assert_eq!(tx.ty(), ty as u8);
            assert_eq!(tx.encoded_2718(), encoded);
        }
    }

    #[test]
    fn rejects_typed_bodies_without_type_byte() {
        for (ty, encoded) in canonical_encodings() {
            if ty == CeloTxType::Legacy {
                continue;
            }
            assert_eq!(encoded[0], ty as u8);
            let bare = &encoded[1..];
            assert!(bare[0] > 0x7F, "{ty:?}: bare body must not start with a type byte");
            assert!(
                decode_2718_canonical::<CeloTxEnvelope>(bare).is_err(),
                "{ty:?}: body without its type byte must be rejected"
            );
        }
    }

    #[test]
    fn rejects_type_tagged_legacy() {
        let legacies: Vec<_> =
            canonical_encodings().into_iter().filter(|(ty, _)| *ty == CeloTxType::Legacy).collect();
        assert!(!legacies.is_empty());
        for (_, legacy) in legacies {
            assert!(legacy[0] > 0x7F);
            let mut tagged = vec![CeloTxType::Legacy as u8];
            tagged.extend_from_slice(&legacy);
            assert!(decode_2718_canonical::<CeloTxEnvelope>(&tagged).is_err());
        }
    }

    #[test]
    fn rejects_trailing_bytes() {
        for (ty, mut encoded) in canonical_encodings() {
            encoded.push(0);
            assert!(decode_2718_canonical::<CeloTxEnvelope>(&encoded).is_err(), "{ty:?}");
        }
    }
}
