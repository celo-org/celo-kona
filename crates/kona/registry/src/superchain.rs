//! Contains the full superchain data.

use super::ChainList;
use alloy_primitives::map::HashMap;

const CELO_CHAOS_CHAIN_ID: u64 = 11162320;
const CELO_SEPOLIA_CHAIN_ID: u64 = 11142220;
const CELO_MAINNET_CHAIN_ID: u64 = 42220;

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
                rollup.superchain_config_address = superchain.config.superchain_config_addr;

                // Carry the per-chain pre-Fjord `max_sequencer_drift` over into the post-Fjord
                // value. Upstream `as_rollup_config()` defaults `fjord_max_sequencer_drift` to
                // OP's `FJORD_MAX_SEQUENCER_DRIFT` (1800), but Celo mainnet runs with 2892
                // (see https://github.com/ethereum-optimism/optimism/pull/18859 for the
                // `rollup_config_override` feature) and Sepolia/Chaos run with 600. Preserving
                // each chain's existing drift across the Fjord upgrade keeps node behavior
                // consistent with the chain-list JSON.
                rollup.fjord_max_sequencer_drift = rollup.max_sequencer_drift;

                // chain_config.as_rollup_config() copies da_challenge_address from
                // alt_da_config.da_challenge_address, but the node RPC rollup config does not.
                if rollup.l2_chain_id == CELO_CHAOS_CHAIN_ID ||
                    rollup.l2_chain_id == CELO_SEPOLIA_CHAIN_ID ||
                    rollup.l2_chain_id == CELO_MAINNET_CHAIN_ID
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
    use crate::{
        Registry,
        superchain::{CELO_CHAOS_CHAIN_ID, CELO_MAINNET_CHAIN_ID, CELO_SEPOLIA_CHAIN_ID},
    };

    #[test]
    fn test_smoketest_init_from_chain_list() {
        Registry::from_chain_list();
    }

    #[test]
    fn test_fjord_max_sequencer_drift_matches_per_chain_value() {
        let registry = Registry::from_chain_list();

        let mainnet = registry.rollup_configs.get(&CELO_MAINNET_CHAIN_ID).unwrap();
        assert_eq!(mainnet.0.max_sequencer_drift, 2892);
        assert_eq!(mainnet.0.fjord_max_sequencer_drift, 2892);

        let sepolia = registry.rollup_configs.get(&CELO_SEPOLIA_CHAIN_ID).unwrap();
        assert_eq!(sepolia.0.max_sequencer_drift, 600);
        assert_eq!(sepolia.0.fjord_max_sequencer_drift, 600);

        let chaos = registry.rollup_configs.get(&CELO_CHAOS_CHAIN_ID).unwrap();
        assert_eq!(chaos.0.max_sequencer_drift, 600);
        assert_eq!(chaos.0.fjord_max_sequencer_drift, 600);
    }
}
