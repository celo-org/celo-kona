use crate::common::fee_currency_context::FeeCurrencyContext;
use op_revm::L1BlockInfo;

#[derive(Debug, Clone, Default)]
pub struct CeloBlockEnv {
    pub l1_block_info: L1BlockInfo,
    pub fee_currency_context: FeeCurrencyContext,
}
