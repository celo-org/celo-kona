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
#[derive(Debug, Clone, Default)]
pub struct Cip64Storage {
    /// Queue of receipt data in transaction execution order (consumed by receipt builder).
    receipt_queue: Arc<Mutex<VecDeque<Cip64ReceiptData>>>,
    /// Accumulated entries for post-execution inspection (not consumed).
    all_entries: Arc<Mutex<Vec<Cip64ReceiptData>>>,
}

impl Cip64Storage {
    /// Stores CIP-64 execution info for a transaction, enqueueing it for receipt building.
    pub fn store_cip64_info(&self, fee_currency: Option<Address>, info: Cip64Info) {
        let data = Cip64ReceiptData { fee_currency, cip64_info: info };
        self.receipt_queue.lock().push_back(data.clone());
        self.all_entries.lock().push(data);
    }

    /// Pops the next CIP-64 receipt data from the queue.
    /// Returns the merged logs and fee_currency for receipt building.
    pub fn pop_cip64_receipt_data(&self) -> Option<Cip64ReceiptData> {
        self.receipt_queue.lock().pop_front()
    }

    /// Returns all stored CIP-64 receipt data entries (not consumed by this call).
    pub fn all_entries(&self) -> Vec<Cip64ReceiptData> {
        self.all_entries.lock().clone()
    }

    /// Merges CIP-64 pre/post logs with the main execution logs.
    pub fn merge_logs(cip64_info: &Cip64Info, main_logs: Vec<Log>) -> Vec<Log> {
        let capacity = cip64_info.logs_pre.len() + main_logs.len() + cip64_info.logs_post.len();
        let mut merged = Vec::with_capacity(capacity);
        merged.extend_from_slice(&cip64_info.logs_pre);
        merged.extend(main_logs);
        merged.extend_from_slice(&cip64_info.logs_post);
        merged
    }
}
