//! This module contains the prologue phase of the client program, pulling in the boot information
//! through the `PreimageOracle` ABI as local keys.
//! TODO: When Celo network which we want to use for CI is included in [superchain-registry], we can
//! remove `CeloBootInfo`.

use alloy_primitives::B256;
use celo_genesis::{CeloEspressoConfig, CeloRollupConfig};
use celo_registry::{CELO_FJORD_MAX_SEQUENCER_DRIFT, ROLLUP_CONFIGS};
use kona_genesis::RollupConfig;
use kona_preimage::{PreimageKey, PreimageOracleClient, errors::PreimageOracleError};
use kona_proof::{
    BootInfo,
    boot::{
        L1_CONFIG_KEY, L1_HEAD_KEY, L2_CHAIN_ID_KEY, L2_CLAIM_BLOCK_NUMBER_KEY, L2_CLAIM_KEY,
        L2_OUTPUT_ROOT_KEY, L2_ROLLUP_CONFIG_KEY,
    },
    errors::OracleProviderError,
};
use kona_registry::L1_CONFIGS;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// The boot information for the client program.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CeloBootInfo {
    /// The boot information for OP.
    pub op_boot_info: BootInfo,
    /// Celo-specific Espresso batch-authentication settings.
    ///
    /// Sourced from the [`CeloRollupConfig`] (registry entry or the rollup config deserialized
    /// from the preimage oracle). The OP [`BootInfo`] carries only the upstream `RollupConfig`,
    /// which has no place for these fields, so they are tracked alongside it here.
    #[serde(default)]
    pub espresso: CeloEspressoConfig,
}

impl CeloBootInfo {
    /// Load the boot information from the preimage oracle.
    ///
    /// ## Takes
    /// - `oracle`: The preimage oracle reader.
    ///
    /// ## Returns
    /// - `Ok(CeloBootInfo)`: The boot information.
    /// - `Err(_)`: Failed to load the boot information.
    pub async fn load<O>(oracle: &O) -> Result<Self, OracleProviderError>
    where
        O: PreimageOracleClient + Send,
    {
        let mut l1_head: B256 = B256::ZERO;
        oracle
            .get_exact(PreimageKey::new_local(L1_HEAD_KEY.to()), l1_head.as_mut())
            .await
            .map_err(OracleProviderError::Preimage)?;

        let mut l2_output_root: B256 = B256::ZERO;
        oracle
            .get_exact(PreimageKey::new_local(L2_OUTPUT_ROOT_KEY.to()), l2_output_root.as_mut())
            .await
            .map_err(OracleProviderError::Preimage)?;

        let mut l2_claim: B256 = B256::ZERO;
        oracle
            .get_exact(PreimageKey::new_local(L2_CLAIM_KEY.to()), l2_claim.as_mut())
            .await
            .map_err(OracleProviderError::Preimage)?;

        let l2_claim_block = u64::from_be_bytes(
            oracle
                .get(PreimageKey::new_local(L2_CLAIM_BLOCK_NUMBER_KEY.to()))
                .await
                .map_err(OracleProviderError::Preimage)?
                .as_slice()
                .try_into()
                .map_err(OracleProviderError::SliceConversion)?,
        );
        let chain_id = u64::from_be_bytes(
            oracle
                .get(PreimageKey::new_local(L2_CHAIN_ID_KEY.to()))
                .await
                .map_err(OracleProviderError::Preimage)?
                .as_slice()
                .try_into()
                .map_err(OracleProviderError::SliceConversion)?,
        );

        // Attempt to load the rollup config from the chain ID. If there is no config for the chain,
        // fall back to loading the config from the preimage oracle.
        let celo_rollup_config: CeloRollupConfig = if let Some(config) =
            ROLLUP_CONFIGS.get(&chain_id)
        {
            // The embedded registry entry is trusted, but it is built via
            // `CeloRollupConfig::new(..)`, which leaves the Celo Espresso settings
            // (`espresso_time` / `batch_authenticator_address`) at their disabled default. The
            // host serves those settings via the `L2_ROLLUP_CONFIG_KEY` preimage (see
            // `CeloSingleChainLocalInputs`), so without merging them in, a known-chain proof
            // would keep using the pre-fork sender-authorization path even after Espresso
            // activation. Take *only* the Espresso fields from the oracle and overlay them onto
            // the trusted registry config; every other field stays registry-authoritative.
            let mut config = config.clone();
            if let Some(oracle_espresso) = load_oracle_espresso_override(oracle, chain_id).await? {
                config.espresso = oracle_espresso;
            }
            config
        } else {
            warn!(
                target: "boot_loader",
                "No rollup config found for chain ID {}, falling back to preimage oracle. This is insecure in production without additional validation!",
                chain_id
            );
            let ser_cfg = oracle
                .get(PreimageKey::new_local(L2_ROLLUP_CONFIG_KEY.to()))
                .await
                .map_err(OracleProviderError::Preimage)?;
            serde_json::from_slice(&ser_cfg).map_err(OracleProviderError::Serde)?
        };
        // Reject internally inconsistent Espresso settings (e.g. `espresso_time` set without a
        // `BatchAuthenticator` address) up front, so a misconfiguration fails fast here instead of
        // silently stalling derivation at the fork boundary.
        celo_rollup_config.validate_espresso().map_err(|e| {
            OracleProviderError::Serde(<serde_json::Error as serde::de::Error>::custom(e))
        })?;
        let espresso = celo_rollup_config.espresso;
        let mut rollup_config = celo_rollup_config.op_rollup_config;

        // Celo chains run with a non-default Fjord max sequencer drift (2892). The embedded
        // registry stamps it onto every Celo rollup config, but a config arriving via the
        // oracle fallback above deserializes with serde's OP default (1800). Re-apply the known
        // value for the known Celo chain IDs so the proof never derives with a drift that
        // disagrees with op-node (which would split on batches with drift in (1800, 2892]).
        enforce_celo_fjord_sequencer_drift(&mut rollup_config, chain_id);

        // Attempt to load the L1 config from the L1 chain ID. If there is no config for the chain,
        // fall back to loading the config from the preimage oracle.
        let mut l1_config = if let Some(config) = L1_CONFIGS.get(&rollup_config.l1_chain_id) {
            config.clone()
        } else {
            warn!(
                target: "boot_loader",
                "No L1 config found for L1 chain ID {}, falling back to preimage oracle. This is insecure in production without additional validation!",
                rollup_config.l1_chain_id
            );
            let ser_cfg = oracle
                .get(PreimageKey::new_local(L1_CONFIG_KEY.to()))
                .await
                .map_err(OracleProviderError::Preimage)?;
            serde_json::from_slice(&ser_cfg).map_err(OracleProviderError::Serde)?
        };

        // BPO (Blob Parameter Only) hardforks introduce changes to blob gas pricing that require
        // corresponding support in op-node. For Celo chains, BPO must be disabled until the Jovian
        // hardfork is activated, because Celo's op-node did not support these L1 hardfork changes
        // early enough. Enabling BPO prematurely would cause the derivation pipeline to use
        // blob schedules and timestamps that op-node ignored at the time.
        // bpo3+ are intentionally omitted: Jovian is expected to activate on all Celo chains
        // before bpo3 is scheduled on any L1 network.
        if matches!(chain_id, 42220 | 11142220 | 11162320) {
            // Celo Mainnet, Celo Sepolia, and Celo Chaos
            let l2_claim_block_timestamp = rollup_config.genesis.l2_time +
                (l2_claim_block - rollup_config.genesis.l2.number) * rollup_config.block_time;
            if !rollup_config.is_jovian_active(l2_claim_block_timestamp) {
                l1_config.osaka_time = None;
                l1_config.bpo1_time = None;
                l1_config.bpo2_time = None;
                l1_config.blob_schedule.remove("osaka");
                l1_config.blob_schedule.remove("bpo1");
                l1_config.blob_schedule.remove("bpo2");
            }
        }

        Ok(Self {
            op_boot_info: BootInfo {
                l1_head,
                agreed_l2_output_root: l2_output_root,
                claimed_l2_output_root: l2_claim,
                claimed_l2_block_number: l2_claim_block,
                chain_id,
                rollup_config,
                l1_config,
            },
            espresso,
        })
    }
}

/// Read the host-supplied [`CeloEspressoConfig`] from the `L2_ROLLUP_CONFIG_KEY` preimage for a
/// known (registry) chain ID.
///
/// The embedded registry config is the source of truth for every rollup field *except* the Celo
/// Espresso settings, which the registry leaves disabled (see [`CeloRollupConfig::new`]). The host
/// carries `espresso_time` / `batch_authenticator_address` in the rollup-config preimage, so this
/// extracts just those fields for the caller to overlay onto the trusted registry config. The rest
/// of the deserialized oracle config is intentionally discarded — only the Espresso fields are
/// trusted from the oracle for a known chain.
///
/// Returns:
/// - `Ok(None)` when the host serves no rollup-config preimage (e.g. the native CLI booted with
///   `--l2-chain-id` and no `--rollup-config-path`). The registry's disabled Espresso default is
///   then kept.
/// - `Ok(Some(espresso))` with the host-supplied Espresso settings otherwise.
/// - `Err(_)` on any non-`KeyNotFound` preimage error or a malformed config payload.
async fn load_oracle_espresso_override<O>(
    oracle: &O,
    chain_id: u64,
) -> Result<Option<CeloEspressoConfig>, OracleProviderError>
where
    O: PreimageOracleClient + Send,
{
    let ser_cfg = match oracle.get(PreimageKey::new_local(L2_ROLLUP_CONFIG_KEY.to())).await {
        Ok(ser_cfg) => ser_cfg,
        // No rollup-config preimage was supplied for this known chain; keep the registry default.
        Err(PreimageOracleError::KeyNotFound) => return Ok(None),
        Err(e) => return Err(OracleProviderError::Preimage(e)),
    };

    let oracle_config: CeloRollupConfig =
        serde_json::from_slice(&ser_cfg).map_err(OracleProviderError::Serde)?;

    if oracle_config.espresso != CeloEspressoConfig::default() {
        warn!(
            target: "boot_loader",
            "Applying host-supplied Espresso settings to the registry rollup config for chain {}",
            chain_id,
        );
    }

    Ok(Some(oracle_config.espresso))
}

/// Re-apply the Celo Fjord max sequencer drift ([`CELO_FJORD_MAX_SEQUENCER_DRIFT`]) to a
/// rollup config for the known Celo chain IDs (Mainnet, Sepolia, Chaos).
///
/// The embedded registry already stamps this value, so for registry-sourced configs this is a
/// no-op. It exists for the preimage-oracle fallback in [`CeloBootInfo::load`], where the
/// host-supplied JSON deserializes `fjord_max_sequencer_drift` with serde's OP default (1800);
/// without this override a Celo proof would accept/reject batches differently from op-node for
/// drift in (1800, 2892], breaking node-vs-proof determinism.
fn enforce_celo_fjord_sequencer_drift(rollup_config: &mut RollupConfig, chain_id: u64) {
    // Celo Mainnet, Celo Sepolia, and Celo Chaos (same set special-cased for BPO above).
    if matches!(chain_id, 42220 | 11142220 | 11162320) &&
        rollup_config.fjord_max_sequencer_drift != CELO_FJORD_MAX_SEQUENCER_DRIFT
    {
        warn!(
            target: "boot_loader",
            "Overriding fjord_max_sequencer_drift {} -> {} for Celo chain {} (config did not carry the Celo value)",
            rollup_config.fjord_max_sequencer_drift, CELO_FJORD_MAX_SEQUENCER_DRIFT, chain_id,
        );
        rollup_config.fjord_max_sequencer_drift = CELO_FJORD_MAX_SEQUENCER_DRIFT;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fjord_drift_forced_for_celo_chain_ids() {
        for chain_id in [42220, 11142220, 11162320] {
            let mut cfg = RollupConfig { fjord_max_sequencer_drift: 1800, ..Default::default() };
            enforce_celo_fjord_sequencer_drift(&mut cfg, chain_id);
            assert_eq!(
                cfg.fjord_max_sequencer_drift, CELO_FJORD_MAX_SEQUENCER_DRIFT,
                "chain {chain_id} must derive with the Celo Fjord drift",
            );
        }
    }

    #[test]
    fn fjord_drift_untouched_for_non_celo_chain() {
        // A non-Celo chain ID (OP Mainnet) keeps whatever drift its config carried.
        let mut cfg = RollupConfig { fjord_max_sequencer_drift: 1800, ..Default::default() };
        enforce_celo_fjord_sequencer_drift(&mut cfg, 10);
        assert_eq!(cfg.fjord_max_sequencer_drift, 1800);
    }

    mod espresso_override {
        use super::*;
        use alloc::{boxed::Box, vec::Vec};
        use alloy_primitives::address;
        use async_trait::async_trait;
        use kona_preimage::errors::PreimageOracleResult;

        const CELO_MAINNET_CHAIN_ID: u64 = 42220;

        /// A minimal [`PreimageOracleClient`] that returns a single canned response for the
        /// `L2_ROLLUP_CONFIG_KEY` query. Any other key is rejected with `KeyNotFound`, which is
        /// all `load_oracle_espresso_override` ever requests.
        struct StubOracle {
            /// The response served for `L2_ROLLUP_CONFIG_KEY`: `Some(bytes)` to serve a preimage,
            /// or `None` to simulate the host serving no rollup config (`KeyNotFound`).
            rollup_config: Option<Vec<u8>>,
        }

        #[async_trait]
        impl PreimageOracleClient for StubOracle {
            async fn get(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
                let rollup_config_key = PreimageKey::new_local(L2_ROLLUP_CONFIG_KEY.to());
                if key == rollup_config_key {
                    self.rollup_config.clone().ok_or(PreimageOracleError::KeyNotFound)
                } else {
                    Err(PreimageOracleError::KeyNotFound)
                }
            }

            async fn get_exact(
                &self,
                _key: PreimageKey,
                _buf: &mut [u8],
            ) -> PreimageOracleResult<()> {
                Err(PreimageOracleError::KeyNotFound)
            }
        }

        fn espresso_config_json() -> Vec<u8> {
            let mut cfg = CeloRollupConfig::new(RollupConfig::default());
            cfg.espresso.espresso_time = Some(1234);
            cfg.espresso.batch_authenticator_address =
                Some(address!("00000000000000000000000000000000000000aa"));
            serde_json::to_vec(&cfg).expect("serialize espresso config")
        }

        #[tokio::test]
        async fn applies_oracle_espresso_for_known_chain() {
            let oracle = StubOracle { rollup_config: Some(espresso_config_json()) };
            let espresso = load_oracle_espresso_override(&oracle, CELO_MAINNET_CHAIN_ID)
                .await
                .expect("override must load")
                .expect("preimage present => Some");

            assert_eq!(espresso.espresso_time, Some(1234));
            assert_eq!(
                espresso.batch_authenticator_address,
                Some(address!("00000000000000000000000000000000000000aa"))
            );
        }

        #[tokio::test]
        async fn known_chain_without_espresso_keeps_default() {
            // Host serves a rollup config, but it carries no Espresso fields: the extracted
            // override is the disabled default, so overlaying it is a no-op.
            let plain = serde_json::to_vec(&CeloRollupConfig::new(RollupConfig::default()))
                .expect("serialize plain config");
            let oracle = StubOracle { rollup_config: Some(plain) };
            let espresso = load_oracle_espresso_override(&oracle, CELO_MAINNET_CHAIN_ID)
                .await
                .expect("override must load")
                .expect("preimage present => Some");

            assert_eq!(espresso, CeloEspressoConfig::default());
        }

        #[tokio::test]
        async fn missing_preimage_yields_no_override() {
            // Native CLI booted with `--l2-chain-id` and no rollup config: the host serves no
            // rollup-config preimage, so the registry default Espresso (disabled) must be kept.
            let oracle = StubOracle { rollup_config: None };
            let result = load_oracle_espresso_override(&oracle, CELO_MAINNET_CHAIN_ID)
                .await
                .expect("KeyNotFound must be swallowed, not propagated");
            assert_eq!(result, None);
        }
    }
}
