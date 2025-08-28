//! Abstraction over receipt building logic to allow plugging different primitive types into
//! [`alloy_op_evm::OpBlockExecutor`].

use alloy_consensus::Eip658Value;
use alloy_evm::{Evm, eth::receipt_builder::ReceiptBuilderCtx};
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;
use alloy_primitives::U256;
use celo_alloy_consensus::{CeloCip64Receipt, CeloReceiptEnvelope, CeloTxEnvelope, CeloTxType};
use celo_revm::common::{fee_currency_context::FeeCurrencyContext, Cip64Storage};
use core::fmt::Debug;
use op_alloy_consensus::OpDepositReceipt;

/// Receipt builder operating on celo-alloy types.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CeloAlloyReceiptBuilder {
    /// The fee currency context for calculating ERC20 base fees
    pub fee_currency_context: FeeCurrencyContext,
    /// Storage for CIP-64 transaction execution results
    pub cip64_storage: Cip64Storage,
}

impl CeloAlloyReceiptBuilder {
    /// Creates a new receipt builder with the given fee currency context and CIP-64 storage
    pub fn new(fee_currency_context: FeeCurrencyContext, cip64_storage: Cip64Storage) -> Self {
        Self { 
            fee_currency_context,
            cip64_storage,
        }
    }

    /// Creates a new receipt builder with the given fee currency context (legacy constructor)
    pub fn new_with_context(fee_currency_context: FeeCurrencyContext) -> Self {
        Self { 
            fee_currency_context,
            cip64_storage: Cip64Storage::new(),
        }
    }
}

impl Default for CeloAlloyReceiptBuilder {
    fn default() -> Self {
        Self {
            fee_currency_context: FeeCurrencyContext::default(),
            cip64_storage: Cip64Storage::new(),
        }
    }
}

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
                        // Use the fee_currency_context to convert the base fee
                        self.fee_currency_context
                            .celo_to_currency(cip64.tx().fee_currency, U256::from(base_fee))
                            .ok()
                            .and_then(|v| v.try_into().ok())
                    }
                } else {
                    None
                };

                // For CIP-64 transactions, we need to check if the transaction actually reverted
                // First check the success status before consuming the result
                let mut success = ctx.result.is_success();
                let logs = ctx.result.into_logs();

                // Get transaction identifier and check stored CIP-64 execution info
                use alloy_primitives::keccak256;
                let tx_identifier = if let CeloTxEnvelope::Cip64(cip64) = ctx.tx {
                    keccak256([cip64.recover_signer().ok().unwrap_or_default().as_slice(), &cip64.tx().nonce.to_be_bytes()].concat())
                } else {
                    // For non-CIP64 transactions, we don't need storage lookup
                    [0u8; 32].into()
                };
                let cip64_info = self.cip64_storage.get_cip64_info(&tx_identifier);
                if let Some(cip64_info) = cip64_info {
                    if cip64_info.reverted {
                        success = false;
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
