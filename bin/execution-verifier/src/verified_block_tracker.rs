use std::collections::BTreeSet;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;

/// Tracks verified blocks and maintains the highest consecutive verified block.
pub(crate) struct VerifiedBlockTracker {
    /// The starting block number for verification. If None, will be set to the first block added.
    start: Option<u64>,
    /// The highest block number that forms a consecutive sequence from start.
    highest: OnceLock<AtomicU64>,
    /// List of verified block numbers that haven't been processed yet.
    /// Blocks are removed from this list once they become part of the consecutive sequence.
    pending: Mutex<BTreeSet<u64>>,
}

impl VerifiedBlockTracker {
    /// Creates a new `VerifiedBlockTracker` with an optional starting block.
    ///
    /// # Arguments
    ///
    /// * `start` - The starting block number for verification. If `None`, it will be set to
    ///   the first block that gets marked verified.
    pub(crate) fn new(start: Option<u64>) -> Self {
        Self { start, highest: OnceLock::new(), pending: Mutex::new(BTreeSet::new()) }
    }

    /// Returns the highest block number that forms a consecutive sequence.
    ///
    /// # Returns
    ///
    /// * `Some(block_number)` - The highest consecutive verified block
    /// * `None` - If no consecutive sequence has been established yet
    pub(crate) fn highest(&self) -> Option<u64> {
        self.highest.get().map(|a| a.load(Ordering::Acquire))
    }

    ///
    /// Marks a block as verified and recalculates the highest consecutive verified block.
    ///
    /// This method sorts the verified blocks, removes duplicates, and determines
    /// the highest block that forms a consecutive sequence starting from the
    /// current highest verified block (or `start` if none exists yet).
    ///
    /// Blocks that become part of the consecutive sequence are removed from
    /// the internal btree to save memory.
    ///
    /// # Returns
    ///
    /// A tuple of (highest_verified_block, was_updated) where:
    /// * `highest_verified_block` - The current highest consecutive verified block (or None)
    /// * `was_updated` - Whether the highest verified block changed during this update
    pub(crate) async fn mark_verified(&self, n: u64) -> (Option<u64>, bool) {
        self.mark_verified_many(std::iter::once(n)).await
    }

    /// Batch-friendly version (one lock acquisition).
    pub(crate) async fn mark_verified_many<I: IntoIterator<Item = u64>>(
        &self,
        blocks: I,
    ) -> (Option<u64>, bool) {
        // OPTIM: sync barrier / bottleneck, all concurrent tasks await this.
        let mut set = self.pending.lock().await;
        let mut advanced = false;
        let mut highest: Option<u64> = None;

        // Insert candidates.
        for b in blocks {
            set.insert(b);
        }

        // If we already published a highest, start checking from the next after that.
        let mut next_probe = if let Some(a) = self.highest.get() {
            let h = a.load(Ordering::Acquire);
            highest = Some(h);
            h + 1
        } else if let Some(s) = self.start {
            // No publication yet, but we have an explicit start.
            s
        } else {
            // No publication yet and no explicit start.
            // Bootstrap from the lowest verified block seen so far (if any).
            match set.iter().next().copied() {
                None => return (None, false), // nothing to do
                Some(m) => m, // we know this will work, but this simplifies the code
            }
        };

        // advance h while we have consecutive next values.
        while set.remove(&(next_probe)) {
            next_probe += 1;
            advanced = true;
        }

        // publish & truncate if we advanced.
        if advanced {
            highest = Some(next_probe - 1);
            // TODO: panic message
            let h = highest.expect("panic");
            // we held the lock, so it's fine for simplicity to get the
            // values again
            if let Some(a) = self.highest.get() {
                a.store(h, Ordering::Release);
            } else if self.highest.set(AtomicU64::new(h)).is_err() {
                // Not possible when this is the only write-ocne,
                // but we lost the race to initialize; just store.
                self.highest.get().unwrap().store(h, Ordering::Release);
            }
            let keep = set.split_off(&(next_probe));
            *set = keep;
        } else {
            if let Some(h) = highest {
                let keep = set.split_off(&(h));
                *set = keep;
            }
        }
        return (highest, advanced);
        // mutex drops after the return
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_no_verified_blocks() {
        let tracker = VerifiedBlockTracker::new(Some(1));
        assert_eq!(tracker.highest(), None);

        let tracker = VerifiedBlockTracker::new(None);
        assert_eq!(tracker.highest(), None);
    }

    #[tokio::test]
    async fn test_single_block() {
        let tracker = VerifiedBlockTracker::new(Some(1));
        tracker.mark_verified(1).await;
        assert_eq!(tracker.highest(), Some(1));

        let tracker = VerifiedBlockTracker::new(None);
        tracker.mark_verified(1).await;
        assert_eq!(tracker.highest(), Some(1));
    }
    #[tokio::test]
    async fn test_single_block_not_at_start() {
        let tracker = VerifiedBlockTracker::new(Some(1));
        tracker.mark_verified(2).await;
        assert_eq!(tracker.highest(), None);

        let tracker = VerifiedBlockTracker::new(None);
        tracker.mark_verified(2).await;
        assert_eq!(tracker.highest(), Some(2));
    }

    #[tokio::test]
    async fn test_consecutive_blocks() {
        let tracker = VerifiedBlockTracker::new(Some(1));
        tracker.mark_verified(1).await;
        tracker.mark_verified(2).await;
        tracker.mark_verified(3).await;
        tracker.mark_verified(4).await;
        assert_eq!(tracker.highest(), Some(4));

        let tracker = VerifiedBlockTracker::new(Some(1));
        tracker.mark_verified_many(1..=4).await;
        assert_eq!(tracker.highest(), Some(4));

        let tracker = VerifiedBlockTracker::new(None);
        tracker.mark_verified(1).await;
        tracker.mark_verified(2).await;
        tracker.mark_verified(3).await;
        tracker.mark_verified(4).await;
        assert_eq!(tracker.highest(), Some(4));

        let tracker = VerifiedBlockTracker::new(None);
        tracker.mark_verified_many(1..=4).await;
        assert_eq!(tracker.highest(), Some(4));
    }

    #[tokio::test]
    async fn test_out_of_order_insertion() {
        let tracker = VerifiedBlockTracker::new(Some(1));
        tracker.mark_verified(3).await;
        tracker.mark_verified(1).await;
        tracker.mark_verified(2).await;
        tracker.mark_verified(5).await;
        assert_eq!(tracker.highest(), Some(3));

        let tracker = VerifiedBlockTracker::new(None);
        tracker.mark_verified(3).await;
        tracker.mark_verified(1).await;
        tracker.mark_verified(2).await;
        tracker.mark_verified(5).await;
        assert_eq!(tracker.highest(), Some(3));
    }

    #[tokio::test]
    async fn test_duplicate_blocks() {
        let tracker = VerifiedBlockTracker::new(Some(1));
        tracker.mark_verified(1).await;
        tracker.mark_verified(1).await;
        tracker.mark_verified(2).await;
        tracker.mark_verified(2).await;
        assert_eq!(tracker.highest(), Some(2));
    }

    #[tokio::test]
    async fn test_verified_blocks_removed() {
        let tracker = VerifiedBlockTracker::new(None);
        tracker.mark_verified(1).await;
        tracker.mark_verified(2).await;
        tracker.mark_verified(4).await;
        tracker.mark_verified(5).await;

        assert_eq!(tracker.highest(), Some(2));

        let verified_blocks: Vec<u64> = {
            let set = tracker.pending.lock().await;
            set.iter().copied().collect()
        };
        // Only blocks 4 and 5 should remain (after the gap at 3)
        assert_eq!(verified_blocks, vec![4u64, 5u64]);

        // Add block 3 to fill the gap
        tracker.mark_verified(3).await;
        assert_eq!(tracker.highest(), Some(5));

        let verified_blocks_empty: Vec<u64> = {
            let set = tracker.pending.lock().await;
            set.iter().copied().collect()
        };
        // All blocks should be removed now as they're all consecutive
        assert_eq!(verified_blocks_empty, Vec::<u64>::new());
    }
}
