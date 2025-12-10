//! Shared storage for CIP-64 transaction execution results.
//!
//! This module provides a thread-safe storage mechanism for sharing CIP-64 transaction
//! execution results between the EVM handler and the receipt builder.

use alloc::sync::Arc;
use alloy_primitives::{Address, keccak256};
use celo_revm::Cip64Info;
use revm::primitives::{B256, HashMap};
use spin::Mutex;

/// Shared storage for CIP-64 transaction execution results.
#[derive(Debug, Clone, Default)]
pub struct Cip64Storage {
    inner: Arc<Mutex<HashMap<B256, Cip64Info>>>,
}

/// Generates a unique identifier for a transaction based on caller address and nonce.
pub fn get_tx_identifier(caller: Address, nonce: u64) -> B256 {
    keccak256([caller.as_slice(), &nonce.to_be_bytes()].concat())
}

/// Returns a null/none transaction identifier.
pub fn none_tx_identifier() -> B256 {
    [0u8; 32].into()
}

impl Cip64Storage {
    /// Stores CIP-64 execution info for a cip64 transaction.
    pub fn store_cip64_info(&self, identifier: B256, info: Cip64Info) {
        let mut storage = self.inner.lock();
        storage.insert(identifier, info);
    }

    /// Retrieves CIP-64 execution info for a transaction.
    pub fn get_cip64_info(&self, identifier: &B256) -> Option<Cip64Info> {
        let storage = self.inner.lock();
        storage.get(identifier).cloned()
    }
}
