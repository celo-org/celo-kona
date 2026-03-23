//! Celo fee-currency-aware payload transaction filtering.
//!
//! Implements block space limits per fee abstraction token as specified at
//! <https://specs.celo.org/fee_abstraction.html#block-space-limits-per-fee-abstraction-token>.
//!
//! Each non-native fee currency is limited to a configurable fraction of the block gas limit.
//! Native CELO transactions are unrestricted.

use crate::pool::CeloPoolTx;
use alloy_celo_evm::blocklist::FeeCurrencyBlocklist;
use alloy_consensus::Transaction;
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
}

impl Default for FeeCurrencyLimits {
    fn default() -> Self {
        Self { limits: HashMap::new(), default_limit: 0.5 }
    }
}

impl FeeCurrencyLimits {
    /// Returns the default per-currency gas limit fractions for Celo Mainnet.
    ///
    /// Matches op-geth's `miner/celo_defaults.go`:
    /// - cUSD, USDT, USDC: 0.9 (high-confidence stablecoins)
    /// - cEUR, cREAL: 0.5 (default)
    pub fn mainnet_defaults() -> HashMap<Address, f64> {
        use alloy_primitives::address;
        HashMap::from([
            (address!("765DE816845861e75A25fCA122bb6898B8B1282a"), 0.9), // cUSD
            (address!("48065fbbe25f71c9282ddf5e1cd6d6a887483d5e"), 0.9), // USDT
            (address!("cebA9300f2b948710d2653dD7B07f33A8B32118C"), 0.9), // USDC
            (address!("D8763CBa276a3738E6DE85b4b3bF5FDed6D6cA73"), 0.5), // cEUR
            (address!("e8537a3d056DA446677B9E9d6c5dB704EaAb4787"), 0.5), // cREAL
        ])
    }

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
                if let (Ok(addr), Ok(frac)) =
                    (addr_str.trim().parse::<Address>(), frac_str.trim().parse::<f64>())
                {
                    map.insert(addr, frac);
                }
            }
        }
        map
    }

    /// Returns the gas limit for a given fee currency address.
    /// Returns `None` (unlimited) for native CELO.
    fn max_gas_for_currency(
        &self,
        fee_currency: Option<Address>,
        block_gas_limit: u64,
    ) -> Option<u64> {
        let fc = fee_currency?;
        let fraction = self.limits.get(&fc).copied().unwrap_or(self.default_limit);
        Some((block_gas_limit as f64 * fraction) as u64)
    }
}

// ---------------------------------------------------------------------------
// CeloPayloadTransactions — OpPayloadTransactions impl
// ---------------------------------------------------------------------------

/// Implements [`OpPayloadTransactions`] for Celo, wrapping the pool's best
/// transactions with per-fee-currency gas limit filtering.
///
/// **Note:** These per-currency gas limits only apply during sequencing (block
/// building from the pool). During derivation, `ConfigureEngineEvm::tx_iterator_for_payload`
/// in `lib.rs` bypasses `CeloPayloadTransactions` entirely, iterating over
/// the L2 block's pre-determined transaction list without any per-currency limits.
#[derive(Debug, Clone)]
pub struct CeloPayloadTransactions {
    limits: FeeCurrencyLimits,
    blocklist: FeeCurrencyBlocklist,
}

impl CeloPayloadTransactions {
    /// Create a new instance with the given fee currency limits and blocklist.
    pub const fn new(limits: FeeCurrencyLimits, blocklist: FeeCurrencyBlocklist) -> Self {
        Self { limits, blocklist }
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
        let block_gas_limit = pool.block_info().block_gas_limit;
        CeloFeeCurrencyFilter {
            inner: BestPayloadTransactions::new(pool.best_transactions_with_attributes(attr)),
            limits: self.limits.clone(),
            blocklist: self.blocklist.clone(),
            block_gas_limit,
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
    blocklist: FeeCurrencyBlocklist,
    /// Block gas limit from the pool, used to compute per-currency gas caps.
    block_gas_limit: u64,
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

            // Check blocklist before gas limits
            if let Some(fc) = fee_currency {
                if self.blocklist.is_blocked(fc) {
                    tracing::debug!(
                        target: "celo::payload",
                        ?fc,
                        "Skipping tx: fee currency is blocklisted"
                    );
                    self.inner.mark_invalid(tx.sender(), tx.nonce());
                    continue;
                }
            }

            if let Some(max_gas) =
                self.limits.max_gas_for_currency(fee_currency, self.block_gas_limit)
            {
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
            "0x765DE816845861e75A25fCA122bb6898B8B1282a=0.9,0xD8763CBa276a3738E6DE85b4b3bF5FDed6D6cA73=0.5",
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
    fn test_parse_limits_invalid_address() {
        let limits = FeeCurrencyLimits::parse_limits("0xDEAD=0.5");
        assert!(limits.is_empty(), "Too-short address should be rejected");
    }

    #[test]
    fn test_parse_limits_invalid_fraction() {
        let limits = FeeCurrencyLimits::parse_limits(
            "0x765DE816845861e75A25fCA122bb6898B8B1282a=notanumber",
        );
        assert!(limits.is_empty(), "Non-numeric fraction should be rejected");
    }

    #[test]
    fn test_parse_limits_mixed_valid_invalid() {
        let limits = FeeCurrencyLimits::parse_limits(
            "0x765DE816845861e75A25fCA122bb6898B8B1282a=0.9,0xINVALID=0.5",
        );
        assert_eq!(limits.len(), 1, "Only valid entry should be kept");
        assert_eq!(
            limits[&"0x765DE816845861e75A25fCA122bb6898B8B1282a".parse::<Address>().unwrap()],
            0.9
        );
    }

    #[test]
    fn test_parse_limits_trailing_comma() {
        let limits =
            FeeCurrencyLimits::parse_limits("0x765DE816845861e75A25fCA122bb6898B8B1282a=0.9,");
        assert_eq!(limits.len(), 1, "Trailing comma should not cause error");
    }

    #[test]
    fn test_parse_limits_extra_whitespace() {
        let limits = FeeCurrencyLimits::parse_limits(
            " 0x765DE816845861e75A25fCA122bb6898B8B1282a = 0.9 , 0xD8763CBa276a3738E6DE85b4b3bF5FDed6D6cA73 = 0.5 ",
        );
        assert_eq!(limits.len(), 2, "Extra whitespace should be handled");
    }

    #[test]
    fn test_parse_limits_no_equals_sign() {
        let limits = FeeCurrencyLimits::parse_limits("0x765DE816845861e75A25fCA122bb6898B8B1282a");
        assert!(limits.is_empty(), "Entry without = should be ignored");
    }

    #[test]
    fn test_mainnet_defaults() {
        let defaults = FeeCurrencyLimits::mainnet_defaults();
        assert_eq!(defaults.len(), 5);
        // cUSD, USDT, USDC = 0.9
        assert_eq!(
            defaults[&"0x765DE816845861e75A25fCA122bb6898B8B1282a".parse::<Address>().unwrap()],
            0.9,
            "cUSD should be 0.9"
        );
        assert_eq!(
            defaults[&"0x48065fbbe25f71c9282ddf5e1cd6d6a887483d5e".parse::<Address>().unwrap()],
            0.9,
            "USDT should be 0.9"
        );
        assert_eq!(
            defaults[&"0xcebA9300f2b948710d2653dD7B07f33A8B32118C".parse::<Address>().unwrap()],
            0.9,
            "USDC should be 0.9"
        );
        // cEUR, cREAL = 0.5
        assert_eq!(
            defaults[&"0xD8763CBa276a3738E6DE85b4b3bF5FDed6D6cA73".parse::<Address>().unwrap()],
            0.5,
            "cEUR should be 0.5"
        );
        assert_eq!(
            defaults[&"0xe8537a3d056DA446677B9E9d6c5dB704EaAb4787".parse::<Address>().unwrap()],
            0.5,
            "cREAL should be 0.5"
        );
    }

    #[test]
    fn test_max_gas_for_currency_native() {
        let limits = FeeCurrencyLimits::default();
        // Native CELO (None) should be unlimited
        assert_eq!(limits.max_gas_for_currency(None, 30_000_000), None);
    }

    #[test]
    fn test_max_gas_for_currency_default() {
        let limits = FeeCurrencyLimits { default_limit: 0.5, ..Default::default() };
        let addr: Address = "0x765DE816845861e75A25fCA122bb6898B8B1282a".parse().unwrap();
        // Default 0.5 of 30M = 15M
        assert_eq!(limits.max_gas_for_currency(Some(addr), 30_000_000), Some(15_000_000));
    }

    #[test]
    fn test_max_gas_for_currency_specific() {
        let addr: Address = "0x765DE816845861e75A25fCA122bb6898B8B1282a".parse().unwrap();
        let mut map = HashMap::new();
        map.insert(addr, 0.9);
        let limits = FeeCurrencyLimits { limits: map, default_limit: 0.5, ..Default::default() };
        // 0.9 of 30M = 27M
        assert_eq!(limits.max_gas_for_currency(Some(addr), 30_000_000), Some(27_000_000));
    }

    // -----------------------------------------------------------------------
    // CeloFeeCurrencyFilter tests
    // -----------------------------------------------------------------------

    use crate::pool::CeloPoolTx;
    use reth_transaction_pool::PoolTransaction;

    /// Create a test CeloPoolTx with default fee values (1 Gwei fee cap, 100 wei tip).
    fn make_test_tx(fee_currency: Option<Address>, gas_limit: u64, sender: Address) -> CeloPoolTx {
        crate::test_utils::make_test_tx(fee_currency, gas_limit, 1_000_000_000, 100, sender)
    }

    /// A simple PayloadTransactions implementation backed by a Vec.
    struct VecPayloadTransactions {
        txs: Vec<CeloPoolTx>,
        invalid: Vec<(Address, u64)>,
    }

    impl PayloadTransactions for VecPayloadTransactions {
        type Transaction = CeloPoolTx;

        fn next(&mut self, _ctx: ()) -> Option<Self::Transaction> {
            if self.txs.is_empty() { None } else { Some(self.txs.remove(0)) }
        }

        fn mark_invalid(&mut self, sender: Address, nonce: u64) {
            self.invalid.push((sender, nonce));
            self.txs.retain(|tx| tx.sender() != sender);
        }
    }

    fn fc_addr(b: u8) -> Address {
        Address::with_last_byte(b)
    }

    #[test]
    fn filter_passes_native_celo_tx() {
        let sender = Address::with_last_byte(1);
        let mut filter = CeloFeeCurrencyFilter {
            inner: VecPayloadTransactions {
                txs: vec![make_test_tx(None, 21_000, sender)],
                invalid: vec![],
            },
            limits: FeeCurrencyLimits::default(),
            blocklist: FeeCurrencyBlocklist::default(),
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
        };

        assert!(filter.next(()).is_some());
        assert!(filter.next(()).is_none());
    }

    #[test]
    fn filter_skips_when_gas_limit_exceeded() {
        let sender = Address::with_last_byte(1);
        let fc = fc_addr(10);
        // Limit = 0.5 * 30M = 15M. Tx with 16M gas should be skipped.
        let mut filter = CeloFeeCurrencyFilter {
            inner: VecPayloadTransactions {
                txs: vec![make_test_tx(Some(fc), 16_000_000, sender)],
                invalid: vec![],
            },
            limits: FeeCurrencyLimits::default(),
            blocklist: FeeCurrencyBlocklist::default(),
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
        };

        assert!(filter.next(()).is_none());
    }

    #[test]
    fn filter_tracks_gas_per_currency_independently() {
        let sender_a = Address::with_last_byte(1);
        let sender_b = Address::with_last_byte(2);
        let fc_a = fc_addr(10);
        let fc_b = fc_addr(11);
        // Each currency can use 15M (0.5 * 30M)
        let mut filter = CeloFeeCurrencyFilter {
            inner: VecPayloadTransactions {
                txs: vec![
                    make_test_tx(Some(fc_a), 10_000_000, sender_a),
                    make_test_tx(Some(fc_b), 10_000_000, sender_b),
                    // This should be skipped: fc_a would be at 20M > 15M
                    make_test_tx(Some(fc_a), 10_000_000, sender_a),
                ],
                invalid: vec![],
            },
            limits: FeeCurrencyLimits::default(),
            blocklist: FeeCurrencyBlocklist::default(),
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
        };

        // First two should pass (different currencies)
        let tx1 = filter.next(()).unwrap();
        assert_eq!(tx1.fee_currency(), Some(fc_a));
        let tx2 = filter.next(()).unwrap();
        assert_eq!(tx2.fee_currency(), Some(fc_b));
        // Third should be skipped (fc_a exceeded)
        assert!(filter.next(()).is_none());
    }

    #[test]
    fn filter_skips_blocklisted_currency() {
        let sender = Address::with_last_byte(1);
        let fc = fc_addr(10);
        let blocklist = FeeCurrencyBlocklist::default();
        blocklist.block_currency(fc, 1000);

        let mut filter = CeloFeeCurrencyFilter {
            inner: VecPayloadTransactions {
                txs: vec![make_test_tx(Some(fc), 21_000, sender)],
                invalid: vec![],
            },
            limits: FeeCurrencyLimits::default(),
            blocklist,
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
        };

        assert!(filter.next(()).is_none());
    }

    #[test]
    fn filter_exactly_at_gas_limit_passes() {
        // tx uses exactly 15M gas = 0.5 * 30M → condition is used + gas > max, so equal passes
        let sender = Address::with_last_byte(1);
        let fc = fc_addr(10);
        let mut filter = CeloFeeCurrencyFilter {
            inner: VecPayloadTransactions {
                txs: vec![make_test_tx(Some(fc), 15_000_000, sender)],
                invalid: vec![],
            },
            limits: FeeCurrencyLimits::default(), // max = 0.5 * 30M = 15M
            blocklist: FeeCurrencyBlocklist::default(),
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
        };

        assert!(filter.next(()).is_some(), "Tx using exactly the gas limit should pass");
        assert!(filter.next(()).is_none());
    }

    #[test]
    fn filter_two_senders_same_currency_second_skipped() {
        // sender_a sends 10M gas, sender_b sends 8M gas, both using the same fc.
        // After sender_a's tx: used=10M. sender_b's 8M would bring total to 18M > 15M → skipped.
        let sender_a = Address::with_last_byte(1);
        let sender_b = Address::with_last_byte(2);
        let fc = fc_addr(10);
        let mut filter = CeloFeeCurrencyFilter {
            inner: VecPayloadTransactions {
                txs: vec![
                    make_test_tx(Some(fc), 10_000_000, sender_a),
                    make_test_tx(Some(fc), 8_000_000, sender_b),
                ],
                invalid: vec![],
            },
            limits: FeeCurrencyLimits::default(), // max = 15M
            blocklist: FeeCurrencyBlocklist::default(),
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
        };

        let tx1 = filter.next(()).expect("sender_a tx should pass");
        assert_eq!(tx1.fee_currency(), Some(fc));
        // sender_b's tx is skipped (cumulative 10M + 8M = 18M > 15M)
        assert!(filter.next(()).is_none(), "sender_b tx should be skipped");
    }

    #[test]
    fn filter_passes_after_blocklist_eviction() {
        let sender = Address::with_last_byte(1);
        let fc = fc_addr(10);
        let blocklist = FeeCurrencyBlocklist::default();

        // Block the currency at timestamp 1000
        blocklist.block_currency(fc, 1000);
        assert!(blocklist.is_blocked(fc));

        // Evict stale entries at timestamp 8201 (> 1000 + 7200 TTL)
        blocklist.evict(8201);
        assert!(!blocklist.is_blocked(fc), "Currency should be unblocked after TTL eviction");

        // Now the filter should pass the tx through
        let mut filter = CeloFeeCurrencyFilter {
            inner: VecPayloadTransactions {
                txs: vec![make_test_tx(Some(fc), 21_000, sender)],
                invalid: vec![],
            },
            limits: FeeCurrencyLimits::default(),
            blocklist,
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
        };

        assert!(filter.next(()).is_some(), "Tx should pass after blocklist eviction");
    }
}
