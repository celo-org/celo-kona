//! Shared local failure policies used while sequencing CIP-64 transactions from the pool.

use crate::{
    blocklist::FeeCurrencyBlocklist,
    revert_evictions::{PayloadGeneration, RevertEvictionAttempt, RevertEvictions, RevertReason},
};
use alloc::sync::Arc;
use alloy_primitives::B256;
use spin::Mutex;

/// Shared blocklist, completed-payload revert evidence, and canonical-head state for pool-backed
/// sequencing, plus an optional buffer scoped to one payload attempt.
///
/// Keeping these channels in one value prevents the EVM producer and canonical pool consumer from
/// being configured independently. Cloning a policy-enabled EVM preserves its attempt buffer;
/// factory policies have no attempt until the sequencing builder attaches one.
#[derive(Debug, Clone, Default)]
pub struct CeloFailurePolicies {
    blocklist: FeeCurrencyBlocklist,
    revert_evictions: RevertEvictions,
    canonical_head: Arc<Mutex<Option<PayloadGeneration>>>,
    revert_eviction_attempt: Option<RevertEvictionAttempt>,
}

impl CeloFailurePolicies {
    /// Creates a policy bundle from its blocklist and revert-evidence channels.
    pub fn new(blocklist: FeeCurrencyBlocklist, revert_evictions: RevertEvictions) -> Self {
        Self {
            blocklist,
            revert_evictions,
            canonical_head: Default::default(),
            revert_eviction_attempt: None,
        }
    }

    /// Returns the shared fee currency blocklist.
    pub const fn blocklist(&self) -> &FeeCurrencyBlocklist {
        &self.blocklist
    }

    /// Returns the shared reverted-transaction eviction channel.
    pub const fn revert_evictions(&self) -> &RevertEvictions {
        &self.revert_evictions
    }

    /// Replaces the blocklist while preserving the shared evidence and canonical-head channels.
    pub fn with_blocklist(self, blocklist: FeeCurrencyBlocklist) -> Self {
        Self {
            blocklist,
            revert_evictions: self.revert_evictions,
            canonical_head: self.canonical_head,
            revert_eviction_attempt: self.revert_eviction_attempt,
        }
    }

    /// Binds a payload-attempt-local revert buffer while preserving every shared channel.
    pub fn with_revert_eviction_attempt(mut self, attempt: RevertEvictionAttempt) -> Self {
        self.revert_eviction_attempt = Some(attempt);
        self
    }

    /// Updates the canonical parent generation accepted by sequencing EVMs.
    pub fn set_canonical_head(&self, generation: PayloadGeneration) {
        *self.canonical_head.lock() = Some(generation);
    }

    /// Records into this payload attempt only while its parent is still the canonical head.
    pub fn record_revert_if_current(
        &self,
        generation: PayloadGeneration,
        tx_hash: B256,
        reason: RevertReason,
    ) -> bool {
        let canonical_head = self.canonical_head.lock();
        if *canonical_head != Some(generation) {
            return false;
        }
        let Some(attempt) = &self.revert_eviction_attempt else {
            return false;
        };
        attempt.record(tx_hash, reason);
        true
    }

    /// Blocklists a currency only while `generation` is still the canonical parent.
    pub fn block_currency_if_current(
        &self,
        generation: PayloadGeneration,
        fee_currency: alloy_primitives::Address,
        timestamp: u64,
    ) -> bool {
        let canonical_head = self.canonical_head.lock();
        if *canonical_head != Some(generation) {
            return false;
        }
        self.blocklist.block_currency(fee_currency, timestamp);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::CeloFailurePolicies;
    use crate::revert_evictions::{
        PayloadBlock, PayloadGeneration, RevertEviction, RevertEvictionAttempt, RevertReason,
    };
    use alloy_primitives::{Address, B256};

    #[test]
    fn clones_share_blocklist_and_revert_evictions() {
        let policies = CeloFailurePolicies::default();
        let clone = policies.clone();
        let fee_currency = Address::with_last_byte(1);
        let tx_hash = B256::with_last_byte(2);

        policies.blocklist().block_currency(fee_currency, 1_000);
        policies.revert_evictions().record(RevertEviction::new(
            tx_hash,
            RevertReason::Debit,
            PayloadBlock::new(1, B256::with_last_byte(3)),
        ));

        assert!(clone.blocklist().is_blocked(fee_currency));
        assert_eq!(clone.revert_evictions().take_all()[0].tx_hash, tx_hash);
    }

    #[test]
    fn records_only_evidence_built_on_the_current_canonical_head() {
        let attempt = RevertEvictionAttempt::default();
        let policies = CeloFailurePolicies::default().with_revert_eviction_attempt(attempt.clone());
        let current = PayloadGeneration::new(10, B256::with_last_byte(1));
        let old = PayloadGeneration::new(9, B256::with_last_byte(2));
        policies.set_canonical_head(current);

        assert!(!policies.record_revert_if_current(
            old,
            B256::with_last_byte(3),
            RevertReason::Debit,
        ));
        assert!(policies.record_revert_if_current(
            current,
            B256::with_last_byte(4),
            RevertReason::Credit,
        ));
        assert_eq!(
            policies
                .revert_evictions()
                .promote_attempt(&attempt, PayloadBlock::new(11, B256::with_last_byte(5)),),
            1,
        );
        assert_eq!(policies.revert_evictions().take_all().len(), 1);
    }
}
