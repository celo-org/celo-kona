//! Rollup Config Types

use crate::CeloHardForkConfig;
use alloy_hardforks::{EthereumHardfork, EthereumHardforks, ForkCondition};
use alloy_op_hardforks::{OpHardfork, OpHardforks};
#[cfg(feature = "serde")]
use alloy_primitives::Address;
#[cfg(feature = "serde")]
use kona_genesis::{
    AltDAConfig, BaseFeeConfig, ChainGenesis, GRANITE_CHANNEL_TIMEOUT, HardForkConfig,
};
use kona_genesis::{
    FJORD_MAX_SEQUENCER_DRIFT, MAX_RLP_BYTES_PER_CHANNEL_BEDROCK, MAX_RLP_BYTES_PER_CHANNEL_FJORD,
    RollupConfig,
};

#[cfg(feature = "serde")]
const fn default_granite_channel_timeout() -> u64 {
    GRANITE_CHANNEL_TIMEOUT
}

#[cfg(feature = "serde")]
const fn default_interop_message_expiry_window() -> u64 {
    use kona_genesis::DEFAULT_INTEROP_MESSAGE_EXPIRY_WINDOW;

    DEFAULT_INTEROP_MESSAGE_EXPIRY_WINDOW
}

/// The Rollup configuration.
#[derive(Default, Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct CeloRollupConfig {
    /// Hardfork timestamps.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub hardforks: CeloHardForkConfig,
    /// The OP rollup config.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub op_rollup_config: RollupConfig,
}

#[cfg(feature = "arbitrary")]
impl<'a> arbitrary::Arbitrary<'a> for CeloRollupConfig {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(Self {
            op_rollup_config: RollupConfig::arbitrary(u)?,
            hardforks: CeloHardForkConfig::arbitrary(u)?,
        })
    }
}

#[cfg(feature = "revm")]
impl CeloRollupConfig {
    /// Returns the active [`op_revm::OpSpecId`] for the executor.
    ///
    /// ## Takes
    /// - `timestamp`: The timestamp of the executing block.
    ///
    /// ## Returns
    /// The active [`op_revm::OpSpecId`] for the executor.
    pub fn spec_id(&self, timestamp: u64) -> op_revm::OpSpecId {
        if self.is_interop_active(timestamp) {
            op_revm::OpSpecId::INTEROP
        } else if self.is_isthmus_active(timestamp) {
            op_revm::OpSpecId::ISTHMUS
        } else if self.is_holocene_active(timestamp) {
            op_revm::OpSpecId::HOLOCENE
        } else if self.is_fjord_active(timestamp) {
            op_revm::OpSpecId::FJORD
        } else if self.is_ecotone_active(timestamp) {
            op_revm::OpSpecId::ECOTONE
        } else if self.is_canyon_active(timestamp) {
            op_revm::OpSpecId::CANYON
        } else if self.is_regolith_active(timestamp) {
            op_revm::OpSpecId::REGOLITH
        } else {
            op_revm::OpSpecId::BEDROCK
        }
    }
}

impl CeloRollupConfig {
    /// Returns true if Regolith is active at the given timestamp.
    pub fn is_regolith_active(&self, timestamp: u64) -> bool {
        self.hardforks
            .op_hardfork_config
            .regolith_time
            .is_some_and(|t| timestamp >= t)
            || self.is_canyon_active(timestamp)
    }

    /// Returns true if the timestamp marks the first Regolith block.
    pub fn is_first_regolith_block(&self, timestamp: u64) -> bool {
        self.is_regolith_active(timestamp)
            && !self.is_regolith_active(timestamp.saturating_sub(self.op_rollup_config.block_time))
    }

    /// Returns true if Canyon is active at the given timestamp.
    pub fn is_canyon_active(&self, timestamp: u64) -> bool {
        self.hardforks
            .op_hardfork_config
            .canyon_time
            .is_some_and(|t| timestamp >= t)
            || self.is_delta_active(timestamp)
    }

    /// Returns true if the timestamp marks the first Canyon block.
    pub fn is_first_canyon_block(&self, timestamp: u64) -> bool {
        self.is_canyon_active(timestamp)
            && !self.is_canyon_active(timestamp.saturating_sub(self.op_rollup_config.block_time))
    }

    /// Returns true if Delta is active at the given timestamp.
    pub fn is_delta_active(&self, timestamp: u64) -> bool {
        self.hardforks
            .op_hardfork_config
            .delta_time
            .is_some_and(|t| timestamp >= t)
            || self.is_ecotone_active(timestamp)
    }

    /// Returns true if the timestamp marks the first Delta block.
    pub fn is_first_delta_block(&self, timestamp: u64) -> bool {
        self.is_delta_active(timestamp)
            && !self.is_delta_active(timestamp.saturating_sub(self.op_rollup_config.block_time))
    }

    /// Returns true if Ecotone is active at the given timestamp.
    pub fn is_ecotone_active(&self, timestamp: u64) -> bool {
        self.hardforks
            .op_hardfork_config
            .ecotone_time
            .is_some_and(|t| timestamp >= t)
            || self.is_fjord_active(timestamp)
    }

    /// Returns true if the timestamp marks the first Ecotone block.
    pub fn is_first_ecotone_block(&self, timestamp: u64) -> bool {
        self.is_ecotone_active(timestamp)
            && !self.is_ecotone_active(timestamp.saturating_sub(self.op_rollup_config.block_time))
    }

    /// Returns true if Fjord is active at the given timestamp.
    pub fn is_fjord_active(&self, timestamp: u64) -> bool {
        self.hardforks
            .op_hardfork_config
            .fjord_time
            .is_some_and(|t| timestamp >= t)
            || self.is_granite_active(timestamp)
    }

    /// Returns true if the timestamp marks the first Fjord block.
    pub fn is_first_fjord_block(&self, timestamp: u64) -> bool {
        self.is_fjord_active(timestamp)
            && !self.is_fjord_active(timestamp.saturating_sub(self.op_rollup_config.block_time))
    }

    /// Returns true if Granite is active at the given timestamp.
    pub fn is_granite_active(&self, timestamp: u64) -> bool {
        self.hardforks
            .op_hardfork_config
            .granite_time
            .is_some_and(|t| timestamp >= t)
            || self.is_holocene_active(timestamp)
    }

    /// Returns true if the timestamp marks the first Granite block.
    pub fn is_first_granite_block(&self, timestamp: u64) -> bool {
        self.is_granite_active(timestamp)
            && !self.is_granite_active(timestamp.saturating_sub(self.op_rollup_config.block_time))
    }

    /// Returns true if Holocene is active at the given timestamp.
    pub fn is_holocene_active(&self, timestamp: u64) -> bool {
        self.hardforks
            .op_hardfork_config
            .holocene_time
            .is_some_and(|t| timestamp >= t)
            || self.is_isthmus_active(timestamp)
    }

    /// Returns true if the timestamp marks the first Holocene block.
    pub fn is_first_holocene_block(&self, timestamp: u64) -> bool {
        self.is_holocene_active(timestamp)
            && !self.is_holocene_active(timestamp.saturating_sub(self.op_rollup_config.block_time))
    }

    /// Returns true if the pectra blob schedule is active at the given timestamp.
    pub fn is_pectra_blob_schedule_active(&self, timestamp: u64) -> bool {
        self.hardforks
            .op_hardfork_config
            .pectra_blob_schedule_time
            .is_some_and(|t| timestamp >= t)
    }

    /// Returns true if the timestamp marks the first pectra blob schedule block.
    pub fn is_first_pectra_blob_schedule_block(&self, timestamp: u64) -> bool {
        self.is_pectra_blob_schedule_active(timestamp)
            && !self.is_pectra_blob_schedule_active(
                timestamp.saturating_sub(self.op_rollup_config.block_time),
            )
    }

    /// Returns true if Isthmus is active at the given timestamp.
    pub fn is_isthmus_active(&self, timestamp: u64) -> bool {
        self.hardforks
            .op_hardfork_config
            .isthmus_time
            .is_some_and(|t| timestamp >= t)
            || self.is_interop_active(timestamp)
    }

    /// Returns true if the timestamp marks the first Isthmus block.
    pub fn is_first_isthmus_block(&self, timestamp: u64) -> bool {
        self.is_isthmus_active(timestamp)
            && !self.is_isthmus_active(timestamp.saturating_sub(self.op_rollup_config.block_time))
    }

    /// Returns true if Interop is active at the given timestamp.
    pub fn is_interop_active(&self, timestamp: u64) -> bool {
        self.hardforks
            .op_hardfork_config
            .interop_time
            .is_some_and(|t| timestamp >= t)
    }

    /// Returns true if the timestamp marks the first Interop block.
    pub fn is_first_interop_block(&self, timestamp: u64) -> bool {
        self.is_interop_active(timestamp)
            && !self.is_interop_active(timestamp.saturating_sub(self.op_rollup_config.block_time))
    }

    /// Returns true if a DA Challenge proxy Address is provided in the rollup config and the
    /// address is not zero.
    pub fn is_alt_da_enabled(&self) -> bool {
        self.op_rollup_config
            .da_challenge_address
            .is_some_and(|addr| !addr.is_zero())
    }

    /// Returns the max sequencer drift for the given timestamp.
    pub fn max_sequencer_drift(&self, timestamp: u64) -> u64 {
        if self.is_fjord_active(timestamp) {
            FJORD_MAX_SEQUENCER_DRIFT
        } else {
            self.op_rollup_config.max_sequencer_drift
        }
    }

    /// Returns the max rlp bytes per channel for the given timestamp.
    pub fn max_rlp_bytes_per_channel(&self, timestamp: u64) -> u64 {
        if self.is_fjord_active(timestamp) {
            MAX_RLP_BYTES_PER_CHANNEL_FJORD
        } else {
            MAX_RLP_BYTES_PER_CHANNEL_BEDROCK
        }
    }

    /// Returns the channel timeout for the given timestamp.
    pub fn channel_timeout(&self, timestamp: u64) -> u64 {
        if self.is_granite_active(timestamp) {
            self.op_rollup_config.granite_channel_timeout
        } else {
            self.op_rollup_config.channel_timeout
        }
    }

    /// Returns the [CeloHardForkConfig] using [CeloRollupConfig] timestamps.
    #[deprecated(since = "0.1.0", note = "Use the `hardforks` field instead.")]
    pub const fn hardfork_config(&self) -> CeloHardForkConfig {
        self.hardforks
    }

    /// Computes a block number from a timestamp, relative to the L2 genesis time and the block
    /// time.
    ///
    /// This function assumes that the timestamp is aligned with the block time, and uses floor
    /// division in its computation.
    pub const fn block_number_from_timestamp(&self, timestamp: u64) -> u64 {
        timestamp
            .saturating_sub(self.op_rollup_config.genesis.l2_time)
            .saturating_div(self.op_rollup_config.block_time)
    }

    /// Checks the scalar value in Ecotone.
    pub fn check_ecotone_l1_system_config_scalar(scalar: [u8; 32]) -> Result<(), &'static str> {
        let version_byte = scalar[0];
        match version_byte {
            0 => {
                if scalar[1..28] != [0; 27] {
                    return Err("Bedrock scalar padding not empty");
                }
                Ok(())
            }
            1 => {
                if scalar[1..24] != [0; 23] {
                    return Err("Invalid version 1 scalar padding");
                }
                Ok(())
            }
            _ => {
                // ignore the event if it's an unknown scalar format
                Err("Unrecognized scalar version")
            }
        }
    }
}

impl EthereumHardforks for CeloRollupConfig {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        if fork <= EthereumHardfork::Berlin {
            // We assume that OP chains were launched with all forks before Berlin activated.
            ForkCondition::Block(0)
        } else if fork <= EthereumHardfork::Paris {
            // Bedrock activates all hardforks up to Paris.
            self.op_fork_activation(OpHardfork::Bedrock)
        } else if fork <= EthereumHardfork::Shanghai {
            // Canyon activates Shanghai hardfork.
            self.op_fork_activation(OpHardfork::Canyon)
        } else if fork <= EthereumHardfork::Cancun {
            // Ecotone activates Cancun hardfork.
            self.op_fork_activation(OpHardfork::Ecotone)
        } else if fork <= EthereumHardfork::Prague {
            // Isthmus activates Prague hardfork.
            self.op_fork_activation(OpHardfork::Isthmus)
        } else {
            ForkCondition::Never
        }
    }
}

impl OpHardforks for CeloRollupConfig {
    fn op_fork_activation(&self, fork: OpHardfork) -> ForkCondition {
        match fork {
            OpHardfork::Bedrock => ForkCondition::Block(0),
            OpHardfork::Regolith => self
                .hardforks
                .op_hardfork_config
                .regolith_time
                .map(ForkCondition::Timestamp)
                .unwrap_or(self.op_fork_activation(OpHardfork::Canyon)),
            OpHardfork::Canyon => self
                .hardforks
                .op_hardfork_config
                .canyon_time
                .map(ForkCondition::Timestamp)
                .unwrap_or(self.op_fork_activation(OpHardfork::Ecotone)),
            OpHardfork::Ecotone => self
                .hardforks
                .op_hardfork_config
                .ecotone_time
                .map(ForkCondition::Timestamp)
                .unwrap_or(self.op_fork_activation(OpHardfork::Fjord)),
            OpHardfork::Fjord => self
                .hardforks
                .op_hardfork_config
                .fjord_time
                .map(ForkCondition::Timestamp)
                .unwrap_or(self.op_fork_activation(OpHardfork::Granite)),
            OpHardfork::Granite => self
                .hardforks
                .op_hardfork_config
                .granite_time
                .map(ForkCondition::Timestamp)
                .unwrap_or(self.op_fork_activation(OpHardfork::Holocene)),
            OpHardfork::Holocene => self
                .hardforks
                .op_hardfork_config
                .holocene_time
                .map(ForkCondition::Timestamp)
                .unwrap_or(self.op_fork_activation(OpHardfork::Isthmus)),
            OpHardfork::Isthmus => self
                .hardforks
                .op_hardfork_config
                .isthmus_time
                .map(ForkCondition::Timestamp)
                .unwrap_or(self.op_fork_activation(OpHardfork::Interop)),
            OpHardfork::Interop => self
                .hardforks
                .op_hardfork_config
                .interop_time
                .map(ForkCondition::Timestamp)
                .unwrap_or(ForkCondition::Never),
        }
    }
}

// TODO: find cleaner way
#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for CeloRollupConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Helper struct to deserialize all fields
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Helper {
            // RollupConfig fields
            genesis: ChainGenesis,
            block_time: u64,
            max_sequencer_drift: u64,
            seq_window_size: u64,
            channel_timeout: u64,
            #[serde(default = "default_granite_channel_timeout")]
            granite_channel_timeout: u64,
            l1_chain_id: u64,
            l2_chain_id: u64,
            batch_inbox_address: Address,
            deposit_contract_address: Address,
            l1_system_config_address: Address,
            protocol_versions_address: Address,
            #[serde(skip_serializing_if = "Option::is_none")]
            superchain_config_address: Option<Address>,
            #[serde(rename = "blobs_data", skip_serializing_if = "Option::is_none")]
            blobs_enabled_l1_timestamp: Option<u64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            da_challenge_address: Option<Address>,
            #[serde(default = "default_interop_message_expiry_window")]
            interop_message_expiry_window: u64,
            #[serde(rename = "alt_da")]
            alt_da_config: Option<AltDAConfig>,
            #[serde(default = "BaseFeeConfig::optimism")]
            chain_op_config: BaseFeeConfig,

            // HardForkConfig fields (flattened from both RollupConfig and CeloHardForkConfig)
            #[serde(default)]
            regolith_time: Option<u64>,
            #[serde(default)]
            canyon_time: Option<u64>,
            #[serde(default)]
            delta_time: Option<u64>,
            #[serde(default)]
            ecotone_time: Option<u64>,
            #[serde(default)]
            fjord_time: Option<u64>,
            #[serde(default)]
            granite_time: Option<u64>,
            #[serde(default)]
            holocene_time: Option<u64>,
            #[serde(default)]
            pectra_blob_schedule_time: Option<u64>,
            #[serde(default)]
            isthmus_time: Option<u64>,
            #[serde(default)]
            interop_time: Option<u64>,

            // CeloHardForkConfig specific field
            #[serde(default)]
            cel2_time: Option<u64>,
        }

        let helper = Helper::deserialize(deserializer)?;

        // Construct HardForkConfig
        let hardfork_config = HardForkConfig {
            regolith_time: helper.regolith_time,
            canyon_time: helper.canyon_time,
            delta_time: helper.delta_time,
            ecotone_time: helper.ecotone_time,
            fjord_time: helper.fjord_time,
            granite_time: helper.granite_time,
            holocene_time: helper.holocene_time,
            pectra_blob_schedule_time: helper.pectra_blob_schedule_time,
            isthmus_time: helper.isthmus_time,
            interop_time: helper.interop_time,
        };

        // Construct RollupConfig
        let op_rollup_config = RollupConfig {
            genesis: helper.genesis,
            block_time: helper.block_time,
            max_sequencer_drift: helper.max_sequencer_drift,
            seq_window_size: helper.seq_window_size,
            channel_timeout: helper.channel_timeout,
            granite_channel_timeout: helper.granite_channel_timeout,
            l1_chain_id: helper.l1_chain_id,
            l2_chain_id: helper.l2_chain_id,
            hardforks: hardfork_config,
            batch_inbox_address: helper.batch_inbox_address,
            deposit_contract_address: helper.deposit_contract_address,
            l1_system_config_address: helper.l1_system_config_address,
            protocol_versions_address: helper.protocol_versions_address,
            superchain_config_address: helper.superchain_config_address,
            blobs_enabled_l1_timestamp: helper.blobs_enabled_l1_timestamp,
            da_challenge_address: helper.da_challenge_address,
            interop_message_expiry_window: helper.interop_message_expiry_window,
            alt_da_config: helper.alt_da_config,
            chain_op_config: helper.chain_op_config,
        };

        // Construct CeloHardForkConfig
        let hardforks = CeloHardForkConfig {
            op_hardfork_config: hardfork_config,
            cel2_time: helper.cel2_time,
        };

        Ok(Self {
            hardforks,
            op_rollup_config,
        })
    }
}

#[cfg(test)]
#[cfg(feature = "serde")]
mod tests {
    use super::*;
    #[cfg(feature = "serde")]
    use alloy_eips::BlockNumHash;
    use alloy_primitives::{Address, address};
    #[cfg(feature = "serde")]
    use alloy_primitives::{U256, b256};
    use kona_genesis::{
        ChainGenesis, DEFAULT_INTEROP_MESSAGE_EXPIRY_WINDOW, GRANITE_CHANNEL_TIMEOUT,
        HardForkConfig,
    };

    #[test]
    #[cfg(feature = "arbitrary")]
    fn test_arbitrary_rollup_config() {
        use arbitrary::Arbitrary;
        use rand::Rng;
        let mut bytes = [0u8; 1024];
        rand::rng().fill(bytes.as_mut_slice());
        CeloRollupConfig::arbitrary(&mut arbitrary::Unstructured::new(&bytes)).unwrap();
    }

    #[test]
    #[cfg(feature = "revm")]
    fn test_revm_spec_id() {
        // By default, the spec ID should be BEDROCK.
        let mut config = CeloRollupConfig {
            hardforks: CeloHardForkConfig {
                op_hardfork_config: HardForkConfig {
                    regolith_time: Some(10),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(config.spec_id(0), op_revm::OpSpecId::BEDROCK);
        assert_eq!(config.spec_id(10), op_revm::OpSpecId::REGOLITH);
        config.hardforks.op_hardfork_config.canyon_time = Some(20);
        assert_eq!(config.spec_id(20), op_revm::OpSpecId::CANYON);
        config.hardforks.op_hardfork_config.ecotone_time = Some(30);
        assert_eq!(config.spec_id(30), op_revm::OpSpecId::ECOTONE);
        config.hardforks.op_hardfork_config.fjord_time = Some(40);
        assert_eq!(config.spec_id(40), op_revm::OpSpecId::FJORD);
        config.hardforks.op_hardfork_config.holocene_time = Some(50);
        assert_eq!(config.spec_id(50), op_revm::OpSpecId::HOLOCENE);
        config.hardforks.op_hardfork_config.isthmus_time = Some(60);
        assert_eq!(config.spec_id(60), op_revm::OpSpecId::ISTHMUS);
    }

    #[test]
    fn test_regolith_active() {
        let mut config = CeloRollupConfig::default();
        assert!(!config.is_regolith_active(0));
        config.hardforks.op_hardfork_config.regolith_time = Some(10);
        assert!(config.is_regolith_active(10));
        assert!(!config.is_regolith_active(9));
    }

    #[test]
    fn test_canyon_active() {
        let mut config = CeloRollupConfig::default();
        assert!(!config.is_canyon_active(0));
        config.hardforks.op_hardfork_config.canyon_time = Some(10);
        assert!(config.is_regolith_active(10));
        assert!(config.is_canyon_active(10));
        assert!(!config.is_canyon_active(9));
    }

    #[test]
    fn test_delta_active() {
        let mut config = CeloRollupConfig::default();
        assert!(!config.is_delta_active(0));
        config.hardforks.op_hardfork_config.delta_time = Some(10);
        assert!(config.is_regolith_active(10));
        assert!(config.is_canyon_active(10));
        assert!(config.is_delta_active(10));
        assert!(!config.is_delta_active(9));
    }

    #[test]
    fn test_ecotone_active() {
        let mut config = CeloRollupConfig::default();
        assert!(!config.is_ecotone_active(0));
        config.hardforks.op_hardfork_config.ecotone_time = Some(10);
        assert!(config.is_regolith_active(10));
        assert!(config.is_canyon_active(10));
        assert!(config.is_delta_active(10));
        assert!(config.is_ecotone_active(10));
        assert!(!config.is_ecotone_active(9));
    }

    #[test]
    fn test_fjord_active() {
        let mut config = CeloRollupConfig::default();
        assert!(!config.is_fjord_active(0));
        config.hardforks.op_hardfork_config.fjord_time = Some(10);
        assert!(config.is_regolith_active(10));
        assert!(config.is_canyon_active(10));
        assert!(config.is_delta_active(10));
        assert!(config.is_ecotone_active(10));
        assert!(config.is_fjord_active(10));
        assert!(!config.is_fjord_active(9));
    }

    #[test]
    fn test_granite_active() {
        let mut config = CeloRollupConfig::default();
        assert!(!config.is_granite_active(0));
        config.hardforks.op_hardfork_config.granite_time = Some(10);
        assert!(config.is_regolith_active(10));
        assert!(config.is_canyon_active(10));
        assert!(config.is_delta_active(10));
        assert!(config.is_ecotone_active(10));
        assert!(config.is_fjord_active(10));
        assert!(config.is_granite_active(10));
        assert!(!config.is_granite_active(9));
    }

    #[test]
    fn test_holocene_active() {
        let mut config = CeloRollupConfig::default();
        assert!(!config.is_holocene_active(0));
        config.hardforks.op_hardfork_config.holocene_time = Some(10);
        assert!(config.is_regolith_active(10));
        assert!(config.is_canyon_active(10));
        assert!(config.is_delta_active(10));
        assert!(config.is_ecotone_active(10));
        assert!(config.is_fjord_active(10));
        assert!(config.is_granite_active(10));
        assert!(config.is_holocene_active(10));
        assert!(!config.is_holocene_active(9));
    }

    #[test]
    fn test_pectra_blob_schedule_active() {
        let mut config = CeloRollupConfig::default();
        config
            .hardforks
            .op_hardfork_config
            .pectra_blob_schedule_time = Some(10);
        // Pectra blob schedule is a unique fork, not included in the hierarchical ordering. Its
        // activation does not imply the activation of any other forks.
        assert!(!config.is_regolith_active(10));
        assert!(!config.is_canyon_active(10));
        assert!(!config.is_delta_active(10));
        assert!(!config.is_ecotone_active(10));
        assert!(!config.is_fjord_active(10));
        assert!(!config.is_granite_active(10));
        assert!(!config.is_holocene_active(0));
        assert!(config.is_pectra_blob_schedule_active(10));
        assert!(!config.is_pectra_blob_schedule_active(9));
    }

    #[test]
    fn test_isthmus_active() {
        let mut config = CeloRollupConfig::default();
        assert!(!config.is_isthmus_active(0));
        config.hardforks.op_hardfork_config.isthmus_time = Some(10);
        assert!(config.is_regolith_active(10));
        assert!(config.is_canyon_active(10));
        assert!(config.is_delta_active(10));
        assert!(config.is_ecotone_active(10));
        assert!(config.is_fjord_active(10));
        assert!(config.is_granite_active(10));
        assert!(config.is_holocene_active(10));
        assert!(!config.is_pectra_blob_schedule_active(10));
        assert!(config.is_isthmus_active(10));
        assert!(!config.is_isthmus_active(9));
    }

    #[test]
    fn test_interop_active() {
        let mut config = CeloRollupConfig::default();
        assert!(!config.is_interop_active(0));
        config.hardforks.op_hardfork_config.interop_time = Some(10);
        assert!(config.is_regolith_active(10));
        assert!(config.is_canyon_active(10));
        assert!(config.is_delta_active(10));
        assert!(config.is_ecotone_active(10));
        assert!(config.is_fjord_active(10));
        assert!(config.is_granite_active(10));
        assert!(config.is_holocene_active(10));
        assert!(!config.is_pectra_blob_schedule_active(10));
        assert!(config.is_isthmus_active(10));
        assert!(config.is_interop_active(10));
        assert!(!config.is_interop_active(9));
    }

    #[test]
    fn test_is_first_fork_block() {
        let cfg = CeloRollupConfig {
            hardforks: CeloHardForkConfig {
                op_hardfork_config: HardForkConfig {
                    regolith_time: Some(10),
                    canyon_time: Some(20),
                    delta_time: Some(30),
                    ecotone_time: Some(40),
                    fjord_time: Some(50),
                    granite_time: Some(60),
                    holocene_time: Some(70),
                    pectra_blob_schedule_time: Some(80),
                    isthmus_time: Some(90),
                    interop_time: Some(100),
                },
                cel2_time: Some(110),
            },
            op_rollup_config: RollupConfig {
                block_time: 2,
                ..Default::default()
            },
        };

        // Regolith
        assert!(!cfg.is_first_regolith_block(8));
        assert!(cfg.is_first_regolith_block(10));
        assert!(!cfg.is_first_regolith_block(12));

        // Canyon
        assert!(!cfg.is_first_canyon_block(18));
        assert!(cfg.is_first_canyon_block(20));
        assert!(!cfg.is_first_canyon_block(22));

        // Delta
        assert!(!cfg.is_first_delta_block(28));
        assert!(cfg.is_first_delta_block(30));
        assert!(!cfg.is_first_delta_block(32));

        // Ecotone
        assert!(!cfg.is_first_ecotone_block(38));
        assert!(cfg.is_first_ecotone_block(40));
        assert!(!cfg.is_first_ecotone_block(42));

        // Fjord
        assert!(!cfg.is_first_fjord_block(48));
        assert!(cfg.is_first_fjord_block(50));
        assert!(!cfg.is_first_fjord_block(52));

        // Granite
        assert!(!cfg.is_first_granite_block(58));
        assert!(cfg.is_first_granite_block(60));
        assert!(!cfg.is_first_granite_block(62));

        // Holocene
        assert!(!cfg.is_first_holocene_block(68));
        assert!(cfg.is_first_holocene_block(70));
        assert!(!cfg.is_first_holocene_block(72));

        // Pectra blob schedule
        assert!(!cfg.is_first_pectra_blob_schedule_block(78));
        assert!(cfg.is_first_pectra_blob_schedule_block(80));
        assert!(!cfg.is_first_pectra_blob_schedule_block(82));

        // Isthmus
        assert!(!cfg.is_first_isthmus_block(88));
        assert!(cfg.is_first_isthmus_block(90));
        assert!(!cfg.is_first_isthmus_block(92));
    }

    #[test]
    fn test_alt_da_enabled() {
        let mut config = CeloRollupConfig::default();
        assert!(!config.is_alt_da_enabled());
        config.op_rollup_config.da_challenge_address = Some(Address::ZERO);
        assert!(!config.is_alt_da_enabled());
        config.op_rollup_config.da_challenge_address =
            Some(address!("0000000000000000000000000000000000000001"));
        assert!(config.is_alt_da_enabled());
    }

    #[test]
    fn test_granite_channel_timeout() {
        let mut config = CeloRollupConfig {
            op_rollup_config: RollupConfig {
                channel_timeout: 100,
                ..Default::default()
            },
            hardforks: CeloHardForkConfig {
                op_hardfork_config: HardForkConfig {
                    granite_time: Some(10),
                    ..Default::default()
                },
                ..Default::default()
            },
        };
        assert_eq!(config.channel_timeout(0), 100);
        assert_eq!(config.channel_timeout(10), GRANITE_CHANNEL_TIMEOUT);
        config.hardforks.op_hardfork_config.granite_time = None;
        assert_eq!(config.channel_timeout(10), 100);
    }

    #[test]
    fn test_max_sequencer_drift() {
        let mut config = CeloRollupConfig {
            op_rollup_config: RollupConfig {
                max_sequencer_drift: 100,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(config.max_sequencer_drift(0), 100);
        config.hardforks.op_hardfork_config.fjord_time = Some(10);
        assert_eq!(config.max_sequencer_drift(0), 100);
        assert_eq!(config.max_sequencer_drift(10), FJORD_MAX_SEQUENCER_DRIFT);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_deserialize_reference_rollup_config() {
        use kona_genesis::{ChainGenesis, OP_MAINNET_BASE_FEE_CONFIG, RollupConfig, SystemConfig};

        let raw: &str = r#"
        {
          "genesis": {
            "l1": {
              "hash": "0x481724ee99b1f4cb71d826e2ec5a37265f460e9b112315665c977f4050b0af54",
              "number": 10
            },
            "l2": {
              "hash": "0x88aedfbf7dea6bfa2c4ff315784ad1a7f145d8f650969359c003bbed68c87631",
              "number": 0
            },
            "l2_time": 1725557164,
            "system_config": {
              "batcherAddr": "0xc81f87a644b41e49b3221f41251f15c6cb00ce03",
              "overhead": "0x0000000000000000000000000000000000000000000000000000000000000000",
              "scalar": "0x00000000000000000000000000000000000000000000000000000000000f4240",
              "gasLimit": 30000000
            }
          },
          "block_time": 2,
          "max_sequencer_drift": 600,
          "seq_window_size": 3600,
          "channel_timeout": 300,
          "l1_chain_id": 3151908,
          "l2_chain_id": 1337,
          "regolith_time": 0,
          "canyon_time": 0,
          "delta_time": 0,
          "ecotone_time": 0,
          "fjord_time": 0,
          "cel2_time": 0,
          "batch_inbox_address": "0xff00000000000000000000000000000000042069",
          "deposit_contract_address": "0x08073dc48dde578137b8af042bcbc1c2491f1eb2",
          "l1_system_config_address": "0x94ee52a9d8edd72a85dea7fae3ba6d75e4bf1710",
          "protocol_versions_address": "0x0000000000000000000000000000000000000000",
          "chain_op_config": {
            "eip1559Elasticity": "0x6",
            "eip1559Denominator": "0x32",
            "eip1559DenominatorCanyon": "0xfa"
          },
          "alt_da": null
        }
        "#;

        let expected = CeloRollupConfig {
            op_rollup_config: RollupConfig {
                genesis: ChainGenesis {
                    l1: BlockNumHash {
                        hash: b256!(
                            "481724ee99b1f4cb71d826e2ec5a37265f460e9b112315665c977f4050b0af54"
                        ),
                        number: 10,
                    },
                    l2: BlockNumHash {
                        hash: b256!(
                            "88aedfbf7dea6bfa2c4ff315784ad1a7f145d8f650969359c003bbed68c87631"
                        ),
                        number: 0,
                    },
                    l2_time: 1725557164,
                    system_config: Some(SystemConfig {
                        batcher_address: address!("c81f87a644b41e49b3221f41251f15c6cb00ce03"),
                        overhead: U256::ZERO,
                        scalar: U256::from(0xf4240),
                        gas_limit: 30_000_000,
                        base_fee_scalar: None,
                        blob_base_fee_scalar: None,
                        eip1559_denominator: None,
                        eip1559_elasticity: None,
                        operator_fee_scalar: None,
                        operator_fee_constant: None,
                    }),
                },
                block_time: 2,
                max_sequencer_drift: 600,
                seq_window_size: 3600,
                channel_timeout: 300,
                granite_channel_timeout: GRANITE_CHANNEL_TIMEOUT,
                l1_chain_id: 3151908,
                l2_chain_id: 1337,
                hardforks: HardForkConfig {
                    regolith_time: Some(0),
                    canyon_time: Some(0),
                    delta_time: Some(0),
                    ecotone_time: Some(0),
                    fjord_time: Some(0),
                    ..Default::default()
                },
                batch_inbox_address: address!("ff00000000000000000000000000000000042069"),
                deposit_contract_address: address!("08073dc48dde578137b8af042bcbc1c2491f1eb2"),
                l1_system_config_address: address!("94ee52a9d8edd72a85dea7fae3ba6d75e4bf1710"),
                protocol_versions_address: Address::ZERO,
                superchain_config_address: None,
                blobs_enabled_l1_timestamp: None,
                da_challenge_address: None,
                interop_message_expiry_window: DEFAULT_INTEROP_MESSAGE_EXPIRY_WINDOW,
                chain_op_config: OP_MAINNET_BASE_FEE_CONFIG,
                alt_da_config: None,
            },
            hardforks: CeloHardForkConfig {
                op_hardfork_config: HardForkConfig {
                    regolith_time: Some(0),
                    canyon_time: Some(0),
                    delta_time: Some(0),
                    ecotone_time: Some(0),
                    fjord_time: Some(0),
                    ..Default::default()
                },
                cel2_time: Some(0),
            },
        };

        let deserialized: CeloRollupConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(deserialized, expected);
    }

    #[test]
    fn test_rollup_config_unknown_field() {
        let raw: &str = r#"
        {
          "genesis": {
            "l1": {
              "hash": "0x481724ee99b1f4cb71d826e2ec5a37265f460e9b112315665c977f4050b0af54",
              "number": 10
            },
            "l2": {
              "hash": "0x88aedfbf7dea6bfa2c4ff315784ad1a7f145d8f650969359c003bbed68c87631",
              "number": 0
            },
            "l2_time": 1725557164,
            "system_config": {
              "batcherAddr": "0xc81f87a644b41e49b3221f41251f15c6cb00ce03",
              "overhead": "0x0000000000000000000000000000000000000000000000000000000000000000",
              "scalar": "0x00000000000000000000000000000000000000000000000000000000000f4240",
              "gasLimit": 30000000
            }
          },
          "block_time": 2,
          "max_sequencer_drift": 600,
          "seq_window_size": 3600,
          "channel_timeout": 300,
          "l1_chain_id": 3151908,
          "l2_chain_id": 1337,
          "regolith_time": 0,
          "canyon_time": 0,
          "delta_time": 0,
          "ecotone_time": 0,
          "fjord_time": 0,
          "batch_inbox_address": "0xff00000000000000000000000000000000042069",
          "deposit_contract_address": "0x08073dc48dde578137b8af042bcbc1c2491f1eb2",
          "l1_system_config_address": "0x94ee52a9d8edd72a85dea7fae3ba6d75e4bf1710",
          "protocol_versions_address": "0x0000000000000000000000000000000000000000",
          "chain_op_config": {
            "eip1559_elasticity": 100,
            "eip1559_denominator": 100,
            "eip1559_denominator_canyon": 100
          },
          "unknown_field": "unknown"
        }
        "#;

        let err = serde_json::from_str::<CeloRollupConfig>(raw).unwrap_err();
        assert_eq!(err.classify(), serde_json::error::Category::Data);
    }

    #[test]
    fn test_compute_block_number_from_time() {
        let cfg = CeloRollupConfig {
            op_rollup_config: RollupConfig {
                genesis: ChainGenesis {
                    l2_time: 10,
                    ..Default::default()
                },
                block_time: 2,
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(cfg.block_number_from_timestamp(20), 5);
        assert_eq!(cfg.block_number_from_timestamp(30), 10);
    }
}
