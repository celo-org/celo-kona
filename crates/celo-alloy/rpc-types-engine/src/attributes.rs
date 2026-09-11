//! Celo-specific payload attribute helpers.

use alloy_eips::eip2718::{Eip2718Result, WithEncoded};
use celo_alloy_consensus::CeloTxEnvelope;
use op_alloy_consensus::decode_2718_canonical;
use op_alloy_rpc_types_engine::OpPayloadAttributes;

/// Celo extension methods for [`OpPayloadAttributes`].
///
/// These decode the attributes' transaction bytes as [`CeloTxEnvelope`] (which includes the
/// CIP-64 transaction type) instead of `OpTxEnvelope`. The methods are `celo_`-prefixed so the
/// inherent [`OpPayloadAttributes`] methods of the same shape don't shadow them.
pub trait CeloPayloadAttributesExt {
    /// Returns an iterator over the decoded [`CeloTxEnvelope`] in this attributes.
    ///
    /// This iterator will be empty if there are no transactions in the attributes.
    fn celo_decoded_transactions(&self)
    -> impl Iterator<Item = Eip2718Result<CeloTxEnvelope>> + '_;

    /// Returns iterator over decoded transactions with their original encoded bytes.
    ///
    /// This iterator will be empty if there are no transactions in the attributes.
    fn celo_decoded_transactions_with_encoded(
        &self,
    ) -> impl Iterator<Item = Eip2718Result<WithEncoded<CeloTxEnvelope>>> + '_;

    /// Returns an iterator over the recovered [`CeloTxEnvelope`] in this attributes.
    ///
    /// This iterator will be empty if there are no transactions in the attributes.
    #[cfg(feature = "k256")]
    fn celo_recovered_transactions(
        &self,
    ) -> impl Iterator<
        Item = Result<
            alloy_consensus::transaction::Recovered<CeloTxEnvelope>,
            alloy_consensus::crypto::RecoveryError,
        >,
    > + '_;

    /// Returns an iterator over the recovered [`CeloTxEnvelope`] in this attributes with their
    /// original encoded bytes.
    ///
    /// This iterator will be empty if there are no transactions in the attributes.
    #[cfg(feature = "k256")]
    fn celo_recovered_transactions_with_encoded(
        &self,
    ) -> impl Iterator<
        Item = Result<
            WithEncoded<alloy_consensus::transaction::Recovered<CeloTxEnvelope>>,
            alloy_consensus::crypto::RecoveryError,
        >,
    > + '_;
}

impl CeloPayloadAttributesExt for OpPayloadAttributes {
    fn celo_decoded_transactions(
        &self,
    ) -> impl Iterator<Item = Eip2718Result<CeloTxEnvelope>> + '_ {
        self.transactions.iter().flatten().map(|tx_bytes| decode_2718_canonical(tx_bytes))
    }

    fn celo_decoded_transactions_with_encoded(
        &self,
    ) -> impl Iterator<Item = Eip2718Result<WithEncoded<CeloTxEnvelope>>> + '_ {
        self.transactions
            .iter()
            .flatten()
            .cloned()
            .zip(self.celo_decoded_transactions())
            .map(|(tx_bytes, result)| result.map(|celo_tx| WithEncoded::new(tx_bytes, celo_tx)))
    }

    #[cfg(feature = "k256")]
    fn celo_recovered_transactions(
        &self,
    ) -> impl Iterator<
        Item = Result<
            alloy_consensus::transaction::Recovered<CeloTxEnvelope>,
            alloy_consensus::crypto::RecoveryError,
        >,
    > + '_ {
        self.celo_decoded_transactions().map(|res| {
            res.map_err(alloy_consensus::crypto::RecoveryError::from_source)
                .and_then(|tx| tx.try_into_recovered())
        })
    }

    #[cfg(feature = "k256")]
    fn celo_recovered_transactions_with_encoded(
        &self,
    ) -> impl Iterator<
        Item = Result<
            WithEncoded<alloy_consensus::transaction::Recovered<CeloTxEnvelope>>,
            alloy_consensus::crypto::RecoveryError,
        >,
    > + '_ {
        self.transactions
            .iter()
            .flatten()
            .cloned()
            .zip(self.celo_recovered_transactions())
            .map(|(tx_bytes, result)| result.map(|celo_tx| WithEncoded::new(tx_bytes, celo_tx)))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use alloc::{vec, vec::Vec};
    use alloy_consensus::SignableTransaction;
    use alloy_eips::Encodable2718;
    use alloy_primitives::{Address, Signature, TxKind, U256};
    use celo_alloy_consensus::TxCip64;

    fn cip64_envelope() -> CeloTxEnvelope {
        let tx = TxCip64 {
            chain_id: 42220,
            nonce: 1,
            gas_limit: 100_000,
            max_fee_per_gas: 2,
            max_priority_fee_per_gas: 1,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            access_list: Default::default(),
            fee_currency: Some(Address::with_last_byte(0x42)),
            input: Default::default(),
        };
        CeloTxEnvelope::Cip64(tx.into_signed(Signature::test_signature()))
    }

    #[test]
    fn test_celo_decoded_transactions_cip64() {
        let envelope = cip64_envelope();
        let attributes = OpPayloadAttributes {
            transactions: Some(vec![envelope.encoded_2718().into()]),
            ..Default::default()
        };

        let decoded = attributes
            .celo_decoded_transactions()
            .collect::<Eip2718Result<Vec<_>>>()
            .expect("decoding should succeed");
        assert_eq!(decoded, vec![envelope]);
    }

    /// A transaction that decodes but does not re-encode to the same bytes (here: a legacy
    /// transaction carrying a `0x00` type tag) must be rejected, not silently canonicalised.
    #[test]
    fn test_celo_decoded_transactions_reject_non_canonical_encoding() {
        let legacy = CeloTxEnvelope::Legacy(
            alloy_consensus::TxLegacy {
                chain_id: Some(42220),
                nonce: 1,
                gas_price: 2,
                gas_limit: 21_000,
                to: TxKind::Call(Address::ZERO),
                value: U256::ZERO,
                input: Default::default(),
            }
            .into_signed(Signature::test_signature()),
        )
        .encoded_2718();
        assert!(legacy[0] > 0x7F, "legacy transactions are untyped");
        let mut tagged = vec![0x00];
        tagged.extend_from_slice(&legacy);

        let attributes =
            OpPayloadAttributes { transactions: Some(vec![tagged.into()]), ..Default::default() };

        let err = attributes.celo_decoded_transactions().next().unwrap().unwrap_err();
        assert!(err.to_string().contains("non-canonical"), "{err}");
    }

    /// A CIP-64 body stripped of its `0x7b` type byte must not be resurrected as a CIP-64
    /// transaction.
    #[test]
    fn test_celo_decoded_transactions_reject_bare_cip64_body() {
        let encoded = cip64_envelope().encoded_2718();
        assert_eq!(encoded[0], 0x7b);
        let attributes = OpPayloadAttributes {
            transactions: Some(vec![encoded[1..].to_vec().into()]),
            ..Default::default()
        };

        assert!(attributes.celo_decoded_transactions().next().unwrap().is_err());
    }

    #[test]
    fn test_celo_decoded_transactions_trailing_bytes() {
        let mut encoded = cip64_envelope().encoded_2718();
        encoded.push(0x00);
        let attributes =
            OpPayloadAttributes { transactions: Some(vec![encoded.into()]), ..Default::default() };

        assert!(attributes.celo_decoded_transactions().next().unwrap().is_err());
    }
}
