//! Tests running upstream's canonical EIP-2718 decoding against [`CeloTxEnvelope`], which
//! carries the CIP-64 (`0x7b`) type that `OpTxEnvelope` lacks.

use crate::{CeloTxEnvelope, CeloTxType, TxCip64, decode_2718_canonical};
use alloy_consensus::{SignableTransaction, TxEip1559, TxEip2930, TxEip7702, TxLegacy, Typed2718};
use alloy_eips::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256, hex};
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
fn untagged_fallback_decode_only_accepts_legacy() {
    // Per EIP-2718, a byte string with no type-byte prefix (first byte >= 0xc0, i.e. it
    // is an RLP list) can only ever represent a legacy transaction; if it fails the legacy
    // schema, decoding must fail outright -- it must never be reinterpreted as some other
    // (typed) transaction kind.
    //
    // This 107-byte RLP list is invalid as a legacy tx: list-index 3 (the would-be `to`)
    // is a 3-byte string, not a 20-byte address or an empty string, and list-index 8 (the
    // would-be `s`) is RLP-typed as an empty list, not an integer. But the very same bytes
    // happen to be a byte-for-byte-valid 12-field `TxEip1559` list (chain_id, nonce,
    // max_priority_fee_per_gas, max_fee_per_gas, gas_limit, to, value, input, access_list,
    // y_parity, r, s). Before the fix, `EthereumTxEnvelope`'s generated `fallback_decode`
    // tried every declared variant in enum-declaration order and accepted this input as
    // `Eip1559`, since that's the first (and only) variant whose raw-field RLP schema
    // happens to fully consume the buffer.
    let untagged_invalid_as_legacy_valid_as_eip1559 = hex!(
        "f8690180830f4240830f424083ffff0c94e1000000000000000000000000000000c0de0000"
        "8020c080a079be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
        "a05168112c1d71e40b6b60530adbd2c4aaf9e4f94b6cca0f5717fac601350af7d3"
    );
    assert!(
        CeloTxEnvelope::decode_2718_exact(&untagged_invalid_as_legacy_valid_as_eip1559).is_err(),
        "untagged input that is not a valid legacy tx must be rejected, not reinterpreted \
         as another transaction type"
    );

    // Guard against over-correction (1): a genuine untagged legacy tx must still decode as
    // `Legacy`.
    let legacy = TxLegacy {
        chain_id: None,
        nonce: 2,
        gas_limit: 1_000_000,
        gas_price: 10_000_000_000,
        to: Address::left_padding_from(&[6]).into(),
        value: U256::from(7_u64),
        ..Default::default()
    }
    .into_signed(Signature::test_signature().with_parity(true));
    let legacy_envelope: CeloTxEnvelope = legacy.into();
    let legacy_encoded = legacy_envelope.encoded_2718();
    assert_eq!(
        CeloTxEnvelope::decode_2718_exact(&legacy_encoded).unwrap(),
        legacy_envelope,
        "a genuine legacy transaction must still decode correctly"
    );

    // Guard against over-correction (2): a genuine, correctly type-tagged (0x02-prefixed)
    // EIP-1559 tx must still decode as `Eip1559` via `typed_decode` (not `fallback_decode`).
    let eip1559 = TxEip1559 {
        chain_id: 1,
        nonce: 2,
        max_fee_per_gas: 3,
        max_priority_fee_per_gas: 4,
        gas_limit: 5,
        to: Address::left_padding_from(&[6]).into(),
        value: U256::from(7_u64),
        input: vec![8].into(),
        access_list: Default::default(),
    }
    .into_signed(Signature::test_signature());
    let eip1559_envelope: CeloTxEnvelope = eip1559.into();
    let eip1559_encoded = eip1559_envelope.encoded_2718();
    assert_eq!(eip1559_encoded[0], 0x02, "sanity: must be type-tagged, not untagged");
    assert_eq!(
        CeloTxEnvelope::decode_2718_exact(&eip1559_encoded).unwrap(),
        eip1559_envelope,
        "a genuine type-tagged EIP-1559 transaction must still decode correctly"
    );
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
