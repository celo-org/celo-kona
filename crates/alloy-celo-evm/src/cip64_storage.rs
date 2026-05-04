//! Shared storage for CIP-64 transaction execution results.
//!
//! This module provides a thread-safe storage mechanism for sharing CIP-64 transaction
//! execution results between the EVM handler and the receipt builder.

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use alloy_primitives::Address;
use celo_revm::Cip64Info;
use revm::primitives::Log;
use spin::Mutex;

/// Data needed by the receipt builder for a CIP-64 transaction.
#[derive(Debug, Clone)]
pub struct Cip64ReceiptData {
    /// The fee currency used (None means native CELO)
    pub fee_currency: Option<Address>,
    /// CIP-64 execution info (pre/post logs, reverted flag, etc.)
    pub cip64_info: Cip64Info,
}

/// Shared storage for CIP-64 transaction execution results.
///
/// The `receipt_queue` is a FIFO consumed by the receipt builder as each CIP-64
/// transaction is processed. The optional `all_entries` Vec (gated behind the
/// `test-utils` feature) accumulates every entry without consuming them, and
/// exists purely so test harnesses can inspect post-execution CIP-64 gas
/// accounting. It must stay off in production: `store_cip64_info` runs on every
/// CIP-64 tx, and an unbounded Vec on a long-running node would leak memory
/// linearly with CIP-64 tx volume.
#[derive(Debug, Clone, Default)]
pub struct Cip64Storage {
    /// Queue of receipt data in transaction execution order (consumed by receipt builder).
    receipt_queue: Arc<Mutex<VecDeque<Cip64ReceiptData>>>,
    /// Accumulated entries for post-execution inspection (test-only).
    #[cfg(any(test, feature = "test-utils"))]
    all_entries: Arc<Mutex<Vec<Cip64ReceiptData>>>,
}

impl Cip64Storage {
    /// Stores CIP-64 execution info for a transaction, enqueueing it for receipt building.
    pub fn store_cip64_info(&self, fee_currency: Option<Address>, info: Cip64Info) {
        let data = Cip64ReceiptData { fee_currency, cip64_info: info };
        #[cfg(any(test, feature = "test-utils"))]
        self.all_entries.lock().push(data.clone());
        self.receipt_queue.lock().push_back(data);
    }

    /// Pops the next CIP-64 receipt data from the queue for receipt building.
    pub fn pop_cip64_receipt_data(&self) -> Option<Cip64ReceiptData> {
        self.receipt_queue.lock().pop_front()
    }

    /// Number of receipt entries waiting to be drained. Should be zero between blocks.
    pub fn pending_receipt_count(&self) -> usize {
        self.receipt_queue.lock().len()
    }

    /// Returns all stored CIP-64 receipt data entries (not consumed by this call).
    ///
    /// Only available when the `test-utils` feature is enabled — this is a
    /// diagnostic used by the kona executor's CIP-64 gas-accounting tests.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn all_entries(&self) -> Vec<Cip64ReceiptData> {
        self.all_entries.lock().clone()
    }

    /// Merges CIP-64 pre/post logs with the main execution logs.
    pub fn merge_logs(cip64_info: &Cip64Info, main_logs: Vec<Log>) -> Vec<Log> {
        cip64_info
            .logs_pre
            .iter()
            .cloned()
            .chain(main_logs)
            .chain(cip64_info.logs_post.iter().cloned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;

    #[test]
    fn receipt_queue_drained_by_pop() {
        // Fundamental invariant: the consuming queue keeps no history of its
        // own — once the receipt builder pops an entry, the queue shrinks.
        // If this ever regressed, celo-reth would leak memory identically to
        // the pre-fix `all_entries` case.
        let storage = Cip64Storage::default();
        for _ in 0..100 {
            storage.store_cip64_info(None, Cip64Info::default());
        }
        assert_eq!(storage.receipt_queue.lock().len(), 100);
        while storage.pop_cip64_receipt_data().is_some() {}
        assert_eq!(storage.receipt_queue.lock().len(), 0);
    }

    #[test]
    fn pending_receipt_count_tracks_queue() {
        let storage = Cip64Storage::default();
        assert_eq!(storage.pending_receipt_count(), 0);
        storage.store_cip64_info(None, Cip64Info::default());
        assert_eq!(storage.pending_receipt_count(), 1);
        storage.store_cip64_info(None, Cip64Info::default());
        storage.store_cip64_info(None, Cip64Info::default());
        assert_eq!(storage.pending_receipt_count(), 3);
        storage.pop_cip64_receipt_data();
        assert_eq!(storage.pending_receipt_count(), 2);
    }

    #[test]
    fn all_entries_accumulates_without_consuming() {
        let storage = Cip64Storage::default();
        let fc = Some(Address::with_last_byte(7));
        storage.store_cip64_info(fc, Cip64Info::default());
        storage.store_cip64_info(None, Cip64Info::default());
        // pop one — `all_entries` must still report both.
        storage.pop_cip64_receipt_data();
        let entries = storage.all_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].fee_currency, fc);
        assert_eq!(entries[1].fee_currency, None);
    }

    fn log_with_addr(b: u8) -> Log {
        Log { address: Address::with_last_byte(b), data: Default::default() }
    }

    #[test]
    fn merge_logs_orders_pre_main_post() {
        let info = Cip64Info {
            logs_pre: vec![log_with_addr(1), log_with_addr(2)],
            logs_post: vec![log_with_addr(5)],
            ..Default::default()
        };
        let main = vec![log_with_addr(3), log_with_addr(4)];
        let merged = Cip64Storage::merge_logs(&info, main);
        let addrs: Vec<u8> = merged.iter().map(|l| l.address.0[19]).collect();
        assert_eq!(addrs, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn merge_logs_empty_inputs() {
        let info = Cip64Info::default();
        assert!(Cip64Storage::merge_logs(&info, vec![]).is_empty());
    }

    #[test]
    fn merge_logs_only_main() {
        // Empty pre/post is the common case for non-CIP-64 paths reusing this
        // helper; the merge must not introduce phantom logs around `main`.
        let info = Cip64Info::default();
        let main = vec![log_with_addr(9)];
        let merged = Cip64Storage::merge_logs(&info, main);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].address, Address::with_last_byte(9));
    }
}
