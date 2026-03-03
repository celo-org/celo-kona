//! Contains the full superchain data.

use super::ChainList;
use alloy_primitives::{address, map::HashMap};

const CELO_CHAOS_CHAIN_ID: u64 = 11162320;
const CELO_SEPOLIA_CHAIN_ID: u64 = 11142220;
const CELO_MAINNET_CHAIN_ID: u64 = 42220;

// The pre-Granite channel timeout for Celo Mainnet, as configured in the node RPC rollup config.
// chain_config.as_rollup_config() defaults to 300, but Celo Mainnet uses 50.
const CELO_MAINNET_CHANNEL_TIMEOUT: u64 = 50;

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

                // Override rollup config for Chaos, Celo Sepolia, and Celo Mainnet to match
                // the rollup config returned by node RPC.
                if rollup.l2_chain_id == CELO_CHAOS_CHAIN_ID {
                    // protocol_versions_address inherits from the superchain config (Sepolia has
                    // a single value), but Chaos and Celo Sepolia have a different address.
                    rollup.protocol_versions_address =
                        address!("0xbca7e7eeddd0a7d5892ed7c4fa4a4cd4047bfdd7");
                } else if rollup.l2_chain_id == CELO_MAINNET_CHAIN_ID {
                    // chain_config.as_rollup_config() defaults channel_timeout to 300, but the
                    // node RPC rollup config for Celo Mainnet uses CELO_MAINNET_CHANNEL_TIMEOUT.
                    rollup.channel_timeout = CELO_MAINNET_CHANNEL_TIMEOUT;
                }

                // chain_config.as_rollup_config() copies da_challenge_address from
                // alt_da_config.da_challenge_address, but the node RPC rollup config does not.
                if rollup.l2_chain_id == CELO_CHAOS_CHAIN_ID
                    || rollup.l2_chain_id == CELO_SEPOLIA_CHAIN_ID
                    || rollup.l2_chain_id == CELO_MAINNET_CHAIN_ID
                {
                    rollup.da_challenge_address = None;
                }
                // Wrap RollupConfig to CeloRollupConfig
                let celo_rollup = CeloRollupConfig(rollup);
                rollup_configs.insert(chain_config.chain_id, celo_rollup);
                op_chains.insert(chain_config.chain_id, chain_config);
            }
        }

        Self { chain_list, op_chains, rollup_configs }
    }
}

#[cfg(test)]
mod tests {
    use crate::Registry;

    #[test]
    fn test_smoketest_init_from_chain_list() {
        Registry::from_chain_list();
    }
}
