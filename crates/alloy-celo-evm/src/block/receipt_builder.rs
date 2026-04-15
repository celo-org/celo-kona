//! Abstraction over receipt building logic to allow plugging different primitive types into
//! [`alloy_op_evm::OpBlockExecutor`].

use alloy_consensus::Eip658Value;
use alloy_evm::{Evm, eth::receipt_builder::ReceiptBuilderCtx};
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;
use celo_alloy_consensus::{CeloCip64Receipt, CeloReceiptEnvelope, CeloTxEnvelope, CeloTxType};
use core::fmt::Debug;
use op_alloy_consensus::OpDepositReceipt;

use crate::cip64_storage::Cip64Storage;

/// Receipt builder operating on celo-alloy types.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct CeloAlloyReceiptBuilder {
    /// Storage for CIP-64 transaction execution results
    pub cip64_storage: Cip64Storage,
}

impl CeloAlloyReceiptBuilder {
    /// Creates a new receipt builder with the given CIP-64 storage
    pub const fn new(cip64_storage: Cip64Storage) -> Self {
        Self { cip64_storage }
    }
}

impl OpReceiptBuilder for CeloAlloyReceiptBuilder {
    type Transaction = CeloTxEnvelope;
    type Receipt = CeloReceiptEnvelope;

    fn build_receipt<'a, E: Evm>(
        &self,
        ctx: ReceiptBuilderCtx<'a, CeloTxType, E>,
    ) -> Result<Self::Receipt, ReceiptBuilderCtx<'a, CeloTxType, E>> {
        match ctx.tx_type {
            CeloTxType::Cip64 => {
                let success = ctx.result.is_success();
                let mut logs = ctx.result.into_logs();

                // Pop the CIP-64 receipt data stored during transact_raw
                let cip64_data = self.cip64_storage.pop_cip64_receipt_data();
                let base_fee_in_erc20 =
                    cip64_data.as_ref().and_then(|d| d.cip64_info.base_fee_in_erc20);

                // Merge CIP-64 pre/post logs if available
                if let Some(data) = &cip64_data {
                    logs = Cip64Storage::merge_logs(&data.cip64_info, logs);
                }

                let receipt = CeloCip64Receipt {
                    inner: alloy_consensus::Receipt {
                        status: Eip658Value::Eip658(success),
                        cumulative_gas_used: ctx.cumulative_gas_used,
                        logs,
                    },
                    base_fee: base_fee_in_erc20,
                };
                Ok(CeloReceiptEnvelope::Cip64(receipt.with_bloom()))
            }
            CeloTxType::Deposit => Err(ctx),
            ty => {
                let receipt = alloy_consensus::Receipt {
                    status: Eip658Value::Eip658(ctx.result.is_success()),
                    cumulative_gas_used: ctx.cumulative_gas_used,
                    logs: ctx.result.into_logs(),
                }
                .with_bloom();

                Ok(match ty {
                    CeloTxType::Legacy => CeloReceiptEnvelope::Legacy(receipt),
                    CeloTxType::Eip2930 => CeloReceiptEnvelope::Eip2930(receipt),
                    CeloTxType::Eip1559 => CeloReceiptEnvelope::Eip1559(receipt),
                    CeloTxType::Eip7702 => CeloReceiptEnvelope::Eip7702(receipt),
                    CeloTxType::Cip64 => unreachable!(),
                    CeloTxType::Deposit => unreachable!(),
                })
            }
        }
    }

    fn build_deposit_receipt(&self, inner: OpDepositReceipt) -> Self::Receipt {
        CeloReceiptEnvelope::Deposit(inner.with_bloom())
    }
}
