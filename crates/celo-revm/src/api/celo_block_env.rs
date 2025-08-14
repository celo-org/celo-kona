use crate::common::fee_currency_context::FeeCurrencyContext;
use op_revm::L1BlockInfo;
use revm::primitives::Log;
use std::vec::Vec;

#[derive(Debug, Clone, Default)]
pub struct Cip64Info {
    // Variable to accumulate the real intrinsic gas used for cip64 tx debit and credit evm calls
    // The protocol allows a 2x overshoot of the intrinsic gas cost
    pub actual_intrinsic_gas_used: u64,
    /// Logs from system calls (debit/credit) that need to be merged into the final receipt
    pub logs_pre: Vec<Log>,
    pub logs_post: Vec<Log>,
}

#[derive(Debug, Clone, Default)]
pub struct CeloBlockEnv {
    pub l1_block_info: L1BlockInfo,
    pub fee_currency_context: FeeCurrencyContext,
    pub cip64_tx_side_effects: Cip64Info,
}
