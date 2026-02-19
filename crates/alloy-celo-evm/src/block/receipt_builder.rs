//! Abstraction over receipt building logic to allow plugging different primitive types into
//! [`alloy_op_evm::OpBlockExecutor`].

use alloy_consensus::Eip658Value;
use alloy_evm::{Evm, eth::receipt_builder::ReceiptBuilderCtx};
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;
use alloy_primitives::U256;
use celo_alloy_consensus::{CeloCip64Receipt, CeloReceiptEnvelope, CeloTxEnvelope, CeloTxType};
use celo_revm::FeeCurrencyContext;
use core::fmt::Debug;
use op_alloy_consensus::OpDepositReceipt;

use crate::cip64_storage::Cip64Storage;
use revm::context_interface::Block;

/// Receipt builder operating on celo-alloy types.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct CeloAlloyReceiptBuilder {
    /// The fee currency context for calculating ERC20 base fees
    pub fee_currency_context: FeeCurrencyContext,
    /// Storage for CIP-64 transaction execution results
    pub cip64_storage: Cip64Storage,
}

impl CeloAlloyReceiptBuilder {
    /// Creates a new receipt builder with the given fee currency context and CIP-64 storage
    pub const fn new(
        fee_currency_context: FeeCurrencyContext,
        cip64_storage: Cip64Storage,
    ) -> Self {
        Self { fee_currency_context, cip64_storage }
    }

    /// Creates a new receipt builder with the given fee currency context (legacy constructor)
    pub fn new_with_context(fee_currency_context: FeeCurrencyContext) -> Self {
        Self { fee_currency_context, cip64_storage: Cip64Storage::default() }
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
                let base_fee = ctx.evm.block().basefee() as u128;

                // Retrieve the current CIP-64 context set during execution.
                // This contains the fee currency and execution info (logs, gas).
                let current_ctx = self.cip64_storage.take_current_context();

                // Calculate base fee in ERC20 using the fee currency from execution context
                let base_fee_in_erc20 = if let Some(ref ctx_data) = current_ctx {
                    match ctx_data.fee_currency {
                        None => {
                            // Paid with CELO
                            Some(base_fee)
                        }
                        Some(fc) if fc.is_zero() => {
                            // Zero address = paid with CELO
                            Some(base_fee)
                        }
                        Some(_) => {
                            // Use the fee_currency_context to convert the base fee
                            self.fee_currency_context
                                .celo_to_currency(ctx_data.fee_currency, U256::from(base_fee))
                                .ok()
                                .and_then(|v| v.try_into().ok())
                        }
                    }
                } else {
                    None
                };

                let success = ctx.result.is_success();
                let mut logs = ctx.result.into_logs();

                // Merge CIP-64 debit/credit logs from execution
                if let Some(ref ctx_data) = current_ctx {
                    if let Some(ref cip64_info) = ctx_data.cip64_info {
                        let mut merged_logs = cip64_info.logs_pre.clone();
                        merged_logs.extend(logs.clone());
                        merged_logs.extend(cip64_info.logs_post.clone());
                        logs = merged_logs;
                    }
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
