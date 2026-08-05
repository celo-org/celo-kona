//! Reverted CIP-64 transactions awaiting local pool eviction.

use alloc::{collections::BTreeSet, sync::Arc};
use alloy_primitives::B256;
use spin::Mutex;

/// Exact transaction hashes whose fee-currency debit or credit reverted during policy-enabled
/// sequencing execution.
#[derive(Debug, Clone, Default)]
pub struct RevertEvictions {
    inner: Arc<Mutex<BTreeSet<B256>>>,
}

impl RevertEvictions {
    /// Records a transaction for eviction by the sequencing payload filter.
    pub fn record(&self, tx_hash: B256) {
        self.inner.lock().insert(tx_hash);
    }

    /// Removes and returns whether a transaction was recorded for eviction.
    pub fn take(&self, tx_hash: B256) -> bool {
        self.inner.lock().remove(&tx_hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_is_consumed_once() {
        let evictions = RevertEvictions::default();
        let hash = B256::with_last_byte(1);

        evictions.record(hash);

        assert!(evictions.take(hash));
        assert!(!evictions.take(hash));
    }

    #[test]
    fn clones_share_entries() {
        let evictions = RevertEvictions::default();
        let clone = evictions.clone();
        let hash = B256::with_last_byte(3);

        evictions.record(hash);

        assert!(clone.take(hash));
    }
}
