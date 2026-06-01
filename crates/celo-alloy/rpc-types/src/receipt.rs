//! Receipt types for RPC

use alloy_consensus::{Receipt, ReceiptWithBloom};
use celo_alloy_consensus::{CeloCip64Receipt, CeloCip64ReceiptWithBloom, CeloReceiptEnvelope};
use op_alloy_consensus::{OpDepositReceipt, OpDepositReceiptWithBloom};
use op_alloy_rpc_types::L1BlockInfo;
use serde::{Deserialize, Serialize};

/// Celo Transaction Receipt type
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[doc(alias = "CeloTxReceipt")]
pub struct CeloTransactionReceipt {
    /// Regular eth transaction receipt including deposit receipts.
    ///
    /// For CIP-64 receipts the FC-denominated base fee is carried inside the inner
    /// `CeloReceiptEnvelope::Cip64`'s [`CeloCip64Receipt::base_fee`] and serialized
    /// as `"baseFee"` via that envelope's flattened JSON output. Do not duplicate it
    /// at this level — two flattened `"baseFee"` keys are an undefined-behavior wire
    /// shape (RFC 8259 §4) and different clients resolve them inconsistently.
    #[serde(flatten)]
    pub inner:
        alloy_rpc_types_eth::TransactionReceipt<CeloReceiptEnvelope<alloy_rpc_types_eth::Log>>,
    /// L1 block info of the transaction.
    #[serde(flatten)]
    pub l1_block_info: L1BlockInfo,
}

impl alloy_network_primitives::ReceiptResponse for CeloTransactionReceipt {
    fn contract_address(&self) -> Option<alloy_primitives::Address> {
        self.inner.contract_address
    }

    fn status(&self) -> bool {
        self.inner.inner.status()
    }

    fn block_hash(&self) -> Option<alloy_primitives::BlockHash> {
        self.inner.block_hash
    }

    fn block_number(&self) -> Option<u64> {
        self.inner.block_number
    }

    fn transaction_hash(&self) -> alloy_primitives::TxHash {
        self.inner.transaction_hash
    }

    fn transaction_index(&self) -> Option<u64> {
        self.inner.transaction_index()
    }

    fn gas_used(&self) -> u64 {
        self.inner.gas_used()
    }

    fn effective_gas_price(&self) -> u128 {
        self.inner.effective_gas_price()
    }

    fn blob_gas_used(&self) -> Option<u64> {
        self.inner.blob_gas_used()
    }

    fn blob_gas_price(&self) -> Option<u128> {
        self.inner.blob_gas_price()
    }

    fn from(&self) -> alloy_primitives::Address {
        self.inner.from()
    }

    fn to(&self) -> Option<alloy_primitives::Address> {
        self.inner.to()
    }

    fn cumulative_gas_used(&self) -> u64 {
        self.inner.cumulative_gas_used()
    }

    fn state_root(&self) -> Option<alloy_primitives::B256> {
        self.inner.state_root()
    }
}

impl From<CeloTransactionReceipt> for CeloReceiptEnvelope<alloy_primitives::Log> {
    fn from(value: CeloTransactionReceipt) -> Self {
        let inner_envelope = value.inner.inner;

        /// Helper function to convert the inner logs within a [ReceiptWithBloom] from RPC to
        /// consensus types.
        #[inline(always)]
        fn convert_standard_receipt(
            receipt: ReceiptWithBloom<Receipt<alloy_rpc_types_eth::Log>>,
        ) -> ReceiptWithBloom<Receipt<alloy_primitives::Log>> {
            let ReceiptWithBloom { logs_bloom, receipt } = receipt;

            let consensus_logs = receipt.logs.into_iter().map(|log| log.inner).collect();
            ReceiptWithBloom {
                receipt: Receipt {
                    status: receipt.status,
                    cumulative_gas_used: receipt.cumulative_gas_used,
                    logs: consensus_logs,
                },
                logs_bloom,
            }
        }

        match inner_envelope {
            CeloReceiptEnvelope::Legacy(receipt) => Self::Legacy(convert_standard_receipt(receipt)),
            CeloReceiptEnvelope::Eip2930(receipt) => {
                Self::Eip2930(convert_standard_receipt(receipt))
            }
            CeloReceiptEnvelope::Eip1559(receipt) => {
                Self::Eip1559(convert_standard_receipt(receipt))
            }
            CeloReceiptEnvelope::Eip7702(receipt) => {
                Self::Eip7702(convert_standard_receipt(receipt))
            }
            CeloReceiptEnvelope::Cip64(CeloCip64ReceiptWithBloom { logs_bloom, receipt }) => {
                let consensus_logs = receipt.inner.logs.into_iter().map(|log| log.inner).collect();
                let consensus_receipt = CeloCip64ReceiptWithBloom {
                    receipt: CeloCip64Receipt {
                        inner: Receipt {
                            status: receipt.inner.status,
                            cumulative_gas_used: receipt.inner.cumulative_gas_used,
                            logs: consensus_logs,
                        },
                        base_fee: receipt.base_fee,
                    },
                    logs_bloom,
                };
                Self::Cip64(consensus_receipt)
            }
            CeloReceiptEnvelope::Deposit(OpDepositReceiptWithBloom { logs_bloom, receipt }) => {
                let consensus_logs = receipt.inner.logs.into_iter().map(|log| log.inner).collect();
                let consensus_receipt = OpDepositReceiptWithBloom {
                    receipt: OpDepositReceipt {
                        inner: Receipt {
                            status: receipt.inner.status,
                            cumulative_gas_used: receipt.inner.cumulative_gas_used,
                            logs: consensus_logs,
                        },
                        deposit_nonce: receipt.deposit_nonce,
                        deposit_receipt_version: receipt.deposit_receipt_version,
                    },
                    logs_bloom,
                };
                Self::Deposit(consensus_receipt)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Eip658Value, Receipt};
    use alloy_primitives::{Bloom, address, b256};

    // Golden-byte JSON tests for receipt variants. Same rationale as the tx-side
    // tests: `serde_json::to_value` round-trips dedupe duplicate keys and miss
    // omitted-required-field regressions. The deposit-receipt golden directly locks
    // the wire shape that 0d5324d7's tx-side counterpart depends on
    // (`depositNonce` + `depositReceiptVersion`).
    //
    // Eip2930 and Eip7702 receipts are structurally identical to Eip1559 (same
    // inner `Receipt<Log>` shape), so we don't add separate goldens for them — the
    // Eip1559 fixture is the load-bearing case.

    fn sample_block_hash() -> alloy_primitives::B256 {
        b256!("0x9e6a0fb7e22159d943d760608cc36a0fb596d1ab3c997146f5b7c55c8c718c67")
    }
    fn sample_tx_hash() -> alloy_primitives::B256 {
        b256!("0xb7c74afdeb7c89fb9de2c312f49b38cb7a850ba36e064734c5223a477e83fdc9")
    }
    fn sample_from() -> alloy_primitives::Address {
        address!("0x7fda9576b9256c5bbe7cc487a0e49da7f038e2f3")
    }
    fn sample_to() -> alloy_primitives::Address {
        address!("0x00000000000000000000000000000000deadbeef")
    }

    fn empty_receipt() -> Receipt<alloy_rpc_types_eth::Log> {
        Receipt { status: Eip658Value::Eip658(true), cumulative_gas_used: 0xfa0d, logs: vec![] }
    }

    fn wrap_inner(inner: CeloReceiptEnvelope<alloy_rpc_types_eth::Log>) -> CeloTransactionReceipt {
        CeloTransactionReceipt {
            inner: alloy_rpc_types_eth::TransactionReceipt {
                inner,
                transaction_hash: sample_tx_hash(),
                transaction_index: Some(0),
                block_hash: Some(sample_block_hash()),
                block_number: Some(0x6cfef89),
                gas_used: 0xfa0d,
                effective_gas_price: 0,
                blob_gas_used: None,
                blob_gas_price: None,
                from: sample_from(),
                to: Some(sample_to()),
                contract_address: None,
            },
            l1_block_info: L1BlockInfo::default(),
        }
    }

    fn fx_receipt_legacy() -> CeloTransactionReceipt {
        wrap_inner(CeloReceiptEnvelope::Legacy(ReceiptWithBloom {
            receipt: empty_receipt(),
            logs_bloom: Bloom::default(),
        }))
    }

    fn fx_receipt_eip1559() -> CeloTransactionReceipt {
        wrap_inner(CeloReceiptEnvelope::Eip1559(ReceiptWithBloom {
            receipt: empty_receipt(),
            logs_bloom: Bloom::default(),
        }))
    }

    fn fx_receipt_cip64() -> CeloTransactionReceipt {
        wrap_inner(CeloReceiptEnvelope::Cip64(CeloCip64ReceiptWithBloom {
            receipt: CeloCip64Receipt { inner: empty_receipt(), base_fee: Some(0x22a4c71a0) },
            logs_bloom: Bloom::default(),
        }))
    }

    const DEPOSIT_RECEIPT_POST_CANYON_INPUT: &str = r#"{
        "blockHash": "0x9e6a0fb7e22159d943d760608cc36a0fb596d1ab3c997146f5b7c55c8c718c67",
        "blockNumber": "0x6cfef89",
        "contractAddress": null,
        "cumulativeGasUsed": "0xfa0d",
        "depositNonce": "0x8a2d11",
        "effectiveGasPrice": "0x0",
        "from": "0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001",
        "gasUsed": "0xfa0d",
        "logs": [],
        "logsBloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        "status": "0x1",
        "to": "0x4200000000000000000000000000000000000015",
        "transactionHash": "0xb7c74afdeb7c89fb9de2c312f49b38cb7a850ba36e064734c5223a477e83fdc9",
        "transactionIndex": "0x0",
        "type": "0x7e",
        "l1GasPrice": "0x3ef12787",
        "l1GasUsed": "0x1177",
        "l1Fee": "0x5bf1ab43d",
        "l1BaseFeeScalar": "0x1",
        "l1BlobBaseFee": "0x600ab8f05e64",
        "l1BlobBaseFeeScalar": "0x1"
    }"#;

    fn fx_receipt_deposit_post_canyon() -> CeloTransactionReceipt {
        serde_json::from_str(DEPOSIT_RECEIPT_POST_CANYON_INPUT).unwrap()
    }

    fn fx_receipt_deposit_pre_canyon() -> CeloTransactionReceipt {
        wrap_inner(CeloReceiptEnvelope::Deposit(OpDepositReceiptWithBloom {
            receipt: OpDepositReceipt {
                inner: empty_receipt(),
                deposit_nonce: Some(0x8a2d11),
                deposit_receipt_version: None,
            },
            logs_bloom: Bloom::default(),
        }))
    }

    #[test]
    fn golden_legacy_receipt() {
        let expected = r#"{"type":"0x0","status":"0x1","cumulativeGasUsed":"0xfa0d","logs":[],"logsBloom":"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000","transactionHash":"0xb7c74afdeb7c89fb9de2c312f49b38cb7a850ba36e064734c5223a477e83fdc9","transactionIndex":"0x0","blockHash":"0x9e6a0fb7e22159d943d760608cc36a0fb596d1ab3c997146f5b7c55c8c718c67","blockNumber":"0x6cfef89","gasUsed":"0xfa0d","effectiveGasPrice":"0x0","from":"0x7fda9576b9256c5bbe7cc487a0e49da7f038e2f3","to":"0x00000000000000000000000000000000deadbeef","contractAddress":null}"#;
        assert_eq!(serde_json::to_string(&fx_receipt_legacy()).unwrap(), expected);
    }

    #[test]
    fn golden_eip1559_receipt() {
        let expected = r#"{"type":"0x2","status":"0x1","cumulativeGasUsed":"0xfa0d","logs":[],"logsBloom":"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000","transactionHash":"0xb7c74afdeb7c89fb9de2c312f49b38cb7a850ba36e064734c5223a477e83fdc9","transactionIndex":"0x0","blockHash":"0x9e6a0fb7e22159d943d760608cc36a0fb596d1ab3c997146f5b7c55c8c718c67","blockNumber":"0x6cfef89","gasUsed":"0xfa0d","effectiveGasPrice":"0x0","from":"0x7fda9576b9256c5bbe7cc487a0e49da7f038e2f3","to":"0x00000000000000000000000000000000deadbeef","contractAddress":null}"#;
        assert_eq!(serde_json::to_string(&fx_receipt_eip1559()).unwrap(), expected);
    }

    #[test]
    fn golden_cip64_receipt() {
        let expected = r#"{"type":"0x7b","status":"0x1","cumulativeGasUsed":"0xfa0d","logs":[],"baseFee":"0x22a4c71a0","logsBloom":"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000","transactionHash":"0xb7c74afdeb7c89fb9de2c312f49b38cb7a850ba36e064734c5223a477e83fdc9","transactionIndex":"0x0","blockHash":"0x9e6a0fb7e22159d943d760608cc36a0fb596d1ab3c997146f5b7c55c8c718c67","blockNumber":"0x6cfef89","gasUsed":"0xfa0d","effectiveGasPrice":"0x0","from":"0x7fda9576b9256c5bbe7cc487a0e49da7f038e2f3","to":"0x00000000000000000000000000000000deadbeef","contractAddress":null}"#;
        assert_eq!(serde_json::to_string(&fx_receipt_cip64()).unwrap(), expected);
    }

    // Invariant lock: the wire output must carry `"baseFee"` exactly once. The previous
    // shape emitted it twice (inner CIP-64 envelope + outer wrapper field), which is
    // undefined per RFC 8259 §4 and parsed inconsistently across clients.
    #[test]
    fn cip64_receipt_to_string_emits_base_fee_once() {
        let s = serde_json::to_string(&fx_receipt_cip64()).unwrap();
        assert_eq!(
            s.matches("\"baseFee\"").count(),
            1,
            "baseFee must appear exactly once in the JSON wire output, got: {s}"
        );
    }

    // Regression lock for 0d5324d7's receipt-side counterpart: deposit RPC receipts
    // must carry `depositNonce` and `depositReceiptVersion`.
    #[test]
    fn golden_deposit_receipt_post_canyon() {
        let expected = r#"{"type":"0x7e","status":"0x1","cumulativeGasUsed":"0xfa0d","logs":[],"depositNonce":"0x8a2d11","logsBloom":"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000","transactionHash":"0xb7c74afdeb7c89fb9de2c312f49b38cb7a850ba36e064734c5223a477e83fdc9","transactionIndex":"0x0","blockHash":"0x9e6a0fb7e22159d943d760608cc36a0fb596d1ab3c997146f5b7c55c8c718c67","blockNumber":"0x6cfef89","gasUsed":"0xfa0d","effectiveGasPrice":"0x0","from":"0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001","to":"0x4200000000000000000000000000000000000015","contractAddress":null,"l1GasPrice":"0x3ef12787","l1GasUsed":"0x1177","l1Fee":"0x5bf1ab43d","l1BaseFeeScalar":"0x1","l1BlobBaseFee":"0x600ab8f05e64","l1BlobBaseFeeScalar":"0x1"}"#;
        assert_eq!(serde_json::to_string(&fx_receipt_deposit_post_canyon()).unwrap(), expected);
    }

    // Pre-canyon: `depositReceiptVersion` is None and must be omitted entirely.
    #[test]
    fn golden_deposit_receipt_pre_canyon() {
        let expected = r#"{"type":"0x7e","status":"0x1","cumulativeGasUsed":"0xfa0d","logs":[],"depositNonce":"0x8a2d11","logsBloom":"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000","transactionHash":"0xb7c74afdeb7c89fb9de2c312f49b38cb7a850ba36e064734c5223a477e83fdc9","transactionIndex":"0x0","blockHash":"0x9e6a0fb7e22159d943d760608cc36a0fb596d1ab3c997146f5b7c55c8c718c67","blockNumber":"0x6cfef89","gasUsed":"0xfa0d","effectiveGasPrice":"0x0","from":"0x7fda9576b9256c5bbe7cc487a0e49da7f038e2f3","to":"0x00000000000000000000000000000000deadbeef","contractAddress":null}"#;
        assert_eq!(serde_json::to_string(&fx_receipt_deposit_pre_canyon()).unwrap(), expected);
    }

    // Maintainer utility: regenerate golden receipt literals when struct field order
    // changes. Run with `cargo test -p celo-alloy-rpc-types _regenerate_receipt_goldens
    // -- --ignored --nocapture`.
    #[test]
    #[ignore = "generator: re-emit golden receipt literals when struct order changes"]
    fn _regenerate_receipt_goldens() {
        for (name, json) in [
            ("legacy", serde_json::to_string(&fx_receipt_legacy()).unwrap()),
            ("eip1559", serde_json::to_string(&fx_receipt_eip1559()).unwrap()),
            ("cip64", serde_json::to_string(&fx_receipt_cip64()).unwrap()),
            (
                "deposit_post_canyon",
                serde_json::to_string(&fx_receipt_deposit_post_canyon()).unwrap(),
            ),
            (
                "deposit_pre_canyon",
                serde_json::to_string(&fx_receipt_deposit_pre_canyon()).unwrap(),
            ),
        ] {
            eprintln!("GOLDEN[{name}]: {json}");
        }
        panic!("regenerator only — copy GOLDEN[...] lines into each golden_*_receipt test literal");
    }

    // <https://github.com/alloy-rs/op-alloy/issues/18>
    #[test]
    fn parse_rpc_receipt() {
        let s = r#"{
        "blockHash": "0x9e6a0fb7e22159d943d760608cc36a0fb596d1ab3c997146f5b7c55c8c718c67",
        "blockNumber": "0x6cfef89",
        "contractAddress": null,
        "cumulativeGasUsed": "0xfa0d",
        "depositNonce": "0x8a2d11",
        "effectiveGasPrice": "0x0",
        "from": "0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001",
        "gasUsed": "0xfa0d",
        "logs": [],
        "logsBloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        "status": "0x1",
        "to": "0x4200000000000000000000000000000000000015",
        "transactionHash": "0xb7c74afdeb7c89fb9de2c312f49b38cb7a850ba36e064734c5223a477e83fdc9",
        "transactionIndex": "0x0",
        "type": "0x7e",
        "l1GasPrice": "0x3ef12787",
        "l1GasUsed": "0x1177",
        "l1Fee": "0x5bf1ab43d",
        "l1BaseFeeScalar": "0x1",
        "l1BlobBaseFee": "0x600ab8f05e64",
        "l1BlobBaseFeeScalar": "0x1"
    }"#;

        let receipt: CeloTransactionReceipt = serde_json::from_str(s).unwrap();
        let value = serde_json::to_value(&receipt).unwrap();
        let expected_value = serde_json::from_str::<serde_json::Value>(s).unwrap();
        assert_eq!(value, expected_value);
    }
}
