//! Celo fee-currency-aware payload transaction filtering.
//!
//! Implements block space limits per fee abstraction token as specified at
//! <https://specs.celo.org/fee_abstraction.html#block-space-limits-per-fee-abstraction-token>.
//!
//! Each non-native fee currency is limited to a configurable fraction of the block gas limit.
//! Native CELO transactions are unrestricted.

use alloy_consensus::Transaction;
use crate::pool::CeloPoolTx;
use alloy_primitives::Address;
use reth_optimism_payload_builder::builder::OpPayloadTransactions;
use reth_payload_util::{BestPayloadTransactions, PayloadTransactions};
use reth_transaction_pool::{PoolTransaction, TransactionPool};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// FeeCurrencyLimits — parsed CLI configuration
// ---------------------------------------------------------------------------

/// Per-fee-currency block space limits.
///
/// Each entry maps a fee currency address to the maximum fraction of block gas
/// it may consume. Currencies not in the map use the `default_limit`.
/// Native CELO (fee_currency = None) is always unlimited.
#[derive(Debug, Clone)]
pub struct FeeCurrencyLimits {
    /// Per-currency gas fraction limits (0.0–1.0).
    pub limits: HashMap<Address, f64>,
    /// Default limit for currencies not explicitly listed.
    pub default_limit: f64,
    /// Block gas limit used for computing per-currency gas caps.
    /// Defaults to 30_000_000 (Celo's typical block gas limit).
    pub block_gas_limit: u64,
}

impl Default for FeeCurrencyLimits {
    fn default() -> Self {
        Self {
            limits: HashMap::new(),
            default_limit: 0.5,
            block_gas_limit: 30_000_000,
        }
    }
}

impl FeeCurrencyLimits {
    /// Parse the `--celo.feecurrency.limits` CLI value.
    ///
    /// Format: `address=fraction,address=fraction,...`
    /// Addresses are not expected to be checksummed.
    pub fn parse_limits(s: &str) -> HashMap<Address, f64> {
        let mut map = HashMap::new();
        for pair in s.split(',') {
            let pair = pair.trim();
            if pair.is_empty() {
                continue;
            }
            if let Some((addr_str, frac_str)) = pair.split_once('=') {
                if let (Ok(addr), Ok(frac)) = (addr_str.trim().parse::<Address>(), frac_str.trim().parse::<f64>()) {
                    map.insert(addr, frac);
                }
            }
        }
        map
    }

    /// Returns the gas limit for a given fee currency address.
    /// Returns `None` (unlimited) for native CELO.
    fn max_gas_for_currency(&self, fee_currency: Option<Address>) -> Option<u64> {
        let fc = fee_currency?;
        let fraction = self.limits.get(&fc).copied().unwrap_or(self.default_limit);
        Some((self.block_gas_limit as f64 * fraction) as u64)
    }
}

// ---------------------------------------------------------------------------
// CeloPayloadTransactions — OpPayloadTransactions impl
// ---------------------------------------------------------------------------

/// Implements [`OpPayloadTransactions`] for Celo, wrapping the pool's best
/// transactions with per-fee-currency gas limit filtering.
#[derive(Debug, Clone)]
pub struct CeloPayloadTransactions {
    limits: FeeCurrencyLimits,
}

impl CeloPayloadTransactions {
    /// Create a new instance with the given fee currency limits.
    pub const fn new(limits: FeeCurrencyLimits) -> Self {
        Self { limits }
    }
}

impl OpPayloadTransactions<CeloPoolTx> for CeloPayloadTransactions {
    fn best_transactions<Pool>(
        &self,
        pool: Pool,
        attr: reth_transaction_pool::BestTransactionsAttributes,
    ) -> impl PayloadTransactions<Transaction = CeloPoolTx>
    where
        Pool: TransactionPool<Transaction = CeloPoolTx>,
    {
        CeloFeeCurrencyFilter {
            inner: BestPayloadTransactions::new(pool.best_transactions_with_attributes(attr)),
            limits: self.limits.clone(),
            gas_used_per_currency: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// CeloFeeCurrencyFilter — PayloadTransactions wrapper
// ---------------------------------------------------------------------------

/// Wraps a [`PayloadTransactions`] iterator and enforces per-fee-currency gas limits.
///
/// Transactions whose fee currency has exceeded its allotted fraction of block gas
/// are skipped (and their sender marked invalid). Native CELO transactions pass through
/// without any limit.
#[derive(Debug)]
struct CeloFeeCurrencyFilter<I> {
    inner: I,
    limits: FeeCurrencyLimits,
    /// Cumulative gas used per fee currency address.
    gas_used_per_currency: HashMap<Address, u64>,
}

impl<I> PayloadTransactions for CeloFeeCurrencyFilter<I>
where
    I: PayloadTransactions<Transaction = CeloPoolTx>,
{
    type Transaction = CeloPoolTx;

    fn next(&mut self, ctx: ()) -> Option<Self::Transaction> {
        loop {
            let tx = self.inner.next(ctx)?;
            let fee_currency = tx.fee_currency();

            if let Some(max_gas) = self.limits.max_gas_for_currency(fee_currency) {
                let fc = fee_currency.unwrap(); // safe: max_gas is Some only when fee_currency is Some
                let used = self.gas_used_per_currency.get(&fc).copied().unwrap_or(0);
                if used + tx.gas_limit() > max_gas {
                    // This fee currency has exceeded its block space limit.
                    // Skip this tx and all descendants from the same sender.
                    tracing::debug!(
                        target: "celo::payload",
                        ?fc,
                        gas_used = used,
                        tx_gas = tx.gas_limit(),
                        max_gas,
                        "Skipping tx: fee currency block space limit exceeded"
                    );
                    self.inner.mark_invalid(tx.sender(), tx.nonce());
                    continue;
                }
                // Track gas usage for this currency
                *self.gas_used_per_currency.entry(fc).or_insert(0) += tx.gas_limit();
            }
            // Native CELO (fee_currency = None): no limit applied

            return Some(tx);
        }
    }

    fn mark_invalid(&mut self, sender: Address, nonce: u64) {
        self.inner.mark_invalid(sender, nonce);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_limits() {
        let limits = FeeCurrencyLimits::parse_limits(
            "0x765DE816845861e75A25fCA122bb6898B8B1282a=0.9,0xD8763CBa276a3738E6DE85b4b3bF5FDed6D6cA73=0.5"
        );
        assert_eq!(limits.len(), 2);
        assert_eq!(
            limits[&"0x765DE816845861e75A25fCA122bb6898B8B1282a".parse::<Address>().unwrap()],
            0.9
        );
        assert_eq!(
            limits[&"0xD8763CBa276a3738E6DE85b4b3bF5FDed6D6cA73".parse::<Address>().unwrap()],
            0.5
        );
    }

    #[test]
    fn test_parse_limits_empty() {
        let limits = FeeCurrencyLimits::parse_limits("");
        assert!(limits.is_empty());
    }

    #[test]
    fn test_max_gas_for_currency_native() {
        let limits = FeeCurrencyLimits::default();
        // Native CELO (None) should be unlimited
        assert_eq!(limits.max_gas_for_currency(None), None);
    }

    #[test]
    fn test_max_gas_for_currency_default() {
        let limits = FeeCurrencyLimits { default_limit: 0.5, ..Default::default() };
        let addr: Address = "0x765DE816845861e75A25fCA122bb6898B8B1282a".parse().unwrap();
        // Default 0.5 of 30M = 15M
        assert_eq!(limits.max_gas_for_currency(Some(addr)), Some(15_000_000));
    }

    #[test]
    fn test_max_gas_for_currency_specific() {
        let addr: Address = "0x765DE816845861e75A25fCA122bb6898B8B1282a".parse().unwrap();
        let mut map = HashMap::new();
        map.insert(addr, 0.9);
        let limits = FeeCurrencyLimits { limits: map, default_limit: 0.5, ..Default::default() };
        // 0.9 of 30M = 27M
        assert_eq!(limits.max_gas_for_currency(Some(addr)), Some(27_000_000));
    }
}
