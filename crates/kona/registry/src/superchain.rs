//! Contains the full superchain data.

use super::ChainList;
use alloy_primitives::map::HashMap;

/// Chain ID of the Celo Chaos testnet.
pub const CELO_CHAOS_CHAIN_ID: u64 = 11162320;
/// Chain ID of Celo Sepolia.
pub const CELO_SEPOLIA_CHAIN_ID: u64 = 11142220;
/// Chain ID of Celo Mainnet.
pub const CELO_MAINNET_CHAIN_ID: u64 = 42220;

/// Returns `true` for the known Celo chain IDs (Mainnet, Sepolia, Chaos).
///
/// Kona-side single source of the "is this a Celo chain?" predicate; the proof
/// boot loader keys its Celo-specific derivation overrides (Fjord sequencer
/// drift, pre-Jovian BPO gating) off this set, so a new Celo network must be
/// added here to derive correctly via the preimage-oracle fallback.
pub const fn is_celo_chain(chain_id: u64) -> bool {
    matches!(
        chain_id,
        CELO_MAINNET_CHAIN_ID | CELO_SEPOLIA_CHAIN_ID | CELO_CHAOS_CHAIN_ID
    )
}

/// Fjord max sequencer drift for every Celo chain (Mainnet, Sepolia, Chaos).
///
/// op-node hardcodes `maxSequencerDriftCelo = 1800 + 1092 = 2892` for Cel2 chains, where
/// the OP default is 1800. This is the single source of truth for the value: the registry
/// stamps it onto every Celo rollup config here, and the proof boot loader re-applies it to
/// any Celo config that arrives without it (e.g. via the preimage-oracle fallback), so the
/// derivation pipeline never splits from op-node on batches with drift in (1800, 2892].
pub const CELO_FJORD_MAX_SEQUENCER_DRIFT: u64 = 2892;

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

                // Upstream `as_rollup_config()` defaults `fjord_max_sequencer_drift` to OP's
                // `FJORD_MAX_SEQUENCER_DRIFT` (1800), which doesn't match Celo. The historical
                // celo-org/kona fork patched the constant globally to 2892 (see
                // https://github.com/ethereum-optimism/optimism/pull/18859 for the
                // `rollup_config_override` feature), so every Celo chain — including Sepolia
                // and Chaos — was operating with an effective fjord drift of 2892 even though
                // the chainList JSON `max_sequencer_drift` for the testnets is 600. Honour the
                // historical 2892 here so the derivation pipeline accepts blocks produced
                // under the old node behaviour; otherwise post-Fjord Celo Sepolia blocks with
                // drift > 600 (observed: ~1300 s) are dropped as `SequencerDriftExceeded`.
                rollup.fjord_max_sequencer_drift = CELO_FJORD_MAX_SEQUENCER_DRIFT;

                // chain_config.as_rollup_config() copies da_challenge_address from
                // alt_da_config.da_challenge_address, but the node RPC rollup config does not.
                if is_celo_chain(rollup.l2_chain_id.id()) {
                    rollup.da_challenge_address = None;
                }
                // Wrap RollupConfig to CeloRollupConfig
                let celo_rollup = CeloRollupConfig::new(rollup);
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
    fn test_fjord_max_sequencer_drift_overrides_chain_list_value() {
        let registry = Registry::from_chain_list();

        // Every Celo chain runs with fjord_max_sequencer_drift = 2892 because the historical
        // kona fork patched the constant globally. The chainList JSON value for Sepolia/Chaos
        // (600) describes the pre-Fjord drift; post-Fjord, the chain ran with 2892.
        let mainnet = registry.rollup_configs.get(&CELO_MAINNET_CHAIN_ID).unwrap();
        assert_eq!(mainnet.max_sequencer_drift, 2892);
        assert_eq!(mainnet.fjord_max_sequencer_drift, 2892);

        let sepolia = registry.rollup_configs.get(&CELO_SEPOLIA_CHAIN_ID).unwrap();
        assert_eq!(sepolia.max_sequencer_drift, 600);
        assert_eq!(sepolia.fjord_max_sequencer_drift, 2892);

        let chaos = registry.rollup_configs.get(&CELO_CHAOS_CHAIN_ID).unwrap();
        assert_eq!(chaos.max_sequencer_drift, 600);
        assert_eq!(chaos.fjord_max_sequencer_drift, 2892);
    }
}
