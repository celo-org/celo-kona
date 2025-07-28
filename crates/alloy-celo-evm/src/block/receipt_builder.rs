//! Abstraction over receipt building logic to allow plugging different primitive types into
//! [`alloy_op_evm::OpBlockExecutor`].

use alloy_consensus::Eip658Value;
use alloy_evm::{Evm, eth::receipt_builder::ReceiptBuilderCtx};
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;
use celo_alloy_consensus::{CeloReceiptEnvelope, CeloTxEnvelope, CeloTxType, CeloCip64Receipt};
use core::fmt::Debug;
use op_alloy_consensus::OpDepositReceipt;

pub trait CeloReceiptBuilder: Debug {
    /// Receipt type.
    type Receipt;

    /// Builds receipt for a deposit transaction.
    fn build_cip64_receipt(&self, inner: CeloCip64Receipt) -> Self::Receipt;
}

/// Receipt builder operating on celo-alloy types.
#[derive(Debug, Default, Clone, Copy)]
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
            CeloTxType::Cip64 => Err(ctx),
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

impl CeloReceiptBuilder for CeloAlloyReceiptBuilder {
    type Receipt = CeloReceiptEnvelope;

    fn build_cip64_receipt(&self, inner: CeloCip64Receipt) -> Self::Receipt {
        CeloReceiptEnvelope::Cip64(inner.with_bloom())
    }
}