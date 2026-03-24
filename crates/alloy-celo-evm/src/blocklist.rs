//! Fee currency blocklist for CIP-64 transactions.
//!
//! Provides a thread-safe blocklist mechanism for fee currencies that cause execution
//! errors. When a fee currency is blocklisted, CIP-64 transactions using that currency
//! are rejected early during block building, avoiding repeated execution failures.
//!
//! This lives in `alloy-celo-evm` (not `celo-reth`) because it is checked inside
//! `CeloEvm::transact_raw()` — the shared execution path used by both reth and any other
//! consumer of `CeloEvmFactory`. Kona/ZK paths do not need it and pass `blocklist: None`
//! when constructing `CeloEvmFactory::default()`.
//!
//! Blocklisting can be controlled per-currency via admin RPCs:
//! - `admin_disableBlocklistFeeCurrencies`: Prevents a currency from being blocklisted.
//! - `admin_enableBlocklistFeeCurrencies`: Re-enables blocklisting for a currency.
//! - `admin_unblockFeeCurrency`: Removes a currency from the blocklist.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use alloy_primitives::Address;
use spin::Mutex;

/// How long (in seconds) a currency stays blocked before automatic eviction.
const BLOCKLIST_EVICTION_SECONDS: u64 = 7200;

/// Internal state for the fee currency blocklist.
#[derive(Debug, Default)]
struct BlocklistState {
    /// Currencies currently blocked due to execution errors, with the timestamp when blocked.
    blocked: BTreeMap<Address, u64>,
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
        self.inner.lock().blocked.contains_key(&currency)
    }

    /// Adds a currency to the blocklist at the given timestamp
    /// (if blocklisting is not disabled for it).
    pub fn block_currency(&self, currency: Address, timestamp: u64) {
        let mut state = self.inner.lock();
        if !state.blocklist_disabled.contains(&currency) {
            state.blocked.insert(currency, timestamp);
        }
    }

    /// Removes entries older than `BLOCKLIST_EVICTION_SECONDS` (7200s) from the current timestamp.
    pub fn evict(&self, current_timestamp: u64) {
        let mut state = self.inner.lock();
        state.blocked.retain(|_, blocked_at| {
            current_timestamp.saturating_sub(*blocked_at) < BLOCKLIST_EVICTION_SECONDS
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> Address {
        Address::with_last_byte(b)
    }

    #[test]
    fn block_and_check() {
        let bl = FeeCurrencyBlocklist::default();
        assert!(!bl.is_blocked(addr(1)));
        bl.block_currency(addr(1), 1000);
        assert!(bl.is_blocked(addr(1)));
    }

    #[test]
    fn eviction_before_deadline() {
        let bl = FeeCurrencyBlocklist::default();
        bl.block_currency(addr(1), 1000);
        // 7199 seconds later — still within window
        bl.evict(1000 + BLOCKLIST_EVICTION_SECONDS - 1);
        assert!(bl.is_blocked(addr(1)));
    }

    #[test]
    fn eviction_at_deadline() {
        let bl = FeeCurrencyBlocklist::default();
        bl.block_currency(addr(1), 1000);
        // Exactly at the eviction boundary
        bl.evict(1000 + BLOCKLIST_EVICTION_SECONDS);
        assert!(!bl.is_blocked(addr(1)));
    }

    #[test]
    fn eviction_after_deadline() {
        let bl = FeeCurrencyBlocklist::default();
        bl.block_currency(addr(1), 1000);
        bl.evict(1000 + BLOCKLIST_EVICTION_SECONDS + 100);
        assert!(!bl.is_blocked(addr(1)));
    }

    #[test]
    fn eviction_preserves_recent() {
        let bl = FeeCurrencyBlocklist::default();
        bl.block_currency(addr(1), 1000);
        bl.block_currency(addr(2), 5000);
        // Evict at t=8200: addr(1) blocked at 1000 is 7200s old (evicted),
        // addr(2) blocked at 5000 is 3200s old (kept)
        bl.evict(1000 + BLOCKLIST_EVICTION_SECONDS);
        assert!(!bl.is_blocked(addr(1)));
        assert!(bl.is_blocked(addr(2)));
    }

    #[test]
    fn unblock_removes_immediately() {
        let bl = FeeCurrencyBlocklist::default();
        bl.block_currency(addr(1), 1000);
        bl.unblock_currency(addr(1));
        assert!(!bl.is_blocked(addr(1)));
    }

    #[test]
    fn disable_blocklist_prevents_blocking() {
        let bl = FeeCurrencyBlocklist::default();
        bl.disable_blocklist(&[addr(1)]);
        bl.block_currency(addr(1), 1000);
        assert!(!bl.is_blocked(addr(1)));
    }

    #[test]
    fn re_enable_blocklist_allows_blocking() {
        let bl = FeeCurrencyBlocklist::default();
        bl.disable_blocklist(&[addr(1)]);
        bl.enable_blocklist(&[addr(1)]);
        bl.block_currency(addr(1), 1000);
        assert!(bl.is_blocked(addr(1)));
    }

    #[test]
    fn block_after_eviction() {
        let bl = FeeCurrencyBlocklist::default();
        bl.block_currency(addr(1), 1000);
        // Evict at the deadline — currency is no longer blocked
        bl.evict(1000 + BLOCKLIST_EVICTION_SECONDS);
        assert!(!bl.is_blocked(addr(1)));
        // Re-add after eviction — should be blocked again
        bl.block_currency(addr(1), 9000);
        assert!(bl.is_blocked(addr(1)));
    }

    #[test]
    fn block_after_unblock() {
        let bl = FeeCurrencyBlocklist::default();
        bl.block_currency(addr(1), 1000);
        bl.unblock_currency(addr(1));
        assert!(!bl.is_blocked(addr(1)));
        // Re-add after manual unblock — should be blocked again
        bl.block_currency(addr(1), 2000);
        assert!(bl.is_blocked(addr(1)));
    }
}
