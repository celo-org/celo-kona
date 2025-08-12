use crate::common::fee_currency_context::FeeCurrencyContext;
use op_revm::L1BlockInfo;
use revm::primitives::Log;
use std::vec::Vec;

#[derive(Debug, Clone, Default)]
pub struct Cip64Info {
    pub actual_intrinsic_gas_used: u64,
    /// Logs from system calls (debit/credit) that need to be merged into the final receipt
    pub logs_pre: Vec<Log>,
    pub logs_post: Vec<Log>,
}

#[derive(Debug, Clone, Default)]
pub struct CeloBlockEnv {
    pub l1_block_info: L1BlockInfo,
    pub fee_currency_context: FeeCurrencyContext,
    pub actual_cip64_tx_info: Cip64Info,
}
