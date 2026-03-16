//! Fee currency blocklist for CIP-64 transactions.
//!
//! Provides a thread-safe blocklist mechanism for fee currencies that cause execution
//! errors. When a fee currency is blocklisted, CIP-64 transactions using that currency
//! are rejected early during block building, avoiding repeated execution failures.
//!
//! Blocklisting can be controlled per-currency via admin RPCs:
//! - `admin_disableBlocklistFeeCurrencies`: Prevents a currency from being blocklisted.
//! - `admin_enableBlocklistFeeCurrencies`: Re-enables blocklisting for a currency.
//! - `admin_unblockFeeCurrency`: Removes a currency from the blocklist.

use alloc::{collections::BTreeSet, sync::Arc};
use alloy_primitives::Address;
use spin::Mutex;

/// Internal state for the fee currency blocklist.
#[derive(Debug, Default)]
struct BlocklistState {
    /// Currencies currently blocked due to execution errors.
    blocked: BTreeSet<Address>,
    /// Currencies for which blocklisting is disabled (will never be auto-blocked).
    blocklist_disabled: BTreeSet<Address>,
}

/// Shared, thread-safe fee currency blocklist.
///
/// When a CIP-64 transaction using a particular fee currency fails during EVM
/// execution, the currency is added to the blocklist. Subsequent transactions
/// using that currency are rejected early without EVM execution.
#[derive(Debug, Clone, Default)]
pub struct FeeCurrencyBlocklist {
    inner: Arc<Mutex<BlocklistState>>,
}

impl FeeCurrencyBlocklist {
    /// Returns `true` if the given currency is currently blocked.
    pub fn is_blocked(&self, currency: Address) -> bool {
        self.inner.lock().blocked.contains(&currency)
    }

    /// Adds a currency to the blocklist (if blocklisting is not disabled for it).
    pub fn block_currency(&self, currency: Address) {
        let mut state = self.inner.lock();
        if !state.blocklist_disabled.contains(&currency) {
            state.blocked.insert(currency);
        }
    }

    /// Removes a currency from the blocklist.
    pub fn unblock_currency(&self, currency: Address) {
        self.inner.lock().blocked.remove(&currency);
    }

    /// Disables blocklisting for the given currencies.
    /// Also unblocks them if currently blocked.
    pub fn disable_blocklist(&self, currencies: &[Address]) {
        let mut state = self.inner.lock();
        for &c in currencies {
            state.blocklist_disabled.insert(c);
            state.blocked.remove(&c);
        }
    }

    /// Re-enables blocklisting for the given currencies.
    pub fn enable_blocklist(&self, currencies: &[Address]) {
        let mut state = self.inner.lock();
        for &c in currencies {
            state.blocklist_disabled.remove(&c);
        }
    }
}
