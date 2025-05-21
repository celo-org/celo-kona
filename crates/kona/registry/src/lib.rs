//! Temporary crate for Celo which is not included in [superchain-registry].
//! TODO: When Celo is included in [superchain-registry], we can remove `celo-registry` crate.
//!
//! [superchain-registry]: https://github.com/ethereum-optimism/superchain-registry/pull/1008/files
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

pub use alloy_primitives::map::{DefaultHashBuilder, HashMap};
pub use kona_genesis::{ChainConfig, RollupConfig};

pub use kona_registry::chain_list::{Chain, ChainList};

pub mod superchain;
pub use superchain::Registry;

lazy_static::lazy_static! {
    /// Private initializer that loads the superchain configurations.
    static ref _INIT: Registry = Registry::from_chain_list();

    /// Chain configurations exported from the registry
    pub static ref CHAINS: ChainList = _INIT.chain_list.clone();

    /// OP Chain configurations exported from the registry
    pub static ref OPCHAINS: HashMap<u64, ChainConfig, DefaultHashBuilder> = _INIT.op_chains.clone();

    /// Rollup configurations exported from the registry
    pub static ref ROLLUP_CONFIGS: HashMap<u64, RollupConfig, DefaultHashBuilder> = _INIT.rollup_configs.clone();
}

/// Returns a [RollupConfig] by its identifier.
pub fn rollup_config_by_ident(ident: &str) -> Option<&RollupConfig> {
    let chain_id = CHAINS.get_chain_by_ident(ident)?.chain_id;
    ROLLUP_CONFIGS.get(&chain_id)
}

/// Returns a [RollupConfig] by its identifier.
pub fn rollup_config_by_alloy_ident(chain: &alloy_chains::Chain) -> Option<&RollupConfig> {
    ROLLUP_CONFIGS.get(&chain.id())
}
