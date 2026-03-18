//! Celo receipt builder for reth, producing bloomless [`CeloReceipt`] types.

use crate::{primitives::CeloTransactionSigned, receipt::CeloReceipt};
use alloy_celo_evm::cip64_storage::Cip64Storage;
use alloy_consensus::Eip658Value;
use alloy_evm::eth::receipt_builder::ReceiptBuilderCtx;
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;
use celo_alloy_consensus::{CeloCip64Receipt, CeloTxType};
use op_alloy_consensus::OpDepositReceipt;
use reth_evm::Evm;

/// Receipt builder that produces bloomless [`CeloReceipt`] types for reth storage.
///
/// Analogous to [`OpRethReceiptBuilder`](reth_optimism_evm::OpRethReceiptBuilder) but with
/// CIP-64 fee currency support.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct CeloRethReceiptBuilder {
    /// Storage for CIP-64 transaction execution results.
    pub cip64_storage: Cip64Storage,
}

impl CeloRethReceiptBuilder {
    /// Creates a new receipt builder with the given CIP-64 storage.
    pub const fn new(cip64_storage: Cip64Storage) -> Self {
        Self { cip64_storage }
    }
}

impl OpReceiptBuilder for CeloRethReceiptBuilder {
    type Transaction = CeloTransactionSigned;
    type Receipt = CeloReceipt;

    fn build_receipt<'a, E: Evm>(
        &self,
        ctx: ReceiptBuilderCtx<'a, CeloTxType, E>,
    ) -> Result<Self::Receipt, ReceiptBuilderCtx<'a, CeloTxType, E>> {
        match ctx.tx_type {
            CeloTxType::Cip64 => {
                let success = ctx.result.is_success();
                let mut logs = ctx.result.into_logs();

                // Get CIP-64 receipt data from storage (stored during handler execution)
                let receipt_data = self.cip64_storage.pop_cip64_receipt_data();

                if let Some(data) = &receipt_data {
                    logs = Cip64Storage::merge_logs(&data.cip64_info, logs);
                }

                let base_fee_in_erc20 =
                    receipt_data.as_ref().and_then(|d| d.cip64_info.base_fee_in_erc20);

                Ok(CeloReceipt::Cip64(CeloCip64Receipt {
                    inner: alloy_consensus::Receipt {
                        status: Eip658Value::Eip658(success),
                        cumulative_gas_used: ctx.cumulative_gas_used,
                        logs,
                    },
                    base_fee: base_fee_in_erc20,
                }))
            }
            CeloTxType::Deposit => Err(ctx),
            ty => {
                let receipt = alloy_consensus::Receipt {
                    status: Eip658Value::Eip658(ctx.result.is_success()),
                    cumulative_gas_used: ctx.cumulative_gas_used,
                    logs: ctx.result.into_logs(),
                };

                Ok(match ty {
                    CeloTxType::Legacy => CeloReceipt::Legacy(receipt),
                    CeloTxType::Eip1559 => CeloReceipt::Eip1559(receipt),
                    CeloTxType::Eip2930 => CeloReceipt::Eip2930(receipt),
                    CeloTxType::Eip7702 => CeloReceipt::Eip7702(receipt),
                    _ => unreachable!(),
                })
            }
        }
    }

    fn build_deposit_receipt(&self, inner: OpDepositReceipt) -> Self::Receipt {
        CeloReceipt::Deposit(inner)
    }
}
