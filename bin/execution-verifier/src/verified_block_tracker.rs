/// Tracks verified blocks and maintains the highest consecutive verified block.
pub(crate) struct VerifiedBlockTracker {
    /// The starting block number for verification. If None, will be set to the first block added.
    start_block: Option<u64>,
    /// The highest block number that forms a consecutive sequence from start_block.
    highest_verified_block: Option<u64>,
    /// List of verified block numbers that haven't been processed yet.
    /// Blocks are removed from this list once they become part of the consecutive sequence.
    verified_blocks: Vec<u64>,
}

impl VerifiedBlockTracker {
    /// Creates a new `VerifiedBlockTracker` with an optional starting block.
    ///
    /// # Arguments
    ///
    /// * `start_block` - The starting block number for verification. If `None`, it will be set to
    ///   the first block that gets added.
    pub(crate) const fn new(start_block: Option<u64>) -> Self {
        Self { start_block, highest_verified_block: None, verified_blocks: Vec::new() }
    }

    /// Returns the highest block number that forms a consecutive sequence.
    ///
    /// # Returns
    ///
    /// * `Some(block_number)` - The highest consecutive verified block
    /// * `None` - If no consecutive sequence has been established yet
    pub(crate) const fn highest_verified_block(&self) -> Option<u64> {
        self.highest_verified_block
    }

    /// Adds a block number to the list of verified blocks.
    pub(crate) fn add_verified_block(&mut self, block_number: u64) {
        self.verified_blocks.push(block_number);
    }

    /// Recalculates the highest consecutive verified block.
    ///
    /// This method sorts the verified blocks, removes duplicates, and determines
    /// the highest block that forms a consecutive sequence starting from the
    /// current highest verified block (or start_block if none exists yet).
    ///
    /// Blocks that become part of the consecutive sequence are removed from
    /// the internal list to save memory.
    pub(crate) fn update_highest_verified_block(&mut self) {
        // Sort and deduplicate only when we need to calculate
        self.verified_blocks.sort_unstable();
        self.verified_blocks.dedup();

        if self.start_block.is_none() {
            self.start_block = self.verified_blocks.first().cloned();
        }

        let mut start =
            self.highest_verified_block.map(|h| h + 1).unwrap_or(self.start_block.unwrap_or(0));

        tracing::debug!(
            "debug::VerifiedBlockTracker::update_highest_verified_block starts start_block={:?}, start={}, highest_verified_block={:?}, verified_blocks={:?}",
            self.start_block,
            start,
            self.highest_verified_block,
            self.verified_blocks
        );

        for verified_block in self.verified_blocks.iter() {
            if *verified_block == start {
                self.highest_verified_block = Some(*verified_block);
                start += 1;
            } else {
                break;
            }
        }

        self.verified_blocks.retain(|&b| b > self.highest_verified_block.unwrap_or_default());

        tracing::debug!(
            "debug::VerifiedBlockTracker ends highest_verified_block={:?} verified_blocks={:?}",
            self.highest_verified_block,
            self.verified_blocks
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_verified_blocks() {
        let mut tracker = VerifiedBlockTracker::new(Some(1));
        assert_eq!(tracker.highest_verified_block(), None);
        tracker.update_highest_verified_block();
        assert_eq!(tracker.highest_verified_block(), None);

        let mut tracker = VerifiedBlockTracker::new(None);
        assert_eq!(tracker.highest_verified_block(), None);
        tracker.update_highest_verified_block();
        assert_eq!(tracker.highest_verified_block(), None);
    }

    #[test]
    fn test_single_block() {
        let mut tracker = VerifiedBlockTracker::new(Some(1));
        tracker.add_verified_block(1);
        tracker.update_highest_verified_block();
        assert_eq!(tracker.highest_verified_block(), Some(1));

        let mut tracker = VerifiedBlockTracker::new(None);
        tracker.add_verified_block(1);
        tracker.update_highest_verified_block();
        assert_eq!(tracker.highest_verified_block(), Some(1));
    }
    #[test]
    fn test_single_block_not_at_start() {
        let mut tracker = VerifiedBlockTracker::new(Some(1));
        tracker.add_verified_block(2);
        tracker.update_highest_verified_block();
        assert_eq!(tracker.highest_verified_block(), None);

        let mut tracker = VerifiedBlockTracker::new(None);
        tracker.add_verified_block(2);
        tracker.update_highest_verified_block();
        assert_eq!(tracker.highest_verified_block(), Some(2));
    }

    #[test]
    fn test_consecutive_blocks() {
        let mut tracker = VerifiedBlockTracker::new(Some(1));
        tracker.add_verified_block(1);
        tracker.add_verified_block(2);
        tracker.add_verified_block(3);
        tracker.add_verified_block(4);
        tracker.update_highest_verified_block();
        assert_eq!(tracker.highest_verified_block(), Some(4));

        let mut tracker = VerifiedBlockTracker::new(None);
        tracker.add_verified_block(1);
        tracker.add_verified_block(2);
        tracker.add_verified_block(3);
        tracker.add_verified_block(4);
        tracker.update_highest_verified_block();
        assert_eq!(tracker.highest_verified_block(), Some(4));
    }

    #[test]
    fn test_out_of_order_insertion() {
        let mut tracker = VerifiedBlockTracker::new(Some(1));
        tracker.add_verified_block(3);
        tracker.add_verified_block(1);
        tracker.add_verified_block(2);
        tracker.add_verified_block(5);
        tracker.update_highest_verified_block();
        assert_eq!(tracker.highest_verified_block(), Some(3));

        // verify `update_highest_verified_block` is idempotent
        tracker.update_highest_verified_block();
        assert_eq!(tracker.highest_verified_block(), Some(3));

        let mut tracker = VerifiedBlockTracker::new(None);
        tracker.add_verified_block(3);
        tracker.add_verified_block(1);
        tracker.add_verified_block(2);
        tracker.add_verified_block(5);
        tracker.update_highest_verified_block();
        assert_eq!(tracker.highest_verified_block(), Some(3));
        tracker.update_highest_verified_block();
        assert_eq!(tracker.highest_verified_block(), Some(3));
    }

    #[test]
    fn test_duplicate_blocks() {
        let mut tracker = VerifiedBlockTracker::new(Some(1));
        tracker.add_verified_block(1);
        tracker.add_verified_block(1);
        tracker.add_verified_block(2);
        tracker.add_verified_block(2);
        tracker.update_highest_verified_block();
        assert_eq!(tracker.highest_verified_block(), Some(2));
    }

    #[test]
    fn test_verified_blocks_removed() {
        let mut tracker = VerifiedBlockTracker::new(None);
        tracker.add_verified_block(1);
        tracker.add_verified_block(2);
        tracker.add_verified_block(4);
        tracker.add_verified_block(5);
        tracker.update_highest_verified_block();

        assert_eq!(tracker.highest_verified_block(), Some(2));
        // Only blocks 4 and 5 should remain (after the gap at 3)
        assert_eq!(tracker.verified_blocks, vec![4u64, 5u64]);

        // Add block 3 to fill the gap
        tracker.add_verified_block(3);
        tracker.update_highest_verified_block();

        assert_eq!(tracker.highest_verified_block(), Some(5));
        // All blocks should be removed now as they're all consecutive
        assert_eq!(tracker.verified_blocks, Vec::<u64>::new());
    }
}
