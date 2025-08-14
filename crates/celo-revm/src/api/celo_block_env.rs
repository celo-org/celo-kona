use crate::common::fee_currency_context::FeeCurrencyContext;
use op_revm::L1BlockInfo;
use revm::primitives::Log;
use std::vec::Vec;

#[derive(Debug, Clone, Default)]
pub struct CeloBlockEnv {
    pub l1_block_info: L1BlockInfo,
    pub fee_currency_context: FeeCurrencyContext,
    /// Logs from system calls (debit/credit) that need to be merged into the final receipt
    pub cip64_actual_tx_system_call_logs_pre: Vec<Log>,
    pub cip64_actual_tx_system_call_logs_post: Vec<Log>,
}
