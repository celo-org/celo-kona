//! Celo-specific payload attributes.

use alloy_eips::{
    Decodable2718,
    eip1559::BaseFeeParams,
    eip2718::{Eip2718Result, WithEncoded},
};
use alloy_primitives::Bytes;
use alloy_rlp::Result;
use celo_alloy_consensus::CeloTxEnvelope;
use op_alloy_consensus::EIP1559ParamError;
use op_alloy_rpc_types_engine::OpPayloadAttributes;

/// Celo Payload Attributes
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct CeloPayloadAttributes {
    /// Optimism payload attributes
    pub op_payload_attributes: OpPayloadAttributes,
}

impl CeloPayloadAttributes {
    /// Encodes the `eip1559` parameters for the payload.
    pub fn get_holocene_extra_data(
        &self,
        default_base_fee_params: BaseFeeParams,
    ) -> Result<Bytes, EIP1559ParamError> {
        self.op_payload_attributes.get_holocene_extra_data(default_base_fee_params)
    }

    /// Extracts the Holocene 1599 parameters from the encoded form:
    /// <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/holocene/exec-engine.md#eip1559params-encoding>
    ///
    /// Returns (`elasticity`, `denominator`)
    pub fn decode_eip_1559_params(&self) -> Option<(u32, u32)> {
        self.op_payload_attributes.decode_eip_1559_params()
    }

    /// Returns an iterator over the decoded [`CeloTxEnvelope`] in this attributes.
    ///
    /// This iterator will be empty if there are no transactions in the attributes.
    pub fn decoded_transactions(&self) -> impl Iterator<Item = Eip2718Result<CeloTxEnvelope>> + '_ {
        self.op_payload_attributes.transactions.iter().flatten().map(|tx_bytes| {
            let mut buf = tx_bytes.as_ref();
            let tx = CeloTxEnvelope::decode_2718(&mut buf).map_err(alloy_rlp::Error::from)?;
            if !buf.is_empty() {
                return Err(alloy_rlp::Error::UnexpectedLength.into());
            }
            Ok(tx)
        })
    }

    /// Returns iterator over decoded transactions with their original encoded bytes.
    ///
    /// This iterator will be empty if there are no transactions in the attributes.
    pub fn decoded_transactions_with_encoded(
        &self,
    ) -> impl Iterator<Item = Eip2718Result<WithEncoded<CeloTxEnvelope>>> + '_ {
        self.op_payload_attributes
            .transactions
            .iter()
            .flatten()
            .cloned()
            .zip(self.decoded_transactions())
            .map(|(tx_bytes, result)| result.map(|celo_tx| WithEncoded::new(tx_bytes, celo_tx)))
    }

    /// Returns an iterator over the recovered [`CeloTxEnvelope`] in this attributes.
    ///
    /// This iterator will be empty if there are no transactions in the attributes.
    #[cfg(feature = "k256")]
    pub fn recovered_transactions(
        &self,
    ) -> impl Iterator<
        Item = Result<
            alloy_consensus::transaction::Recovered<CeloTxEnvelope>,
            alloy_consensus::crypto::RecoveryError,
        >,
    > + '_ {
        self.decoded_transactions().map(|res| {
            res.map_err(alloy_consensus::crypto::RecoveryError::from_source)
                .and_then(|tx| tx.try_into_recovered())
        })
    }

    /// Returns an iterator over the recovered [`CeloTxEnvelope`] in this attributes with their
    /// original encoded bytes.
    ///
    /// This iterator will be empty if there are no transactions in the attributes.
    #[cfg(feature = "k256")]
    pub fn recovered_transactions_with_encoded(
        &self,
    ) -> impl Iterator<
        Item = Result<
            WithEncoded<alloy_consensus::transaction::Recovered<CeloTxEnvelope>>,
            alloy_consensus::crypto::RecoveryError,
        >,
    > + '_ {
        self.op_payload_attributes
            .transactions
            .iter()
            .flatten()
            .cloned()
            .zip(self.recovered_transactions())
            .map(|(tx_bytes, result)| result.map(|celo_tx| WithEncoded::new(tx_bytes, celo_tx)))
    }
}

#[cfg(all(test, feature = "serde"))]
mod test {
    use super::*;
    use alloc::vec;
    use alloy_primitives::{Address, B64, B256, b64};
    use alloy_rpc_types_engine::PayloadAttributes;
    use core::str::FromStr;

    #[test]
    fn test_serde_roundtrip_attributes_pre_holocene() {
        let attributes = CeloPayloadAttributes {
            op_payload_attributes: OpPayloadAttributes {
                payload_attributes: PayloadAttributes {
                    timestamp: 0x1337,
                    prev_randao: B256::ZERO,
                    suggested_fee_recipient: Address::ZERO,
                    withdrawals: Default::default(),
                    parent_beacon_block_root: Some(B256::ZERO),
                },
                transactions: Some(vec![b"hello".to_vec().into()]),
                no_tx_pool: Some(true),
                gas_limit: Some(42),
                eip_1559_params: None,
                min_base_fee: None,
            },
        };

        let ser = serde_json::to_string(&attributes).unwrap();
        let de: CeloPayloadAttributes = serde_json::from_str(&ser).unwrap();

        assert_eq!(attributes, de);
    }

    #[test]
    fn test_serde_roundtrip_attributes_post_holocene() {
        let attributes = CeloPayloadAttributes {
            op_payload_attributes: OpPayloadAttributes {
                payload_attributes: PayloadAttributes {
                    timestamp: 0x1337,
                    prev_randao: B256::ZERO,
                    suggested_fee_recipient: Address::ZERO,
                    withdrawals: Default::default(),
                    parent_beacon_block_root: Some(B256::ZERO),
                },
                transactions: Some(vec![b"hello".to_vec().into()]),
                no_tx_pool: Some(true),
                gas_limit: Some(42),
                eip_1559_params: Some(b64!("0000dead0000beef")),
                min_base_fee: None,
            },
        };

        let ser = serde_json::to_string(&attributes).unwrap();
        let de: CeloPayloadAttributes = serde_json::from_str(&ser).unwrap();

        assert_eq!(attributes, de);
    }

    #[test]
    fn test_get_extra_data_post_holocene() {
        let attributes = CeloPayloadAttributes {
            op_payload_attributes: OpPayloadAttributes {
                eip_1559_params: Some(B64::from_str("0x0000000800000008").unwrap()),
                ..Default::default()
            },
        };
        let extra_data = attributes.get_holocene_extra_data(BaseFeeParams::new(80, 60));
        assert_eq!(extra_data.unwrap(), Bytes::copy_from_slice(&[0, 0, 0, 0, 8, 0, 0, 0, 8]));
    }

    #[test]
    fn test_get_extra_data_post_holocene_default() {
        let attributes = CeloPayloadAttributes {
            op_payload_attributes: OpPayloadAttributes {
                eip_1559_params: Some(B64::ZERO),
                ..Default::default()
            },
        };
        let extra_data = attributes.get_holocene_extra_data(BaseFeeParams::new(80, 60));
        assert_eq!(extra_data.unwrap(), Bytes::copy_from_slice(&[0, 0, 0, 0, 80, 0, 0, 0, 60]));
    }
}
