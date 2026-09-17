//! Reverted CIP-64 transactions awaiting local pool eviction.

use alloc::{collections::BTreeSet, sync::Arc, vec::Vec};
use alloy_primitives::B256;
use spin::Mutex;

/// Parent block that identifies one payload-building generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PayloadGeneration {
    /// Parent block number.
    pub parent_number: u64,
    /// Parent block hash.
    pub parent_hash: B256,
}

impl PayloadGeneration {
    /// Creates a payload generation from its parent block.
    pub const fn new(parent_number: u64, parent_hash: B256) -> Self {
        Self { parent_number, parent_hash }
    }
}

/// Exact block produced by one completed payload attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PayloadBlock {
    /// Built block number.
    pub number: u64,
    /// Built block hash.
    pub hash: B256,
}

impl PayloadBlock {
    /// Creates an exact built-payload identity.
    pub const fn new(number: u64, hash: B256) -> Self {
        Self { number, hash }
    }
}

/// Fee-currency operation that reverted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RevertReason {
    /// `debitGasFees` reverted before user execution.
    Debit,
    /// The max-fee `balanceOf` read reverted before `debitGasFees`.
    BalanceRead,
    /// `creditGasFees` reverted after user execution.
    Credit,
}

/// Exact transaction evidence recorded by a completed sequencing payload attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevertEviction {
    /// Exact transaction hash.
    pub tx_hash: B256,
    /// Reverted fee-currency operation.
    pub reason: RevertReason,
    /// Exact completed payload block that produced the evidence.
    payload: PayloadBlock,
}

impl RevertEviction {
    /// Creates revert evidence associated with one exact completed payload block.
    pub const fn new(tx_hash: B256, reason: RevertReason, payload: PayloadBlock) -> Self {
        Self { tx_hash, reason, payload }
    }

    /// Returns the exact completed payload block that produced this evidence.
    pub const fn payload(&self) -> PayloadBlock {
        self.payload
    }
}

/// Revert records produced by one in-progress payload attempt.
///
/// This buffer is shared only by that attempt's EVM and block-builder wrapper. Dropping the
/// wrapper without successfully finishing the block drops the records. A successful finish
/// promotes them to [`RevertEvictions`] with the exact built child identity.
#[derive(Debug, Clone, Default)]
pub struct RevertEvictionAttempt {
    inner: Arc<Mutex<BTreeSet<(B256, RevertReason)>>>,
}

impl RevertEvictionAttempt {
    /// Records one transaction operation failure within this payload attempt.
    pub fn record(&self, tx_hash: B256, reason: RevertReason) {
        self.inner.lock().insert((tx_hash, reason));
    }

    fn take_all(&self) -> BTreeSet<(B256, RevertReason)> {
        core::mem::take(&mut *self.inner.lock())
    }
}

/// Shared completed-payload revert evidence awaiting canonical maintenance.
#[derive(Debug, Clone, Default)]
pub struct RevertEvictions {
    inner: Arc<Mutex<BTreeSet<(PayloadBlock, B256, RevertReason)>>>,
}

impl RevertEvictions {
    /// Records exact transaction-operation evidence.
    pub fn record(&self, eviction: RevertEviction) {
        self.inner.lock().insert((eviction.payload, eviction.tx_hash, eviction.reason));
    }

    /// Promotes one successful attempt's records with the exact block it produced.
    pub fn promote_attempt(&self, attempt: &RevertEvictionAttempt, payload: PayloadBlock) -> usize {
        let attempt_records = attempt.take_all();
        let count = attempt_records.len();
        if count == 0 {
            return 0;
        }

        let mut records = self.inner.lock();
        for (tx_hash, reason) in attempt_records {
            records.insert((payload, tx_hash, reason));
        }
        count
    }

    /// Removes every queued record without retaining the channel lock while it is processed.
    ///
    /// Draining all records lets callers discard stale payloads before applying a separate limit
    /// to canonical EVM rechecks. Concurrent writes land in a fresh map and are not lost.
    pub fn take_all(&self) -> Vec<RevertEviction> {
        let queued = core::mem::take(&mut *self.inner.lock());
        queued
            .into_iter()
            .map(|(payload, tx_hash, reason)| RevertEviction { tx_hash, reason, payload })
            .collect()
    }

    /// Merges records back into the shared queue without losing concurrent writes.
    pub fn requeue(&self, records: impl IntoIterator<Item = RevertEviction>) {
        let mut queued = self.inner.lock();
        for record in records {
            queued.insert((record.payload, record.tx_hash, record.reason));
        }
    }

    /// Returns the current number of queued records.
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// Returns whether no records are queued.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_operations_are_preserved_for_one_payload() {
        let evictions = RevertEvictions::default();
        let payload = PayloadBlock::new(10, B256::with_last_byte(1));
        let hash = B256::with_last_byte(2);

        evictions.record(RevertEviction::new(hash, RevertReason::Debit, payload));
        evictions.record(RevertEviction::new(hash, RevertReason::Credit, payload));

        let records = evictions.take_all();
        assert_eq!(records.len(), 2);
        assert!(records.contains(&RevertEviction::new(hash, RevertReason::Debit, payload)));
        assert!(records.contains(&RevertEviction::new(hash, RevertReason::Credit, payload)));
    }

    #[test]
    fn requeued_rechecks_preserve_exact_payload_and_distinct_operations() {
        let evictions = RevertEvictions::default();
        let payload = PayloadBlock::new(10, B256::with_last_byte(1));
        let hash = B256::with_last_byte(2);

        evictions.record(RevertEviction::new(hash, RevertReason::Debit, payload));
        evictions.record(RevertEviction::new(hash, RevertReason::BalanceRead, payload));
        let records = evictions.take_all();
        evictions.requeue(records);

        let records = evictions.take_all();
        assert_eq!(records.len(), 2);
        assert!(records.iter().any(|record| record.reason == RevertReason::Debit));
        assert!(records.iter().any(|record| record.reason == RevertReason::BalanceRead));
        assert!(records.iter().all(|record| record.payload() == payload));
    }

    #[test]
    fn sibling_attempts_are_preserved_with_exact_children() {
        let evictions = RevertEvictions::default();
        let hash = B256::with_last_byte(1);
        let first = PayloadBlock::new(10, B256::with_last_byte(2));
        let second = PayloadBlock::new(10, B256::with_last_byte(3));
        let first_attempt = RevertEvictionAttempt::default();
        let second_attempt = RevertEvictionAttempt::default();

        first_attempt.record(hash, RevertReason::Debit);
        second_attempt.record(hash, RevertReason::Debit);
        evictions.promote_attempt(&first_attempt, first);
        evictions.promote_attempt(&second_attempt, second);

        let records = evictions.take_all();
        assert_eq!(records.len(), 2);
        assert!(records.contains(&RevertEviction::new(hash, RevertReason::Debit, first)));
        assert!(records.contains(&RevertEviction::new(hash, RevertReason::Debit, second)));
    }

    #[test]
    fn successful_attempt_is_promoted_with_exact_payload() {
        let attempt = RevertEvictionAttempt::default();
        let evictions = RevertEvictions::default();
        let payload = PayloadBlock::new(10, B256::with_last_byte(1));
        let hash = B256::with_last_byte(2);
        attempt.record(hash, RevertReason::Credit);

        assert_eq!(evictions.promote_attempt(&attempt, payload), 1);

        assert_eq!(
            evictions.take_all(),
            vec![RevertEviction::new(hash, RevertReason::Credit, payload)]
        );
    }

    #[test]
    fn unpromoted_attempt_does_not_reach_shared_queue() {
        let attempt = RevertEvictionAttempt::default();
        let evictions = RevertEvictions::default();
        attempt.record(B256::with_last_byte(1), RevertReason::Debit);

        drop(attempt);

        assert!(evictions.is_empty());
    }

    #[test]
    fn requeue_merges_without_losing_concurrent_records() {
        let evictions = RevertEvictions::default();
        let clone = evictions.clone();
        let payload = PayloadBlock::new(10, B256::with_last_byte(1));
        let drained = RevertEviction::new(B256::with_last_byte(2), RevertReason::Debit, payload);
        let concurrent =
            RevertEviction::new(B256::with_last_byte(3), RevertReason::Credit, payload);

        evictions.record(drained);
        let records = evictions.take_all();
        clone.record(concurrent);
        evictions.requeue(records);

        let records = evictions.take_all();
        assert_eq!(records.len(), 2);
        assert!(records.contains(&drained));
        assert!(records.contains(&concurrent));
    }
}
