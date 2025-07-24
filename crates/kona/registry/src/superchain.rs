//! Contains the full superchain data.

use super::ChainList;
use alloy_primitives::map::HashMap;
use celo_genesis::CeloRollupConfig;
use kona_genesis::{ChainConfig, Superchains};

/// The registry containing all the superchain configurations.
#[derive(Debug, Clone, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Registry {
    /// The list of chains.
    pub chain_list: ChainList,
    /// Map of chain IDs to their chain configuration.
    pub op_chains: HashMap<u64, ChainConfig>,
    /// Map of chain IDs to their rollup configurations.
    pub rollup_configs: HashMap<u64, CeloRollupConfig>,
}

impl Registry {
    /// Read the chain list.
    pub fn read_chain_list() -> ChainList {
        let chain_list = include_str!("../etc/chainList.json");
        serde_json::from_str(chain_list).expect("Failed to read chain list")
    }

    /// Read superchain configs.
    pub fn read_superchain_configs() -> Superchains {
        let superchain_configs = include_str!("../etc/configs.json");
        serde_json::from_str(superchain_configs).expect("Failed to read superchain configs")
    }

    /// Initialize the superchain configurations from the chain list.
    pub fn from_chain_list() -> Self {
        let chain_list = Self::read_chain_list();
        let superchains = Self::read_superchain_configs();
        let mut op_chains = HashMap::default();
        let mut rollup_configs = HashMap::default();

        for superchain in superchains.superchains {
            for mut chain_config in superchain.chains {
                chain_config.l1_chain_id = superchain.config.l1.chain_id;
                if let Some(a) = &mut chain_config.addresses {
                    a.zero_proof_addresses();
                }
                let mut rollup = chain_config.as_rollup_config();
                rollup.protocol_versions_address = superchain
                    .config
                    .protocol_versions_addr
                    .expect("Missing protocol versions address");
                rollup.superchain_config_address = superchain.config.superchain_config_addr;
                // Wrap RollupConfig to CeloRollupConfig
                let celo_rollup = CeloRollupConfig { op_rollup_config: rollup };
                rollup_configs.insert(chain_config.chain_id, celo_rollup);
                op_chains.insert(chain_config.chain_id, chain_config);
            }
        }

        Self { chain_list, op_chains, rollup_configs }
    }
}
