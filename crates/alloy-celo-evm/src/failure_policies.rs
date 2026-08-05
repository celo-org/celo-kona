//! Shared local failure policies used while constructing a next block with CIP-64 transactions.

use crate::{blocklist::FeeCurrencyBlocklist, revert_evictions::RevertEvictions};

/// Shared blocklist and revert-eviction state for local next-block construction.
///
/// Keeping both channels in one value prevents the EVM producer and payload-pool consumer from
/// being configured independently. Cloning this value preserves both shared channels.
#[derive(Debug, Clone, Default)]
pub struct CeloFailurePolicies {
    blocklist: FeeCurrencyBlocklist,
    revert_evictions: RevertEvictions,
}

impl CeloFailurePolicies {
    /// Creates a policy bundle from its two shared channels.
    pub const fn new(blocklist: FeeCurrencyBlocklist, revert_evictions: RevertEvictions) -> Self {
        Self { blocklist, revert_evictions }
    }

    /// Returns the shared fee currency blocklist.
    pub const fn blocklist(&self) -> &FeeCurrencyBlocklist {
        &self.blocklist
    }

    /// Returns the shared reverted-transaction eviction channel.
    pub const fn revert_evictions(&self) -> &RevertEvictions {
        &self.revert_evictions
    }
}

#[cfg(test)]
mod tests {
    use super::CeloFailurePolicies;
    use alloy_primitives::{Address, B256};

    #[test]
    fn clones_share_blocklist_and_revert_evictions() {
        let policies = CeloFailurePolicies::default();
        let clone = policies.clone();
        let fee_currency = Address::with_last_byte(1);
        let tx_hash = B256::with_last_byte(2);

        policies.blocklist().block_currency(fee_currency, 1_000);
        policies.revert_evictions().record(tx_hash);

        assert!(clone.blocklist().is_blocked(fee_currency));
        assert!(clone.revert_evictions().take(tx_hash));
    }
}
