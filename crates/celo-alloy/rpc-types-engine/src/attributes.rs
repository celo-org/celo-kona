//! Celo-specific payload attribute helpers.

use alloy_eips::eip2718::{Eip2718Result, WithEncoded};
use celo_alloy_consensus::{CeloTxEnvelope, decode_2718_canonical};
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
        // Require each entry to be the transaction's canonical EIP-2718 encoding: a body that
        // decodes to a typed transaction but does not re-encode to itself (for example a typed
        // body with its type byte stripped, or a `0x00`-tagged legacy) is rejected rather than
        // normalised. See ethereum-optimism/optimism#22778.
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

    #[test]
    fn test_celo_decoded_transactions_trailing_bytes() {
        let mut encoded = cip64_envelope().encoded_2718();
        encoded.push(0x00);
        let attributes =
            OpPayloadAttributes { transactions: Some(vec![encoded.into()]), ..Default::default() };

        assert!(attributes.celo_decoded_transactions().next().unwrap().is_err());
    }

    /// A bare CIP-64 body (type byte stripped) decodes to a transaction but re-encodes
    /// differently, so it must be rejected rather than silently canonicalised. See
    /// ethereum-optimism/optimism#22778.
    #[test]
    fn test_celo_decoded_transactions_reject_non_canonical_encoding() {
        let encoded = cip64_envelope().encoded_2718();
        assert_eq!(encoded[0], 0x7b);
        let bare: alloy_primitives::Bytes = alloy_primitives::Bytes::copy_from_slice(&encoded[1..]);

        let attributes =
            OpPayloadAttributes { transactions: Some(vec![bare]), ..Default::default() };

        assert!(attributes.celo_decoded_transactions().next().unwrap().is_err());
    }
}
