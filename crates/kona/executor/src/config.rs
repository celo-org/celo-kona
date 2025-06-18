//! Celo-specific configuration structures.

use kona_genesis::RollupConfig;
use serde::{Deserialize, Serialize};

/// Celo-specific rollup configuration that extends the base [RollupConfig].
#[derive(Debug, Clone, Eq, PartialEq, Deserialize, Serialize)]
pub struct CeloRollupConfig {
    /// The base rollup configuration.
    #[serde(flatten)]
    pub rollup_config: RollupConfig,

    /// The minimum base fee for EIP-1559 transactions.
    pub eip1559_base_fee_floor: u64,
}

impl CeloRollupConfig {
    /// Create a new [CeloRollupConfig] with the given base fee floor.
    pub fn new(rollup_config: RollupConfig, eip1559_base_fee_floor: u64) -> Self {
        Self {
            rollup_config,
            eip1559_base_fee_floor,
        }
    }

    /// Get the EIP-1559 base fee floor.
    pub const fn base_fee_floor(&self) -> u64 {
        self.eip1559_base_fee_floor
    }
}

impl core::ops::Deref for CeloRollupConfig {
    type Target = RollupConfig;

    fn deref(&self) -> &Self::Target {
        &self.rollup_config
    }
}

impl From<RollupConfig> for CeloRollupConfig {
    fn from(rollup_config: RollupConfig) -> Self {
        Self {
            rollup_config,
            // Default value from the original magic number
            eip1559_base_fee_floor: 25_000_000_000,
        }
    }
}
