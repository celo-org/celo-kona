//! This module contains the prologue phase of the client program, pulling in the boot information
//! through the `PreimageOracle` ABI as local keys.
//! TODO: When Celo network which we want to use for CI is included in [superchain-registry], we can
//! remove `CeloBootInfo`.

use alloy_primitives::{Address, B256, U256, address};
use celo_genesis::{CeloEspressoConfig, CeloRollupConfig};
use celo_registry::{CELO_FJORD_MAX_SEQUENCER_DRIFT, ROLLUP_CONFIGS, is_celo_chain};
use kona_genesis::RollupConfig;
use kona_preimage::{PreimageKey, PreimageOracleClient};
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
    /// Activation timestamp (L2) for Upgrade 18 (CGT v2); `None` = not scheduled.
    ///
    /// Sourced from the [`CeloRollupConfig`] like [`espresso`](Self::espresso). Dropping it at
    /// boot would make the proof execute the activation boundary block *without* the CGT v2
    /// irregular state transition the node applies — a guaranteed state-root mismatch on every
    /// proof from the boundary block on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade18_time: Option<u64>,
    /// Overrides `liquidityControllerOwner` in the Upgrade 18 activation artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade18_liquidity_controller_owner: Option<Address>,
    /// Overrides `celoTokenL1` in the Upgrade 18 activation artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade18_celo_token_l1: Option<Address>,
    /// Overrides `celoGasBridgeL1` in the Upgrade 18 activation artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade18_celo_gas_bridge_l1: Option<Address>,
    /// Overrides `nativeAssetLiquidityAmount` (the reserve seed, in wei) in the Upgrade 18
    /// activation artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade18_native_asset_liquidity_amount: Option<U256>,
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
        let mut celo_rollup_config: CeloRollupConfig = if let Some(config) =
            ROLLUP_CONFIGS.get(&chain_id)
        {
            config.clone()
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
        // Pin the Espresso settings for the known Celo chains to the program-baked values, so a
        // proof always derives with the Espresso params bound by its verification key rather than
        // any host-supplied value.
        //
        // TODO(upgrade18): when Upgrade 18 (CGT v2) is scheduled on a production chain, pin its
        // activation params for the known chain IDs the same way — in particular
        // `upgrade18_native_asset_liquidity_amount`, which mints the reserve seed backing all
        // bridged CELO, must be bound by the verification key rather than host-supplied.
        enforce_celo_espresso(&mut celo_rollup_config.espresso, chain_id);
        // Reject internally inconsistent Espresso settings up front, so a misconfiguration fails
        // fast here instead of corrupting derivation. This covers `espresso_time` set without a
        // `BatchAuthenticator` address (would stall at the fork boundary) and `espresso_time`
        // scheduled before `ecotone_time` (would route post-espresso blocks to the pre-ecotone
        // calldata source and silently bypass event-based authorization).
        celo_rollup_config.validate_espresso().map_err(|e| {
            OracleProviderError::Serde(<serde_json::Error as serde::de::Error>::custom(e))
        })?;
        // Exhaustive destructuring: a Celo-only field added to `CeloRollupConfig` in the future
        // must be threaded through this boot info too (or consciously dropped here) — the
        // compiler will point at this spot.
        let CeloRollupConfig {
            op_rollup_config: mut rollup_config,
            espresso,
            upgrade18_time,
            upgrade18_liquidity_controller_owner,
            upgrade18_celo_token_l1,
            upgrade18_celo_gas_bridge_l1,
            upgrade18_native_asset_liquidity_amount,
        } = celo_rollup_config;

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
        if is_celo_chain(chain_id) {
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
            upgrade18_time,
            upgrade18_liquidity_controller_owner,
            upgrade18_celo_token_l1,
            upgrade18_celo_gas_bridge_l1,
            upgrade18_native_asset_liquidity_amount,
        })
    }

    /// Reassembles the full [`CeloRollupConfig`]: the upstream OP config carried by
    /// [`BootInfo`] plus the Celo-only settings tracked alongside it here.
    ///
    /// This is what the executor must be configured with. Building a [`CeloRollupConfig`]
    /// from `op_boot_info.rollup_config` alone (`CeloRollupConfig::new`) silently zeroes
    /// `upgrade18_time` and the activation-artifact params, which skips the CGT v2
    /// transition in the proof while the node applies it.
    pub fn celo_rollup_config(&self) -> CeloRollupConfig {
        CeloRollupConfig {
            op_rollup_config: self.op_boot_info.rollup_config.clone(),
            espresso: self.espresso,
            upgrade18_time: self.upgrade18_time,
            upgrade18_liquidity_controller_owner: self.upgrade18_liquidity_controller_owner,
            upgrade18_celo_token_l1: self.upgrade18_celo_token_l1,
            upgrade18_celo_gas_bridge_l1: self.upgrade18_celo_gas_bridge_l1,
            upgrade18_native_asset_liquidity_amount: self.upgrade18_native_asset_liquidity_amount,
        }
    }
}

/// Celo Espresso batch-authentication settings for Celo Mainnet.
// TODO(espresso): set `espresso_time` + `batch_authenticator_address` when Espresso activation is
// scheduled for Celo Mainnet. `None` keeps vanilla OP Stack sender-based batch authorization.
const CELO_MAINNET_ESPRESSO: CeloEspressoConfig =
    CeloEspressoConfig { espresso_time: None, batch_authenticator_address: None };

/// Celo Espresso batch-authentication settings for Celo Sepolia.
// TODO(espresso): set `espresso_time` + `batch_authenticator_address` when Espresso activation is
// scheduled for Celo Sepolia. `None` keeps vanilla OP Stack sender-based batch authorization.
const CELO_SEPOLIA_ESPRESSO: CeloEspressoConfig =
    CeloEspressoConfig { espresso_time: None, batch_authenticator_address: None };

/// Celo Espresso batch-authentication settings for Celo Chaos.
///
/// Espresso is scheduled on Chaos at L2 timestamp `1782910800`; from that fork batches are
/// authorized by `BatchInfoAuthenticated` events emitted by the configured `BatchAuthenticator`
/// contract instead of by transaction sender.
const CELO_CHAOS_ESPRESSO: CeloEspressoConfig = CeloEspressoConfig {
    espresso_time: Some(1782910800),
    batch_authenticator_address: Some(address!("b4B5343d9635b05cA4FbdB09BB4929E21A1A8B37")),
};

/// Pin the Espresso batch-authentication settings for the known Celo chain IDs (Mainnet, Sepolia,
/// Chaos) to the program-baked values [`CELO_MAINNET_ESPRESSO`] / [`CELO_SEPOLIA_ESPRESSO`] /
/// [`CELO_CHAOS_ESPRESSO`].
///
/// Espresso settings are consensus-critical — `espresso_time` switches batch authorization from
/// sender-based to `BatchAuthenticator` event-based — so for the known chains they are baked into
/// the program and bound by the proof's verification key, rather than sourced from the
/// unauthenticated `L2_ROLLUP_CONFIG_KEY` preimage. This is the same known-chain set special-cased
/// by [`enforce_celo_fjord_sequencer_drift`] and the BPO override.
///
/// A no-op for any other chain ID, so the unknown-chain oracle fallback in [`CeloBootInfo::load`]
/// keeps the Espresso settings carried by its host-supplied config.
const fn enforce_celo_espresso(espresso: &mut CeloEspressoConfig, chain_id: u64) {
    match chain_id {
        42220 => *espresso = CELO_MAINNET_ESPRESSO,
        11142220 => *espresso = CELO_SEPOLIA_ESPRESSO,
        11162320 => *espresso = CELO_CHAOS_ESPRESSO,
        _ => {}
    }
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
    if is_celo_chain(chain_id) &&
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
    use alloy_primitives::Address;

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

    #[test]
    fn espresso_forced_for_celo_chain_ids() {
        // Mainnet/Sepolia/Chaos derive with the program-baked Espresso settings, ignoring any
        // input.
        for (chain_id, expected) in [
            (42220u64, CELO_MAINNET_ESPRESSO),
            (11142220u64, CELO_SEPOLIA_ESPRESSO),
            (11162320u64, CELO_CHAOS_ESPRESSO),
        ] {
            let mut espresso = CeloEspressoConfig {
                espresso_time: Some(999),
                batch_authenticator_address: Some(Address::repeat_byte(0xbb)),
            };
            enforce_celo_espresso(&mut espresso, chain_id);
            assert_eq!(espresso, expected, "chain {chain_id} must derive with the baked Espresso");
        }
    }

    /// `celo_rollup_config` must carry every Celo-only field into the executor's config —
    /// this is the seam where dropping one silently disables the corresponding consensus
    /// rule in the proof (the Upgrade 18 transition, most critically).
    #[test]
    fn celo_rollup_config_carries_all_celo_fields() {
        use alloy_primitives::U256;

        let espresso = CeloEspressoConfig {
            espresso_time: Some(7),
            batch_authenticator_address: Some(Address::repeat_byte(0xee)),
        };
        let boot = CeloBootInfo {
            op_boot_info: BootInfo {
                l1_head: B256::repeat_byte(1),
                agreed_l2_output_root: B256::repeat_byte(2),
                claimed_l2_output_root: B256::repeat_byte(3),
                claimed_l2_block_number: 4,
                chain_id: 1337,
                rollup_config: RollupConfig::default(),
                l1_config: Default::default(),
            },
            espresso,
            upgrade18_time: Some(4242),
            upgrade18_liquidity_controller_owner: Some(Address::repeat_byte(0xaa)),
            upgrade18_celo_token_l1: Some(Address::repeat_byte(0xbb)),
            upgrade18_celo_gas_bridge_l1: Some(Address::repeat_byte(0xcc)),
            upgrade18_native_asset_liquidity_amount: Some(U256::from(1_000_000u64)),
        };

        let cfg = boot.celo_rollup_config();
        assert_eq!(cfg.op_rollup_config, boot.op_boot_info.rollup_config);
        assert_eq!(cfg.espresso, espresso);
        assert_eq!(cfg.upgrade18_time, Some(4242));
        assert_eq!(cfg.upgrade18_liquidity_controller_owner, Some(Address::repeat_byte(0xaa)));
        assert_eq!(cfg.upgrade18_celo_token_l1, Some(Address::repeat_byte(0xbb)));
        assert_eq!(cfg.upgrade18_celo_gas_bridge_l1, Some(Address::repeat_byte(0xcc)));
        assert_eq!(cfg.upgrade18_native_asset_liquidity_amount, Some(U256::from(1_000_000u64)));
    }

    #[test]
    fn espresso_untouched_for_non_celo_chain() {
        // A non-Celo chain ID keeps whatever Espresso its config carried (the oracle fallback).
        let carried = CeloEspressoConfig {
            espresso_time: Some(42),
            batch_authenticator_address: Some(Address::repeat_byte(0xcc)),
        };
        let mut espresso = carried;
        enforce_celo_espresso(&mut espresso, 10);
        assert_eq!(espresso, carried);
    }
}
