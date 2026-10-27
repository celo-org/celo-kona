//! Fixed CGTv2 network inputs and an isolated synthetic test profile.

use alloy_primitives::{Address, U256, address, uint};

/// Celo mainnet execution chain ID.
pub const CELO_MAINNET_CHAIN_ID: u64 = 42220;
/// Celo Sepolia execution chain ID.
pub const CELO_SEPOLIA_CHAIN_ID: u64 = 11142220;
/// Celo Chaos execution chain ID.
pub const CELO_CHAOS_CHAIN_ID: u64 = 11162320;
/// Synthetic profile matching the frozen contract/NUT fixtures.
pub const CGT_V2_FIXTURE_CHAIN_ID: u64 = 1337;

/// CeloTokenL1.TOTAL_MARKET_CAP: 1 billion CELO, with 18 decimals.
/// Source: celo-contracts/v6.0.0 at 0296412d673a666af6236efa1f615032b1f1c424.
pub const CGT_V2_TOTAL_SUPPLY: U256 = uint!(1_000_000_000_000_000_000_000_000_000_U256);

// TODO(CGTv2): replace for each network after its L1 bridge deployment.
// This temporary address is not a deployed production bridge.
const TEMPORARY_L1_BRIDGE: Address = address!("00000000000000000000000000000000c6700001");

/// Fixed migration parameters resolved only from the execution chain ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CgtV2NetworkParams {
    /// Fixed L1 bridge proxy that emits the authenticated migration event.
    pub celo_gas_bridge_l1: Address,
    total_supply: U256,
}

impl CgtV2NetworkParams {
    /// Checked reserve increment. The supply cannot be supplied by the signal or an operator.
    pub const fn reserve_seed(&self, migrated_amount: U256) -> Option<U256> {
        self.total_supply.checked_sub(migrated_amount)
    }
}

struct CgtV2NetworkEntry {
    chain_id: u64,
    params: CgtV2NetworkParams,
}

const NETWORKS: [CgtV2NetworkEntry; 3] = [
    CgtV2NetworkEntry {
        chain_id: CELO_MAINNET_CHAIN_ID,
        params: CgtV2NetworkParams {
            celo_gas_bridge_l1: TEMPORARY_L1_BRIDGE,
            total_supply: CGT_V2_TOTAL_SUPPLY,
        },
    },
    CgtV2NetworkEntry {
        chain_id: CELO_SEPOLIA_CHAIN_ID,
        params: CgtV2NetworkParams {
            celo_gas_bridge_l1: TEMPORARY_L1_BRIDGE,
            total_supply: CGT_V2_TOTAL_SUPPLY,
        },
    },
    CgtV2NetworkEntry {
        chain_id: CELO_CHAOS_CHAIN_ID,
        params: CgtV2NetworkParams {
            celo_gas_bridge_l1: TEMPORARY_L1_BRIDGE,
            total_supply: CGT_V2_TOTAL_SUPPLY,
        },
    },
];

// Fixture inputs must never become production defaults or select production bundles.
const FIXTURE_PARAMS: CgtV2NetworkParams = CgtV2NetworkParams {
    celo_gas_bridge_l1: Address::with_last_byte(0xcc),
    total_supply: uint!(1000_U256),
};

/// The network or its deployment-specific upgrade bundle has not been configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CgtV2ConfigError {
    /// The execution chain whose configuration is unavailable.
    pub chain_id: u64,
}
impl core::fmt::Display for CgtV2ConfigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "CGTv2 network or upgrade bundle is not configured for chain {}", self.chain_id)
    }
}
impl core::error::Error for CgtV2ConfigError {}

/// Resolve the fixed network table, with chain 1337 as a separate synthetic profile.
pub fn cgt_v2_params(chain_id: u64) -> Result<CgtV2NetworkParams, CgtV2ConfigError> {
    if chain_id == CGT_V2_FIXTURE_CHAIN_ID {
        return Ok(FIXTURE_PARAMS);
    }
    NETWORKS
        .iter()
        .find_map(|network| (network.chain_id == chain_id).then_some(network.params))
        .ok_or(CgtV2ConfigError { chain_id })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cgt_v2_reserve_checks_amount() {
        let p = cgt_v2_params(1337).unwrap();
        assert_eq!(p.reserve_seed(U256::from(600)), Some(U256::from(400)));
        assert_eq!(p.reserve_seed(U256::ZERO), Some(U256::from(1000)));
        assert_eq!(p.reserve_seed(U256::from(1000)), Some(U256::ZERO));
        assert_eq!(p.reserve_seed(U256::from(1001)), None);
    }
    #[test]
    fn cgt_v2_active_networks_use_fixed_supply() {
        let supply = U256::from(1_000_000_000_000_000_000_000_000_000_u128);
        for chain_id in [42220, 11142220, 11162320] {
            let params = cgt_v2_params(chain_id).unwrap();
            assert_ne!(params.celo_gas_bridge_l1, Address::ZERO);
            assert_ne!(params.celo_gas_bridge_l1, Address::with_last_byte(0xcc));
            assert_eq!(params.reserve_seed(U256::ZERO), Some(supply));
            assert_eq!(params.reserve_seed(supply), Some(U256::ZERO));
            assert_eq!(params.reserve_seed(supply + U256::from(1)), None);
        }
        for chain_id in [44787, 62320, 1] {
            assert_eq!(cgt_v2_params(chain_id), Err(CgtV2ConfigError { chain_id }));
        }
    }
}
