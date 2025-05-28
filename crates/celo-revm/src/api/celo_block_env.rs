use alloy_primitives::{Address, U256, map::HashMap};
use op_revm::L1BlockInfo;

#[derive(Debug, Clone, Default)]
pub struct CeloBlockEnv {
    pub l1_block_info: L1BlockInfo,
    pub exchange_rates: HashMap<Address, (U256, U256)>,
    pub intrinsic_gas: HashMap<Address, U256>,
}
