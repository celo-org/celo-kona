//! This module contains the prologue phase of the client program, pulling in the boot information
//! through the `PreimageOracle` ABI as local keys.
//! TODO: When Celo network which we want to use for CI is included in [superchain-registry], we can
//! remove `CeloBootInfo`.

use alloy_primitives::B256;
use celo_registry::ROLLUP_CONFIGS;
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
        let rollup_config = if let Some(config) = ROLLUP_CONFIGS.get(&chain_id) {
            config.0.clone()
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

        // Override the L1 config to remove the BPOs and OSAKA timestamp and blob schedule for Celo,
        // until we're ready for Fusaka in op-geth.
        // This is done independently of the chain id, because updates are
        // required once Mainnet or Sepolia has further changes.
        // Also pay attention when bumping kona if there're any additions to
        // l1_config.blob_schedule. https://github.com/op-rs/kona/blob/kona-client/v1.1.6/crates/protocol/registry/src/l1/mod.rs#L63-L86
        l1_config.osaka_time = None;
        l1_config.bpo1_time = None;
        l1_config.bpo2_time = None;
        l1_config.bpo3_time = None;
        l1_config.bpo4_time = None;
        l1_config.bpo5_time = None;
        l1_config.blob_schedule.remove("osaka");
        l1_config.blob_schedule.remove("bpo1");
        l1_config.blob_schedule.remove("bpo2");

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
        })
    }
}
