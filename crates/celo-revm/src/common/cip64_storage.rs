//! Shared storage for CIP-64 transaction execution results.
//! 
//! This module provides a thread-safe storage mechanism for sharing CIP-64 transaction
//! execution results between the EVM handler and the receipt builder.

use revm::primitives::{HashMap, B256};
use spin::Mutex;
use std::sync::Arc;

use crate::transaction::abstraction::Cip64Info;

/// Shared storage for CIP-64 transaction execution results.
/// 
/// This storage is used to share CIP-64 execution results (including revert status
/// and revert data) between the EVM execution and receipt building phases.
#[derive(Debug, Clone, Default)]
pub struct Cip64Storage {
    inner: Arc<Mutex<HashMap<B256, Cip64Info>>>,
}

impl Cip64Storage {
    /// Creates a new empty CIP-64 storage.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::default())),
        }
    }

    /// Stores CIP-64 execution info for a cip64 transaction.
    /// 
    /// # Arguments
    /// * `info` - The CIP-64 execution information
    pub fn store_cip64_info(&self, identifier: B256, info: Cip64Info) {
        let mut storage = self.inner.lock();
        storage.insert(identifier, info);
    }

    /// Retrieves CIP-64 execution info for a transaction.
    /// 
    /// # Arguments
    /// * `tx_hash` - The transaction hash
    /// 
    /// # Returns
    /// The CIP-64 execution info if found, None otherwise
    pub fn get_cip64_info(&self, identifier: &B256) -> Option<Cip64Info> {
        let storage = self.inner.lock();
        storage.get(identifier).cloned()
    }
}