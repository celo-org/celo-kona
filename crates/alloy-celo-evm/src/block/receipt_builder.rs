//! Abstraction over receipt building logic to allow plugging different primitive types into
//! [`alloy_op_evm::OpBlockExecutor`].

use alloy_consensus::Eip658Value;
use alloy_evm::{Evm, eth::receipt_builder::ReceiptBuilderCtx};
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;
use alloy_primitives::U256;
use celo_alloy_consensus::{CeloCip64Receipt, CeloReceiptEnvelope, CeloTxEnvelope, CeloTxType};
use core::fmt::Debug;
use op_alloy_consensus::OpDepositReceipt;

/// Receipt builder operating on celo-alloy types.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct CeloAlloyReceiptBuilder;

impl OpReceiptBuilder for CeloAlloyReceiptBuilder {
    type Transaction = CeloTxEnvelope;
    type Receipt = CeloReceiptEnvelope;

    fn build_receipt<'a, E: Evm>(
        &self,
        ctx: ReceiptBuilderCtx<'a, CeloTxEnvelope, E>,
    ) -> Result<Self::Receipt, ReceiptBuilderCtx<'a, CeloTxEnvelope, E>> {
        match ctx.tx.tx_type() {
            CeloTxType::Cip64 => {
                let base_fee = ctx.evm.block().basefee as u128;
                // For CIP-64 transactions, calculate the base fee in ERC20
                let base_fee_in_erc20 = if let CeloTxEnvelope::Cip64(cip64) = ctx.tx {
                    if cip64.tx().fee_currency.is_none() {
                        // Paid with Celo
                        Some(base_fee)
                    } else {
                        celo_revm::common::global_cip_64_context::get_fee_currency_context()
                            .and_then(|fee_currency_context| {
                                fee_currency_context
                                    .celo_to_currency(cip64.tx().fee_currency, U256::from(base_fee))
                                    .ok()
                                    .and_then(|v| v.try_into().ok())
                            })
                    }
                } else {
                    None
                };

                let receipt = CeloCip64Receipt {
                    inner: alloy_consensus::Receipt {
                        status: Eip658Value::Eip658(ctx.result.is_success()),
                        cumulative_gas_used: ctx.cumulative_gas_used,
                        logs: ctx.result.into_logs(),
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
