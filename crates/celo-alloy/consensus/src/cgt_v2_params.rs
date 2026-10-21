//! Fixed execution parameters. Production deployment values are not finalized yet.

use alloy_primitives::{Address, U256};

/// Fixed migration parameters resolved only from the execution chain ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CgtV2NetworkParams {
    /// Existing governance owner for the installed liquidity controller.
    pub liquidity_controller_owner: Address,
    /// CELO ERC-20 on L1.
    pub celo_token_l1: Address,
    /// Fixed L1 bridge proxy used as the L2 bridge counterpart.
    pub celo_gas_bridge_l1: Address,
    total_supply: U256,
}

impl CgtV2NetworkParams {
    /// Checked reserve increment. The supply cannot be supplied by the signal or an operator.
    pub const fn reserve_seed(&self, migrated_amount: U256) -> Option<U256> {
        self.total_supply.checked_sub(migrated_amount)
    }
}

/// Production values must be supplied before this network can activate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CgtV2ConfigError {
    /// The execution chain whose protocol parameters are unresolved.
    pub chain_id: u64,
}
impl core::fmt::Display for CgtV2ConfigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "CGTv2 network addresses and total supply are unresolved for chain {}",
            self.chain_id
        )
    }
}
impl core::error::Error for CgtV2ConfigError {}

/// Resolve fixed compiled rules. Chain 1337 is an isolated synthetic devnet profile.
/// Mainnet/testnet deployment addresses and S intentionally remain unresolved.
pub fn cgt_v2_params(chain_id: u64) -> Result<CgtV2NetworkParams, CgtV2ConfigError> {
    if chain_id != 1337 {
        return Err(CgtV2ConfigError { chain_id });
    }
    Ok(CgtV2NetworkParams {
        liquidity_controller_owner: Address::with_last_byte(0xaa),
        celo_token_l1: Address::with_last_byte(0xbb),
        celo_gas_bridge_l1: Address::with_last_byte(0xcc),
        total_supply: U256::from(1000),
    })
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
    fn cgt_v2_production_params_must_be_resolved() {
        for chain_id in [42220, 11142220, 11162320, 1] {
            assert_eq!(cgt_v2_params(chain_id), Err(CgtV2ConfigError { chain_id }));
        }
    }
}
