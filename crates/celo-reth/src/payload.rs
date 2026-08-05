//! Celo fee-currency-aware payload transaction filtering.
//!
//! Implements block space limits per fee abstraction token as specified at
//! <https://specs.celo.org/fee_abstraction.html#block-space-limits-per-fee-abstraction-token>.
//!
//! Each non-native fee currency is limited to a configurable fraction of the block gas limit.
//! Native CELO transactions are unrestricted.

use crate::pool::CeloPoolTx;
use alloy_celo_evm::CeloFailurePolicies;
use alloy_consensus::Transaction;
use alloy_primitives::{Address, B256};
use reth_optimism_payload_builder::builder::OpPayloadTransactions;
use reth_payload_util::{BestPayloadTransactions, PayloadTransactions};
use reth_transaction_pool::{PoolTransaction, TransactionPool};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// FeeCurrencyLimits — parsed CLI configuration
// ---------------------------------------------------------------------------

/// Default fraction of block gas allowed for fee currencies that are not
/// explicitly configured. Matches op-geth's `DefaultFeeCurrencyLimit`.
pub const DEFAULT_FEE_CURRENCY_LIMIT_FRACTION: f64 = 0.5;

/// Parse and validate a fee-currency block-space fraction (the value of
/// `--celo.feecurrency.default`).
///
/// Rejects non-finite and out-of-range values at parse time. The field is a plain `f64`, and
/// the downstream cap is computed as `(block_gas_limit as f64 * fraction) as u64`. `clamp`
/// would let `NaN` through and `NaN as u64 == 0`, silently zeroing the default cap so the
/// sequencer drops every fee-currency tx not named in `--celo.feecurrency.limits`.
/// `(0.0..=1.0).contains` is `false` for `NaN` (and infinities), so this rejects them.
pub fn parse_fee_currency_fraction(s: &str) -> Result<f64, String> {
    let value: f64 = s.parse().map_err(|_| format!("`{s}` is not a valid number"))?;
    if (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(format!("fee currency fraction must be in [0.0, 1.0], got `{s}`"))
    }
}

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
        Self { limits: HashMap::new(), default_limit: DEFAULT_FEE_CURRENCY_LIMIT_FRACTION }
    }
}

impl FeeCurrencyLimits {
    /// Returns the built-in per-currency gas limit defaults for the given chain.
    ///
    /// Keys must be addresses the chain's `FeeCurrencyDirectory` registers, since that is what a
    /// CIP-64 transaction carries in `feeCurrency`. USDT and USDC are 6-decimal and Celo prices
    /// gas in 18 decimals, so the directory registers a scaling *adapter* for each and the token
    /// address itself is never a valid `feeCurrency`. The cStables are 18-decimal and are
    /// registered directly. The fractions match op-geth's `miner/celo_defaults.go`.
    ///
    /// Other chains get an empty map and fall back to `default_limit` for every currency.
    pub fn defaults_for_chain(chain_id: u64) -> HashMap<Address, f64> {
        use alloy_primitives::address;
        match chain_id {
            celo_revm::constants::CELO_MAINNET_CHAIN_ID => HashMap::from([
                (address!("765DE816845861e75A25fCA122bb6898B8B1282a"), 0.9), // cUSD
                (address!("0E2A3e05bc9A16F5292A6170456A710cb89C6f72"), 0.9), // USDT adapter
                (address!("2F25deB3848C207fc8E0c34035B3Ba7fC157602B"), 0.9), // USDC adapter
                (address!("D8763CBa276a3738E6DE85b4b3bF5FDed6D6cA73"), 0.5), // cEUR
                (address!("e8537a3d056DA446677B9E9d6c5dB704EaAb4787"), 0.5), // cREAL
            ]),
            celo_revm::constants::CELO_SEPOLIA_CHAIN_ID => HashMap::from([
                (address!("EF4d55D6dE8e8d73232827Cd1e9b2F2dBb45bC80"), 0.9), // cUSD
                (address!("e19447B12cb0d0220B2a501D8382be2f61CcF92a"), 0.9), // USDT
                (address!("bf1441Ea57f43f35f713431001f35742c88071c7"), 0.9), // USDC
                (address!("6B172e333e2978484261D7eCC3DE491E79764BbC"), 0.5), // cEUR
                (address!("13d68A1Bf4a8cB7d9feF54EF70401871b666269c"), 0.5), // cREAL
            ]),
            _ => HashMap::new(),
        }
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
            if let Some((addr_str, frac_str)) = pair.split_once('=') &&
                let (Ok(addr), Ok(frac)) =
                    (addr_str.trim().parse::<Address>(), frac_str.trim().parse::<f64>()) &&
                (0.0..=1.0).contains(&frac)
            {
                map.insert(addr, frac);
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
        let fraction = self.limits.get(&fc).copied().unwrap_or(self.default_limit).clamp(0.0, 1.0);
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
    failure_policies: CeloFailurePolicies,
}

impl CeloPayloadTransactions {
    /// Creates an instance with fee currency limits and shared sequencing failure policies.
    pub const fn new(limits: FeeCurrencyLimits, failure_policies: CeloFailurePolicies) -> Self {
        Self { limits, failure_policies }
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
        // Do not clear revert markers here. Reth can run multiple payload jobs concurrently, so
        // one iterator must not erase another job's marker before its inline `mark_invalid` call.
        // Evict stale blocklist entries before filtering. Otherwise transactions using an expired
        // entry would continue to be rejected by `CeloFeeCurrencyFilter` below even past the 7200s
        // TTL. Wall clock is a safe time source here: block timestamps track wall time within
        // seconds and the blocklist is a best-effort sequencing heuristic, not consensus state.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.failure_policies.blocklist().evict(now);

        let block_gas_limit = pool.block_info().block_gas_limit;
        let inner = BestPayloadTransactions::new(pool.best_transactions_with_attributes(attr));
        CeloFeeCurrencyFilter {
            inner,
            pool,
            limits: self.limits.clone(),
            failure_policies: self.failure_policies.clone(),
            block_gas_limit,
            gas_used_per_currency: HashMap::new(),
            pending_charge: None,
        }
    }
}

// ---------------------------------------------------------------------------
// CeloFeeCurrencyFilter — PayloadTransactions wrapper
// ---------------------------------------------------------------------------

/// Gas reservation for the most recently yielded fee-currency transaction.
#[derive(Debug, Clone, Copy)]
struct PendingFeeCurrencyCharge {
    sender: Address,
    nonce: u64,
    tx_hash: B256,
    fee_currency: Address,
    gas_limit: u64,
}

/// Wraps a [`PayloadTransactions`] iterator and enforces per-fee-currency gas limits.
///
/// Transactions whose fee currency has exceeded its allotted fraction of block gas
/// are skipped (and their sender marked invalid). Native CELO transactions pass through
/// without any limit.
#[derive(Debug)]
struct CeloFeeCurrencyFilter<I, Pool> {
    inner: I,
    pool: Pool,
    limits: FeeCurrencyLimits,
    failure_policies: CeloFailurePolicies,
    /// Block gas limit from the pool, used to compute per-currency gas caps.
    block_gas_limit: u64,
    /// Cumulative gas used per fee currency address.
    gas_used_per_currency: HashMap<Address, u64>,
    /// Gas reservation for the most recently yielded fee-currency transaction. A matching
    /// `mark_invalid` rolls it back; otherwise a following `next` call leaves it committed.
    pending_charge: Option<PendingFeeCurrencyCharge>,
}

impl<I, Pool> PayloadTransactions for CeloFeeCurrencyFilter<I, Pool>
where
    I: PayloadTransactions<Transaction = CeloPoolTx>,
    Pool: TransactionPool<Transaction = CeloPoolTx>,
{
    type Transaction = CeloPoolTx;

    fn next(&mut self, ctx: ()) -> Option<Self::Transaction> {
        // Continuing iteration drops the rollback handle and leaves the reservation counted. This
        // normally follows inclusion, but upstream's nonce-too-low path also continues without
        // calling mark_invalid, leaving a conservative reservation for this payload attempt.
        self.pending_charge = None;

        loop {
            let tx = self.inner.next(ctx)?;
            let fee_currency = tx.fee_currency();

            // Check blocklist before gas limits
            if let Some(fc) = fee_currency &&
                self.failure_policies.blocklist().is_blocked(fc)
            {
                tracing::debug!(
                    target: "celo::payload",
                    ?fc,
                    "Skipping tx: fee currency is blocklisted"
                );
                metrics::counter!("celo_payload_skipped_total", "reason" => "blocklisted")
                    .increment(1);
                self.inner.mark_invalid(tx.sender(), tx.nonce());
                continue;
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
                    metrics::counter!("celo_payload_skipped_total", "reason" => "gas_limit_exceeded")
                        .increment(1);
                    self.inner.mark_invalid(tx.sender(), tx.nonce());
                    continue;
                }
                // Track gas usage for this currency
                *self.gas_used_per_currency.entry(fc).or_insert(0) += tx.gas_limit();
                self.pending_charge = Some(PendingFeeCurrencyCharge {
                    sender: tx.sender(),
                    nonce: tx.nonce(),
                    tx_hash: *tx.hash(),
                    fee_currency: fc,
                    gas_limit: tx.gas_limit(),
                });
            }
            // Native CELO (fee_currency = None): no limit applied

            return Some(tx);
        }
    }

    fn mark_invalid(&mut self, sender: Address, nonce: u64) {
        let charge =
            self.pending_charge.take_if(|charge| charge.sender == sender && charge.nonce == nonce);

        if let Some(charge) = charge {
            if let std::collections::hash_map::Entry::Occupied(mut entry) =
                self.gas_used_per_currency.entry(charge.fee_currency)
            {
                let remaining = entry.get().saturating_sub(charge.gas_limit);
                if remaining == 0 {
                    entry.remove();
                } else {
                    *entry.get_mut() = remaining;
                }
            }

            self.inner.mark_invalid(sender, nonce);

            if self.failure_policies.revert_evictions().take(charge.tx_hash) {
                let removed = self.pool.remove_transactions_and_descendants(vec![charge.tx_hash]);
                if !removed.is_empty() {
                    metrics::counter!(
                        "celo_pool_evictions_total",
                        "reason" => "debit_credit_reverted"
                    )
                    .increment(removed.len() as u64);
                    tracing::info!(
                        target: "celo::pool",
                        tx_hash = ?charge.tx_hash,
                        removed = removed.len(),
                        "Evicted reverted CIP-64 transaction and descendants"
                    );
                }
            }
        } else {
            self.inner.mark_invalid(sender, nonce);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_celo_evm::{blocklist::FeeCurrencyBlocklist, revert_evictions::RevertEvictions};
    use alloy_primitives::{U256, address};

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
    fn test_parse_limits_out_of_range_fractions() {
        let limits = FeeCurrencyLimits::parse_limits(
            "0x765DE816845861e75A25fCA122bb6898B8B1282a=-0.1,0xD8763CBa276a3738E6DE85b4b3bF5FDed6D6cA73=1.5",
        );
        assert!(limits.is_empty(), "Fractions outside 0.0..=1.0 should be rejected");
    }

    #[test]
    fn test_parse_limits_boundary_fractions_accepted() {
        let limits = FeeCurrencyLimits::parse_limits(
            "0x765DE816845861e75A25fCA122bb6898B8B1282a=0.0,0xD8763CBa276a3738E6DE85b4b3bF5FDed6D6cA73=1.0",
        );
        assert_eq!(limits.len(), 2, "0.0 and 1.0 should both be accepted");
    }

    #[test]
    fn parse_fraction_accepts_unit_interval() {
        assert_eq!(parse_fee_currency_fraction("0.0").unwrap(), 0.0);
        assert_eq!(parse_fee_currency_fraction("0.5").unwrap(), 0.5);
        assert_eq!(parse_fee_currency_fraction("1.0").unwrap(), 1.0);
    }

    #[test]
    fn parse_fraction_rejects_nan_inf_and_out_of_range() {
        assert!(parse_fee_currency_fraction("NaN").is_err(), "NaN must be rejected");
        assert!(parse_fee_currency_fraction("inf").is_err(), "infinity must be rejected");
        assert!(parse_fee_currency_fraction("1.5").is_err(), "> 1.0 must be rejected");
        assert!(parse_fee_currency_fraction("-0.1").is_err(), "< 0.0 must be rejected");
        assert!(parse_fee_currency_fraction("abc").is_err(), "non-numeric must be rejected");
    }

    #[test]
    fn test_defaults_for_unknown_chain_is_empty() {
        let defaults = FeeCurrencyLimits::defaults_for_chain(0xdead_beef);
        assert!(defaults.is_empty(), "Unknown chains should fall back to default_limit");
    }

    /// Asserts `defaults_for_chain` returns exactly `expected`, address by address.
    fn assert_chain_defaults(chain_id: u64, expected: &[(Address, f64)]) {
        let defaults = FeeCurrencyLimits::defaults_for_chain(chain_id);
        assert_eq!(defaults.len(), expected.len(), "chain {chain_id}: wrong number of defaults");
        for (addr, fraction) in expected {
            assert_eq!(
                defaults.get(addr),
                Some(fraction),
                "chain {chain_id}: wrong limit for {addr}"
            );
        }
    }

    #[test]
    fn test_celo_sepolia_defaults() {
        assert_chain_defaults(
            celo_revm::constants::CELO_SEPOLIA_CHAIN_ID,
            &[
                (address!("EF4d55D6dE8e8d73232827Cd1e9b2F2dBb45bC80"), 0.9), // cUSD
                (address!("e19447B12cb0d0220B2a501D8382be2f61CcF92a"), 0.9), // USDT
                (address!("bf1441Ea57f43f35f713431001f35742c88071c7"), 0.9), // USDC
                (address!("6B172e333e2978484261D7eCC3DE491E79764BbC"), 0.5), // cEUR
                (address!("13d68A1Bf4a8cB7d9feF54EF70401871b666269c"), 0.5), // cREAL
            ],
        );
    }

    /// Mainnet USDT and USDC are reachable only through their `FeeCurrencyDirectory` adapters,
    /// so those are the addresses that must carry the 0.9 fraction.
    #[test]
    fn test_celo_mainnet_defaults() {
        assert_chain_defaults(
            celo_revm::constants::CELO_MAINNET_CHAIN_ID,
            &[
                (address!("765DE816845861e75A25fCA122bb6898B8B1282a"), 0.9), // cUSD
                (address!("0E2A3e05bc9A16F5292A6170456A710cb89C6f72"), 0.9), // USDT adapter
                (address!("2F25deB3848C207fc8E0c34035B3Ba7fC157602B"), 0.9), // USDC adapter
                (address!("D8763CBa276a3738E6DE85b4b3bF5FDed6D6cA73"), 0.5), // cEUR
                (address!("e8537a3d056DA446677B9E9d6c5dB704EaAb4787"), 0.5), // cREAL
            ],
        );

        // The underlying token addresses never appear in `feeCurrency`, so they must stay
        // absent from the map and fall back to the default limit.
        let defaults =
            FeeCurrencyLimits::defaults_for_chain(celo_revm::constants::CELO_MAINNET_CHAIN_ID);
        for (name, token) in [
            ("USDT", address!("48065fbbe25f71c9282ddf5e1cd6d6a887483d5e")),
            ("USDC", address!("cebA9300f2b948710d2653dD7B07f33A8B32118C")),
        ] {
            assert!(
                !defaults.contains_key(&token),
                "{name} token address must take the default limit, not a keyed one"
            );
        }
    }

    #[test]
    fn test_max_gas_for_currency_native() {
        let limits = FeeCurrencyLimits::default();
        // Native CELO (None) should be unlimited
        assert_eq!(limits.max_gas_for_currency(None, 30_000_000), None);
    }

    #[test]
    fn test_max_gas_for_currency_default() {
        let limits = FeeCurrencyLimits { limits: HashMap::new(), default_limit: 0.5 };
        let addr: Address = "0x765DE816845861e75A25fCA122bb6898B8B1282a".parse().unwrap();
        // Default 0.5 of 30M = 15M
        assert_eq!(limits.max_gas_for_currency(Some(addr), 30_000_000), Some(15_000_000));
    }

    #[test]
    fn test_max_gas_for_currency_specific() {
        let addr: Address = "0x765DE816845861e75A25fCA122bb6898B8B1282a".parse().unwrap();
        let mut map = HashMap::new();
        map.insert(addr, 0.9);
        let limits = FeeCurrencyLimits { limits: map, default_limit: 0.5 };
        // 0.9 of 30M = 27M
        assert_eq!(limits.max_gas_for_currency(Some(addr), 30_000_000), Some(27_000_000));
    }

    // -----------------------------------------------------------------------
    // CeloFeeCurrencyFilter tests
    // -----------------------------------------------------------------------

    use crate::pool::CeloPoolTx;
    use reth_transaction_pool::{
        CoinbaseTipOrdering, Pool, PoolConfig, PoolTransaction, TransactionOrigin,
        TransactionValidationOutcome, TransactionValidator, blobstore::NoopBlobStore,
        validate::ValidTransaction,
    };

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

    #[derive(Debug)]
    struct AcceptAll;

    impl TransactionValidator for AcceptAll {
        type Transaction = CeloPoolTx;
        type Block = crate::primitives::CeloBlock;

        async fn validate_transaction(
            &self,
            _origin: TransactionOrigin,
            mut transaction: CeloPoolTx,
        ) -> TransactionValidationOutcome<CeloPoolTx> {
            transaction
                .apply_exchange_rate(crate::pool::ExchangeRate { numerator: 1, denominator: 1 });
            TransactionValidationOutcome::Valid {
                balance: U256::MAX,
                state_nonce: 0,
                bytecode_hash: None,
                transaction: ValidTransaction::Valid(transaction),
                propagate: false,
                authorities: None,
            }
        }
    }

    type EvictionTestPool = Pool<AcceptAll, CoinbaseTipOrdering<CeloPoolTx>, NoopBlobStore>;

    fn eviction_test_pool() -> EvictionTestPool {
        Pool::new(
            AcceptAll,
            CoinbaseTipOrdering::default(),
            NoopBlobStore::default(),
            PoolConfig::default(),
        )
    }

    fn eviction_filter(
        pool: EvictionTestPool,
        txs: Vec<CeloPoolTx>,
        revert_evictions: RevertEvictions,
    ) -> CeloFeeCurrencyFilter<VecPayloadTransactions, EvictionTestPool> {
        CeloFeeCurrencyFilter {
            inner: VecPayloadTransactions { txs, invalid: vec![] },
            pool,
            limits: FeeCurrencyLimits { limits: HashMap::new(), default_limit: 1.0 },
            failure_policies: CeloFailurePolicies::new(
                FeeCurrencyBlocklist::default(),
                revert_evictions,
            ),
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
            pending_charge: None,
        }
    }

    #[tokio::test]
    async fn reverted_transaction_evicts_exact_hash_and_descendants() {
        let fc = fc_addr(10);
        let sender = Address::with_last_byte(1);
        let other_sender = Address::with_last_byte(2);
        let ancestor =
            crate::test_utils::make_test_tx_with_nonce(Some(fc), 0, 100_000, 100, 10, sender);
        let target =
            crate::test_utils::make_test_tx_with_nonce(Some(fc), 1, 100_000, 100, 10, sender);
        let descendant =
            crate::test_utils::make_test_tx_with_nonce(Some(fc), 2, 100_000, 100, 10, sender);
        let other =
            crate::test_utils::make_test_tx_with_nonce(Some(fc), 0, 100_001, 100, 10, other_sender);
        let ancestor_hash = *ancestor.hash();
        let target_hash = *target.hash();
        let descendant_hash = *descendant.hash();
        let other_hash = *other.hash();
        let pool = eviction_test_pool();
        for tx in [ancestor, target.clone(), descendant, other] {
            pool.add_transaction(TransactionOrigin::External, tx).await.unwrap();
        }
        let evictions = RevertEvictions::default();
        evictions.record(target_hash);
        let mut filter = CeloFeeCurrencyFilter {
            inner: VecPayloadTransactions { txs: vec![target.clone()], invalid: vec![] },
            pool: pool.clone(),
            limits: FeeCurrencyLimits::default(),
            failure_policies: CeloFailurePolicies::new(FeeCurrencyBlocklist::default(), evictions),
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
            pending_charge: None,
        };

        let yielded = filter.next(()).unwrap();
        filter.mark_invalid(yielded.sender(), yielded.nonce());

        assert!(pool.get(&ancestor_hash).is_some());
        assert!(pool.get(&target_hash).is_none());
        assert!(pool.get(&descendant_hash).is_none());
        assert!(pool.get(&other_hash).is_some());
    }

    #[tokio::test]
    async fn same_nonce_replacement_is_not_removed_by_old_hash_marker() {
        let fc = fc_addr(11);
        let sender = Address::with_last_byte(1);
        let old = crate::test_utils::make_test_tx_with_nonce(Some(fc), 0, 100_000, 100, 10, sender);
        let replacement =
            crate::test_utils::make_test_tx_with_nonce(Some(fc), 0, 100_000, 120, 12, sender);
        let old_hash = *old.hash();
        let replacement_hash = *replacement.hash();
        let pool = eviction_test_pool();
        pool.add_transaction(TransactionOrigin::External, replacement.clone()).await.unwrap();
        let evictions = RevertEvictions::default();
        evictions.record(old_hash);
        let mut filter = eviction_filter(pool.clone(), vec![old], evictions.clone());

        let yielded = filter.next(()).unwrap();
        assert_eq!(*yielded.hash(), old_hash);
        filter.mark_invalid(yielded.sender(), yielded.nonce());

        assert!(pool.get(&replacement_hash).is_some());
        assert!(!evictions.take(old_hash));
    }

    #[tokio::test]
    async fn unrelated_mark_invalid_does_not_consume_or_evict() {
        let fc = fc_addr(12);
        let sender = Address::with_last_byte(1);
        let tx = crate::test_utils::make_test_tx_with_nonce(Some(fc), 0, 100_000, 100, 10, sender);
        let tx_hash = *tx.hash();
        let pool = eviction_test_pool();
        pool.add_transaction(TransactionOrigin::External, tx.clone()).await.unwrap();
        let evictions = RevertEvictions::default();
        evictions.record(tx_hash);
        let mut filter = eviction_filter(pool.clone(), vec![tx], evictions.clone());

        assert!(filter.next(()).is_some());
        filter.mark_invalid(Address::with_last_byte(9), 0);

        assert!(pool.get(&tx_hash).is_some());
        assert!(evictions.take(tx_hash));
    }

    #[tokio::test]
    async fn repeated_revert_eviction_is_idempotent() {
        let fc = fc_addr(13);
        let sender = Address::with_last_byte(1);
        let tx = crate::test_utils::make_test_tx_with_nonce(Some(fc), 0, 100_000, 100, 10, sender);
        let tx_hash = *tx.hash();
        let pool = eviction_test_pool();
        pool.add_transaction(TransactionOrigin::External, tx.clone()).await.unwrap();
        let evictions = RevertEvictions::default();
        evictions.record(tx_hash);
        let mut filter = eviction_filter(pool.clone(), vec![tx], evictions);

        let yielded = filter.next(()).unwrap();
        filter.mark_invalid(yielded.sender(), yielded.nonce());
        filter.mark_invalid(yielded.sender(), yielded.nonce());

        assert!(pool.get(&tx_hash).is_none());
    }

    #[tokio::test]
    async fn blocklist_skip_does_not_consume_revert_marker_or_evict() {
        let fc = fc_addr(14);
        let sender = Address::with_last_byte(1);
        let tx = crate::test_utils::make_test_tx_with_nonce(Some(fc), 0, 100_000, 100, 10, sender);
        let tx_hash = *tx.hash();
        let pool = eviction_test_pool();
        pool.add_transaction(TransactionOrigin::External, tx.clone()).await.unwrap();
        let evictions = RevertEvictions::default();
        evictions.record(tx_hash);
        let mut filter = eviction_filter(pool.clone(), vec![tx], evictions.clone());
        filter.failure_policies.blocklist().block_currency(fc, 1);

        assert!(filter.next(()).is_none());
        assert!(pool.get(&tx_hash).is_some());
        assert!(evictions.take(tx_hash));
    }

    #[tokio::test]
    async fn gas_cap_skip_does_not_consume_revert_marker_or_evict() {
        let fc = fc_addr(15);
        let sender = Address::with_last_byte(1);
        let tx = crate::test_utils::make_test_tx_with_nonce(Some(fc), 0, 100_000, 100, 10, sender);
        let tx_hash = *tx.hash();
        let pool = eviction_test_pool();
        pool.add_transaction(TransactionOrigin::External, tx.clone()).await.unwrap();
        let evictions = RevertEvictions::default();
        evictions.record(tx_hash);
        let mut filter = eviction_filter(pool.clone(), vec![tx], evictions.clone());
        filter.limits.default_limit = 0.0;

        assert!(filter.next(()).is_none());
        assert!(pool.get(&tx_hash).is_some());
        assert!(evictions.take(tx_hash));
    }

    #[test]
    fn starting_payload_iterator_preserves_markers_from_concurrent_jobs() {
        let tx_hash = alloy_primitives::B256::with_last_byte(1);
        let evictions = RevertEvictions::default();
        evictions.record(tx_hash);
        let payload_transactions = CeloPayloadTransactions::new(
            FeeCurrencyLimits::default(),
            CeloFailurePolicies::new(FeeCurrencyBlocklist::default(), evictions.clone()),
        );

        let _filter = payload_transactions.best_transactions(
            eviction_test_pool(),
            reth_transaction_pool::BestTransactionsAttributes::new(0, None),
        );

        assert!(evictions.take(tx_hash));
    }

    #[test]
    fn filter_passes_native_celo_tx() {
        let sender = Address::with_last_byte(1);
        let mut filter = CeloFeeCurrencyFilter {
            inner: VecPayloadTransactions {
                txs: vec![make_test_tx(None, 21_000, sender)],
                invalid: vec![],
            },
            pool: reth_transaction_pool::noop::NoopTransactionPool::<CeloPoolTx>::new(),
            limits: FeeCurrencyLimits::default(),
            failure_policies: CeloFailurePolicies::default(),
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
            pending_charge: None,
        };

        assert!(filter.next(()).is_some());
        assert!(filter.next(()).is_none());
    }

    #[test]
    fn filter_native_celo_bypasses_the_per_currency_cap() {
        // A native CELO tx far above any per-currency cap (25M > 0.5*30M = 15M) must
        // still pass — native CELO is unlimited. (filter_passes_native_celo_tx only
        // uses a trivial 21k tx, so it never exercises "past the cap".)
        let sender = Address::with_last_byte(1);
        let mut filter = CeloFeeCurrencyFilter {
            inner: VecPayloadTransactions {
                txs: vec![make_test_tx(None, 25_000_000, sender)],
                invalid: vec![],
            },
            pool: reth_transaction_pool::noop::NoopTransactionPool::<CeloPoolTx>::new(),
            limits: FeeCurrencyLimits::default(),
            failure_policies: CeloFailurePolicies::default(),
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
            pending_charge: None,
        };
        assert!(filter.next(()).is_some(), "native CELO must bypass the per-currency cap");
        assert!(filter.next(()).is_none());
    }

    #[test]
    fn filter_applies_per_currency_fraction_from_limits_map() {
        // fc_a configured at 0.9 (-> 27M cap), fc_b uses the 0.5 default (-> 15M cap)
        // on a 30M block. A 20M tx passes for fc_a but is skipped for fc_b — proving
        // the filter applies the per-address fraction, not just the default.
        let sender_a = Address::with_last_byte(1);
        let sender_b = Address::with_last_byte(2);
        let fc_a = fc_addr(10);
        let fc_b = fc_addr(11);
        let mut limits = FeeCurrencyLimits::default();
        limits.limits.insert(fc_a, 0.9);
        let mut filter = CeloFeeCurrencyFilter {
            inner: VecPayloadTransactions {
                txs: vec![
                    make_test_tx(Some(fc_a), 20_000_000, sender_a),
                    make_test_tx(Some(fc_b), 20_000_000, sender_b),
                ],
                invalid: vec![],
            },
            pool: reth_transaction_pool::noop::NoopTransactionPool::<CeloPoolTx>::new(),
            limits,
            failure_policies: CeloFailurePolicies::default(),
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
            pending_charge: None,
        };
        let t1 = filter.next(()).unwrap();
        assert_eq!(t1.fee_currency(), Some(fc_a), "fc_a 20M is under its 27M (0.9) cap");
        assert!(filter.next(()).is_none(), "fc_b 20M exceeds its 15M (0.5 default) cap");
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
            pool: reth_transaction_pool::noop::NoopTransactionPool::<CeloPoolTx>::new(),
            limits: FeeCurrencyLimits::default(),
            failure_policies: CeloFailurePolicies::default(),
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
            pending_charge: None,
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
            pool: reth_transaction_pool::noop::NoopTransactionPool::<CeloPoolTx>::new(),
            limits: FeeCurrencyLimits::default(),
            failure_policies: CeloFailurePolicies::default(),
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
            pending_charge: None,
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
            pool: reth_transaction_pool::noop::NoopTransactionPool::<CeloPoolTx>::new(),
            limits: FeeCurrencyLimits::default(),
            failure_policies: CeloFailurePolicies::new(blocklist, RevertEvictions::default()),
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
            pending_charge: None,
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
            pool: reth_transaction_pool::noop::NoopTransactionPool::<CeloPoolTx>::new(),
            limits: FeeCurrencyLimits::default(), // max = 0.5 * 30M = 15M
            failure_policies: CeloFailurePolicies::default(),
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
            pending_charge: None,
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
            pool: reth_transaction_pool::noop::NoopTransactionPool::<CeloPoolTx>::new(),
            limits: FeeCurrencyLimits::default(), // max = 15M
            failure_policies: CeloFailurePolicies::default(),
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
            pending_charge: None,
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
            pool: reth_transaction_pool::noop::NoopTransactionPool::<CeloPoolTx>::new(),
            limits: FeeCurrencyLimits::default(),
            failure_policies: CeloFailurePolicies::new(blocklist, RevertEvictions::default()),
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
            pending_charge: None,
        };

        assert!(filter.next(()).is_some(), "Tx should pass after blocklist eviction");
    }

    #[test]
    fn filter_rolls_back_rejected_fee_currency_charge() {
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
            pool: reth_transaction_pool::noop::NoopTransactionPool::<CeloPoolTx>::new(),
            limits: FeeCurrencyLimits::default(),
            failure_policies: CeloFailurePolicies::default(),
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
            pending_charge: None,
        };

        let rejected = filter.next(()).expect("first tx should fit");
        filter.mark_invalid(rejected.sender(), rejected.nonce());

        let next = filter.next(()).expect("rejected tx should release its 10M reservation");
        assert_eq!(next.sender(), sender_b);
        assert_eq!(next.fee_currency(), Some(fc));
    }

    #[test]
    fn filter_mismatched_invalidation_keeps_fee_currency_charge() {
        let sender_a = Address::with_last_byte(1);
        let sender_b = Address::with_last_byte(2);
        let unrelated_sender = Address::with_last_byte(3);
        let fc = fc_addr(10);
        let mut filter = CeloFeeCurrencyFilter {
            inner: VecPayloadTransactions {
                txs: vec![
                    make_test_tx(Some(fc), 10_000_000, sender_a),
                    make_test_tx(Some(fc), 8_000_000, sender_b),
                ],
                invalid: vec![],
            },
            pool: reth_transaction_pool::noop::NoopTransactionPool::<CeloPoolTx>::new(),
            limits: FeeCurrencyLimits::default(),
            failure_policies: CeloFailurePolicies::default(),
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
            pending_charge: None,
        };

        assert_eq!(filter.next(()).unwrap().sender(), sender_a);
        filter.mark_invalid(unrelated_sender, 0);

        assert!(
            filter.next(()).is_none(),
            "unrelated invalidation must not release sender A's gas"
        );
        assert_eq!(filter.inner.invalid.last(), Some(&(sender_b, 0)));
    }

    #[test]
    fn filter_continued_iteration_commits_fee_currency_charge() {
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
            pool: reth_transaction_pool::noop::NoopTransactionPool::<CeloPoolTx>::new(),
            limits: FeeCurrencyLimits::default(),
            failure_policies: CeloFailurePolicies::default(),
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
            pending_charge: None,
        };

        assert_eq!(filter.next(()).unwrap().sender(), sender_a);
        assert!(filter.next(()).is_none(), "continuing iteration commits sender A's reservation");
    }

    #[test]
    fn filter_native_rejection_does_not_change_fee_currency_charge() {
        let native_sender = Address::with_last_byte(1);
        let fee_currency_sender = Address::with_last_byte(2);
        let fc = fc_addr(10);
        let mut filter = CeloFeeCurrencyFilter {
            inner: VecPayloadTransactions {
                txs: vec![
                    make_test_tx(None, 30_000_000, native_sender),
                    make_test_tx(Some(fc), 15_000_000, fee_currency_sender),
                ],
                invalid: vec![],
            },
            pool: reth_transaction_pool::noop::NoopTransactionPool::<CeloPoolTx>::new(),
            limits: FeeCurrencyLimits::default(),
            failure_policies: CeloFailurePolicies::default(),
            block_gas_limit: 30_000_000,
            gas_used_per_currency: HashMap::new(),
            pending_charge: None,
        };

        let rejected = filter.next(()).expect("native tx should bypass the fee-currency cap");
        filter.mark_invalid(rejected.sender(), rejected.nonce());

        assert_eq!(filter.next(()).unwrap().sender(), fee_currency_sender);
    }
}
