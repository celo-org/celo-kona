//! Reverted CIP-64 transactions awaiting local pool eviction.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
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

/// Fee-currency hook that reverted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RevertReason {
    /// `debitGasFees` reverted before user execution.
    Debit,
    /// `creditGasFees` reverted after user execution.
    Credit,
}

/// Exact transaction evidence recorded by a sequencing payload attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevertEviction {
    /// Exact transaction hash.
    pub tx_hash: B256,
    /// Reverted fee-currency hook.
    pub reason: RevertReason,
    /// Payload parent against which the revert was observed.
    pub generation: PayloadGeneration,
}

impl RevertEviction {
    /// Creates revert evidence for one exact transaction and payload generation.
    pub const fn new(tx_hash: B256, reason: RevertReason, generation: PayloadGeneration) -> Self {
        Self { tx_hash, reason, generation }
    }
}

/// Bounded set of records removed from the shared queue for one maintenance pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RevertEvictionBatch {
    /// Owned records. No channel lock remains held while they are processed.
    pub records: Vec<RevertEviction>,
    /// Records still queued after this batch was removed.
    pub remaining: usize,
}

/// Shared sequencing revert evidence awaiting canonical maintenance.
#[derive(Debug, Clone, Default)]
pub struct RevertEvictions {
    inner: Arc<Mutex<BTreeMap<(PayloadGeneration, B256), RevertReason>>>,
}

impl RevertEvictions {
    /// Records or merges exact transaction evidence.
    pub fn record(&self, eviction: RevertEviction) {
        let mut records = self.inner.lock();
        Self::merge(&mut records, eviction);
    }

    /// Removes at most `limit` records for processing without retaining the channel lock.
    pub fn take_batch(&self, limit: usize) -> RevertEvictionBatch {
        let mut queued = self.inner.lock();
        let mut records = Vec::with_capacity(limit.min(queued.len()));
        while records.len() < limit {
            let Some(((generation, tx_hash), reason)) = queued.pop_first() else {
                break;
            };
            records.push(RevertEviction::new(tx_hash, reason, generation));
        }
        RevertEvictionBatch { records, remaining: queued.len() }
    }

    /// Merges records back into the shared queue without losing concurrent writes.
    pub fn requeue(&self, records: impl IntoIterator<Item = RevertEviction>) {
        let mut queued = self.inner.lock();
        for record in records {
            Self::merge(&mut queued, record);
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

    fn merge(
        records: &mut BTreeMap<(PayloadGeneration, B256), RevertReason>,
        eviction: RevertEviction,
    ) {
        records
            .entry((eviction.generation, eviction.tx_hash))
            .and_modify(|reason| *reason = (*reason).max(eviction.reason))
            .or_insert(eviction.reason);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credit_dominates_debit_for_one_generation() {
        let evictions = RevertEvictions::default();
        let generation = PayloadGeneration::new(10, B256::with_last_byte(1));
        let hash = B256::with_last_byte(2);

        evictions.record(RevertEviction::new(hash, RevertReason::Debit, generation));
        evictions.record(RevertEviction::new(hash, RevertReason::Credit, generation));

        let batch = evictions.take_batch(16);
        assert_eq!(
            batch.records,
            vec![RevertEviction::new(hash, RevertReason::Credit, generation)]
        );
        assert_eq!(batch.remaining, 0);
    }

    #[test]
    fn same_hash_in_different_generations_is_preserved() {
        let evictions = RevertEvictions::default();
        let hash = B256::with_last_byte(1);
        let first = PayloadGeneration::new(10, B256::with_last_byte(2));
        let second = PayloadGeneration::new(11, B256::with_last_byte(3));

        evictions.record(RevertEviction::new(hash, RevertReason::Debit, first));
        evictions.record(RevertEviction::new(hash, RevertReason::Debit, second));

        let batch = evictions.take_batch(16);
        assert_eq!(batch.records.len(), 2);
        assert!(batch.records.contains(&RevertEviction::new(hash, RevertReason::Debit, first)));
        assert!(batch.records.contains(&RevertEviction::new(hash, RevertReason::Debit, second)));
    }

    #[test]
    fn take_batch_leaves_overflow_queued() {
        let evictions = RevertEvictions::default();
        let generation = PayloadGeneration::new(10, B256::with_last_byte(1));
        for byte in 1..=3 {
            evictions.record(RevertEviction::new(
                B256::with_last_byte(byte),
                RevertReason::Debit,
                generation,
            ));
        }

        let first = evictions.take_batch(2);
        assert_eq!(first.records.len(), 2);
        assert_eq!(first.remaining, 1);
        let second = evictions.take_batch(2);
        assert_eq!(second.records.len(), 1);
        assert_eq!(second.remaining, 0);
    }

    #[test]
    fn requeue_merges_without_losing_concurrent_records() {
        let evictions = RevertEvictions::default();
        let clone = evictions.clone();
        let generation = PayloadGeneration::new(10, B256::with_last_byte(1));
        let drained = RevertEviction::new(B256::with_last_byte(2), RevertReason::Debit, generation);
        let concurrent =
            RevertEviction::new(B256::with_last_byte(3), RevertReason::Credit, generation);

        evictions.record(drained);
        let batch = evictions.take_batch(16);
        clone.record(concurrent);
        evictions.requeue(batch.records);

        let records = evictions.take_batch(16).records;
        assert_eq!(records.len(), 2);
        assert!(records.contains(&drained));
        assert!(records.contains(&concurrent));
    }
}
