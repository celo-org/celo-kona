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
    use alloy_primitives::{Address, B64, B256, b64, hex};
    use alloy_rpc_types_engine::PayloadAttributes;
    use core::str::FromStr;

    /// Real signed CIP-64 transaction observed on Baklava (see
    /// `celo-alloy-consensus::transaction::cip64::recover_signer_cip64`). Used to
    /// exercise the iterator methods with bytes that round-trip through
    /// `CeloTxEnvelope::decode_2718`.
    const ENCODED_CIP64_TX: &[u8] = &hex!(
        "0x7bf8a882a4ec8207058304d7ee85026442dbed8303644c947a1e295c4babdf229776680c93ed0f73d069abc080a4cac35c7a290decd9548b62a8d60345a988386fc84ba6bc95484008f6362f93160ef3e563c0942f25deb3848c207fc8e0c34035b3ba7fc157602b80a0aa0cfaa3df893578b3504062b862428f0e4a94046370cf2a4fd6c392c0760dd8a01337d022bbb8faed78a9707e6c38d51f575816e7f85aa540f3d37a9081c58a71"
    );

    fn attributes_with_transactions(txs: Option<Vec<Bytes>>) -> CeloPayloadAttributes {
        CeloPayloadAttributes {
            op_payload_attributes: OpPayloadAttributes {
                payload_attributes: PayloadAttributes {
                    timestamp: 0,
                    prev_randao: B256::ZERO,
                    suggested_fee_recipient: Address::ZERO,
                    withdrawals: Default::default(),
                    parent_beacon_block_root: None,
                },
                transactions: txs,
                no_tx_pool: None,
                gas_limit: None,
                eip_1559_params: None,
                min_base_fee: None,
            },
        }
    }

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

    /// `decode_eip_1559_params` should round-trip through `op_payload_attributes`.
    /// The expected tuple is `(0x10, 0x0a)` — distinct from the constant-replacement
    /// mutants `(0,0)`, `(0,1)`, `(1,0)`, `(1,1)` and from `None`.
    #[test]
    fn decode_eip_1559_params_returns_op_layer_value() {
        let attributes = CeloPayloadAttributes {
            op_payload_attributes: OpPayloadAttributes {
                eip_1559_params: Some(b64!("000000100000000a")),
                ..Default::default()
            },
        };
        // Encoded form is `denominator | elasticity`; decoded tuple is
        // `(elasticity, denominator)` per op-alloy's contract.
        assert_eq!(attributes.decode_eip_1559_params(), Some((0x0a, 0x10)));
    }

    #[test]
    fn decode_eip_1559_params_returns_none_when_unset() {
        let attributes = CeloPayloadAttributes {
            op_payload_attributes: OpPayloadAttributes {
                eip_1559_params: None,
                ..Default::default()
            },
        };
        assert_eq!(attributes.decode_eip_1559_params(), None);
    }

    #[test]
    fn decoded_transactions_yields_one_for_one_encoded_input() {
        let attributes =
            attributes_with_transactions(Some(vec![Bytes::copy_from_slice(ENCODED_CIP64_TX)]));
        let decoded: Vec<_> = attributes.decoded_transactions().collect();
        assert_eq!(decoded.len(), 1);
        let tx = decoded.into_iter().next().unwrap().expect("decode should succeed");
        // Cip64 envelope variant carries the chain id from the encoded tx (0xa4ec).
        assert!(matches!(tx, CeloTxEnvelope::Cip64(_)));
    }

    #[test]
    fn decoded_transactions_is_empty_when_no_transactions() {
        let attributes = attributes_with_transactions(None);
        assert_eq!(attributes.decoded_transactions().count(), 0);

        let attributes = attributes_with_transactions(Some(vec![]));
        assert_eq!(attributes.decoded_transactions().count(), 0);
    }

    /// Pins the `if !buf.is_empty()` guard that rejects encoded transactions with
    /// trailing bytes. Without the `!`, trailing bytes would silently parse.
    #[test]
    fn decoded_transactions_rejects_trailing_bytes() {
        let mut padded = ENCODED_CIP64_TX.to_vec();
        padded.push(0xff);
        let attributes = attributes_with_transactions(Some(vec![padded.into()]));
        let decoded: Vec<_> = attributes.decoded_transactions().collect();
        assert_eq!(decoded.len(), 1);
        assert!(decoded.into_iter().next().unwrap().is_err());
    }

    #[test]
    fn decoded_transactions_with_encoded_pairs_bytes_with_decoded() {
        let bytes = Bytes::copy_from_slice(ENCODED_CIP64_TX);
        let attributes = attributes_with_transactions(Some(vec![bytes.clone()]));
        let entries: Vec<_> = attributes.decoded_transactions_with_encoded().collect();
        assert_eq!(entries.len(), 1);
        let with_encoded = entries.into_iter().next().unwrap().expect("decode should succeed");
        assert_eq!(with_encoded.encoded_bytes(), &bytes);
        assert!(matches!(with_encoded.into_value(), CeloTxEnvelope::Cip64(_)));
    }

    #[test]
    fn decoded_transactions_with_encoded_is_empty_when_no_transactions() {
        let attributes = attributes_with_transactions(None);
        assert_eq!(attributes.decoded_transactions_with_encoded().count(), 0);
    }

    #[cfg(feature = "k256")]
    #[test]
    fn recovered_transactions_returns_signer_for_signed_input() {
        use alloy_primitives::address;
        let attributes =
            attributes_with_transactions(Some(vec![Bytes::copy_from_slice(ENCODED_CIP64_TX)]));
        let recovered: Vec<_> = attributes.recovered_transactions().collect();
        assert_eq!(recovered.len(), 1);
        let recovered_tx = recovered.into_iter().next().unwrap().expect("recovery should succeed");
        assert_eq!(recovered_tx.signer(), address!("0xefe945ee33ce4ab037ff4d1e1384d0efcd95f37b"));
    }

    #[cfg(feature = "k256")]
    #[test]
    fn recovered_transactions_with_encoded_pairs_bytes_with_signer() {
        use alloy_primitives::address;
        let bytes = Bytes::copy_from_slice(ENCODED_CIP64_TX);
        let attributes = attributes_with_transactions(Some(vec![bytes.clone()]));
        let entries: Vec<_> = attributes.recovered_transactions_with_encoded().collect();
        assert_eq!(entries.len(), 1);
        let with_encoded = entries.into_iter().next().unwrap().expect("recovery should succeed");
        assert_eq!(with_encoded.encoded_bytes(), &bytes);
        assert_eq!(
            with_encoded.into_value().signer(),
            address!("0xefe945ee33ce4ab037ff4d1e1384d0efcd95f37b")
        );
    }
}
