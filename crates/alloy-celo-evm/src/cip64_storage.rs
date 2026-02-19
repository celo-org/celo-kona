//! Shared storage for CIP-64 transaction execution results.
//!
//! This module provides a thread-safe storage mechanism for sharing CIP-64 transaction
//! execution results between the EVM handler and the receipt builder.

use alloc::sync::Arc;
use alloy_primitives::{Address, keccak256};
use celo_revm::Cip64Info;
use revm::primitives::{B256, HashMap};
use spin::Mutex;

/// Context for the current CIP-64 transaction being processed.
/// Set during execution, consumed during receipt building.
#[derive(Debug, Clone)]
pub struct CurrentCip64Context {
    /// Fee currency of the transaction (None = paid in CELO)
    pub fee_currency: Option<Address>,
    /// CIP-64 execution info (logs, gas tracking)
    pub cip64_info: Option<Cip64Info>,
}

/// Shared storage for CIP-64 transaction execution results.
#[derive(Debug, Clone, Default)]
pub struct Cip64Storage {
    inner: Arc<Mutex<HashMap<B256, Cip64Info>>>,
    /// Context for the most recently executed CIP-64 transaction.
    /// Used to pass data from execution to receipt building without
    /// requiring the full transaction in the receipt builder.
    current: Arc<Mutex<Option<CurrentCip64Context>>>,
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

    /// Sets the current CIP-64 context for receipt building.
    /// Called during transaction execution.
    pub fn set_current_context(&self, context: CurrentCip64Context) {
        *self.current.lock() = Some(context);
    }

    /// Takes (consumes) the current CIP-64 context.
    /// Called during receipt building to retrieve data set during execution.
    pub fn take_current_context(&self) -> Option<CurrentCip64Context> {
        self.current.lock().take()
    }
}
