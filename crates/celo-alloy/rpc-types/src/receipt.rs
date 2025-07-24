//! Receipt types for RPC

use alloy_consensus::{Receipt, ReceiptWithBloom};
use celo_alloy_consensus::CeloReceiptEnvelope;
use op_alloy_consensus::{OpDepositReceipt, OpDepositReceiptWithBloom};
use op_alloy_rpc_types::L1BlockInfo;
use serde::{Deserialize, Serialize};

/// Celo Transaction Receipt type
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[doc(alias = "CeloTxReceipt")]
pub struct CeloTransactionReceipt {
    /// Regular eth transaction receipt including deposit receipts
    #[serde(flatten)]
    pub inner:
        alloy_rpc_types_eth::TransactionReceipt<CeloReceiptEnvelope<alloy_rpc_types_eth::Log>>,
    /// L1 block info of the transaction.
    #[serde(flatten)]
    pub l1_block_info: L1BlockInfo,
    /// BaseFee stored in fee currency for fee currency txs.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub base_fee: Option<u128>,
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
            CeloReceiptEnvelope::Cip64(receipt) => Self::Cip64(convert_standard_receipt(receipt)),
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
