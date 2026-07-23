//! Block-start fee-currency context: resolver and memo.
//!
//! Celo's consensus rule for CIP-64 reads the fee-currency exchange rates **once per block, from
//! block-start state**, and reuses them for every transaction. Paths that run a whole block
//! through one EVM (the block executor used by import/sequencing/derivation, reth's parity
//! `trace_*` path via `TxTracer`) get this for free: `load_fee_currency_context` in `celo-revm`
//! loads at tx 0 and the `updated_at_block` check short-circuits every later transaction. reth's
//! `debug_trace*` path instead creates a **fresh EVM per transaction**, committing each
//! transaction to the shared DB before the next, so each EVM re-loads the context from
//! **mid-block** state: if an earlier transaction updated an oracle rate, later CIP-64
//! transactions validate against the wrong rate — `max fee per gas less than block base fee`
//! errors on `debug_traceBlockByHash`, or silently wrong gas/fee amounts in traces.
//!
//! [`CeloEvm::transact_raw`](crate::CeloEvm) pins those replay EVMs to block-start rates by
//! asking a **[`FeeContextResolver`]** for the block's context, keyed `(block_number,
//! parent_hash)` and memoized in the [`FeeCurrencyContextCache`] (shared via
//! [`CeloEvmFactory`](crate::CeloEvmFactory)) so the resolver runs once per block, not once per
//! transaction. The resolver — wired only by the std-only node layer (`celo-reth`), backed by the
//! state provider — computes the context **solely from stored chain state**: a canonical block at
//! `block_number` with a matching parent resolves under its own env, and a not-yet-canonical
//! *child of a known parent* (prewarming, next-block bundles, head-extension traces) resolves
//! from that parent's post-state — which IS its block-start state — under a synthesized
//! next-block env (see `celo-reth`'s `fee_resolver` docs). Hence no RPC caller can influence a
//! value (a forged `debug_traceBlock` body cannot change a rate), historical blocks resolve the
//! same as recent ones, and — the resolver path being the memo's only writer — there is nothing
//! to poison.
//!
//! The key pairs the number with `parent_hash` (header fields like number/timestamp/prevrandao
//! can collide across unsafe-head reorgs; reorg siblings get distinct keys), so an inconsistent
//! forged pair can never alias a legitimately resolved entry. Seeding a memo hit skips the
//! directory system calls, so there is no warmth to neutralize (no `transaction_id` bump needed)
//! — the same observable state as consensus transactions 1..n.
//!
//! Scope — RPC replay only:
//! - **Consensus never participates.** `CeloBlockExecutorFactory::create_executor` — the single
//!   choke point block import, derivation, sequencing, and the kona proof executor obtain their EVM
//!   through — opts out via `CeloEvm::for_block_executor`. A single-EVM-per-block executor loads
//!   the correct context at tx 0 by construction, and opting out also skips the extra
//!   `Database::block_hash` lookup (which on a witness-recording proof DB could perturb the
//!   recorded preimage set). no-std consumers (kona) wire no resolver at all; only the loose per-tx
//!   EVMs reth's RPC layer builds via `EvmFactory::create_evm*` participate.
//! - **Call-style simulations bypass** (`transact_raw` skips pinning when the base-fee check is
//!   disabled): `eth_call`/`eth_estimateGas`/`debug_traceCall` run at end-of-block state where the
//!   rates they load are the intended semantics.
//! - **Unresolvable ⇒ refuse.** When the block-start context cannot be obtained — no resolver
//!   wired, failing `Database::block_hash`, a forged/unknown `(number, parent)` pair, pruned state
//!   — `transact_raw` errors rather than fall back to mid-block rates: the block-start-rates rule
//!   is absolute and a mid-block per-tx EVM cannot reconstruct block-start rates from its own
//!   state. Genesis is the one benign exception: block 0 carries no transactions, so its state *is*
//!   block-start and the fresh load is already correct.
//!
//! The real fix is upstream — reth reusing one EVM per block in `debug_trace*` — after which the
//! resolver and memo can be deleted.

use alloc::{collections::VecDeque, sync::Arc};
use alloy_primitives::B256;
use celo_revm::FeeCurrencyContext;
use spin::Mutex;

/// Resolves the block-start fee-currency context for a block from chain state the node stores.
///
/// Consulted by [`CeloEvm::transact_raw`](crate::CeloEvm) on a [`FeeCurrencyContextCache`] miss
/// (see the module docs). Implementations **must** derive the returned context solely from
/// chain state the node already stores for the given `(block_number, parent_hash)` — never from
/// any caller-supplied block body — so that a value can never be influenced by an RPC caller
/// (e.g. a forged `debug_traceBlock` payload).
pub trait FeeContextResolver: Send + Sync {
    /// Returns the block-start [`FeeCurrencyContext`] for the block at `block_number` whose parent
    /// is `parent_hash`, or `None` if it cannot be resolved from stored chain state (unknown
    /// block or parent, pruned history, provider error).
    fn resolve(&self, block_number: u64, parent_hash: B256) -> Option<FeeCurrencyContext>;
}

/// Maximum number of memoized block contexts.
///
/// Bounds memory only; correctness never depends on residency — a miss simply re-runs the
/// resolver. Concurrent tracing is semaphore-limited well below this.
const CACHE_CAPACITY: usize = 128;

type CacheKey = (u64, B256);

/// Shared, thread-safe cache of block-start [`FeeCurrencyContext`]s keyed by
/// `(block_number, parent_hash)`, FIFO-evicted at `CACHE_CAPACITY` entries.
///
/// See the module docs for the full design and safety argument.
#[derive(Debug, Clone, Default)]
pub struct FeeCurrencyContextCache {
    /// Insertion-ordered entries; linear scan is fine at this capacity, and it is
    /// touched at most once per (EVM, block) — not per opcode.
    inner: Arc<Mutex<VecDeque<(CacheKey, FeeCurrencyContext)>>>,
}

impl FeeCurrencyContextCache {
    /// Returns the cached block-start context for the block `number` whose parent is
    /// `parent_hash`, if present.
    pub fn get(&self, number: u64, parent_hash: B256) -> Option<FeeCurrencyContext> {
        let key = (number, parent_hash);
        self.inner.lock().iter().find(|(k, _)| *k == key).map(|(_, ctx)| ctx.clone())
    }

    /// Memoizes the block-start context for `(number, parent_hash)`, evicting the oldest entry
    /// when full. Re-inserting an existing key replaces its value in place. The only caller is
    /// `CeloEvm::transact_raw`, which inserts exactly what a [`FeeContextResolver`] returned.
    pub fn insert(&self, number: u64, parent_hash: B256, context: FeeCurrencyContext) {
        let key = (number, parent_hash);
        let mut entries = self.inner.lock();
        if let Some(entry) = entries.iter_mut().find(|(k, _)| *k == key) {
            entry.1 = context;
            return;
        }
        if entries.len() >= CACHE_CAPACITY {
            entries.pop_front();
        }
        entries.push_back((key, context));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;

    fn ctx_at_block(number: u64) -> FeeCurrencyContext {
        FeeCurrencyContext::new(Default::default(), Some(U256::from(number)))
    }

    #[test]
    fn get_returns_inserted_context() {
        let cache = FeeCurrencyContextCache::default();
        let parent = B256::repeat_byte(1);
        assert!(cache.get(5, parent).is_none());

        cache.insert(5, parent, ctx_at_block(5));
        assert_eq!(cache.get(5, parent).unwrap().updated_at_block, Some(U256::from(5)));
        // Same number, different parent (reorg sibling chain) — distinct entry.
        assert!(cache.get(5, B256::repeat_byte(2)).is_none());
    }

    #[test]
    fn reinsert_replaces_value() {
        let cache = FeeCurrencyContextCache::default();
        let parent = B256::repeat_byte(1);
        cache.insert(5, parent, ctx_at_block(5));
        cache.insert(5, parent, FeeCurrencyContext::new(Default::default(), Some(U256::from(99))));
        assert_eq!(cache.get(5, parent).unwrap().updated_at_block, Some(U256::from(99)));
    }

    #[test]
    fn fifo_eviction_at_capacity() {
        let cache = FeeCurrencyContextCache::default();
        let parent = B256::repeat_byte(1);
        for n in 0..=CACHE_CAPACITY as u64 {
            cache.insert(n, parent, ctx_at_block(n));
        }
        // Entry 0 was evicted by the (CAPACITY+1)-th insert; the rest remain.
        assert!(cache.get(0, parent).is_none());
        assert!(cache.get(1, parent).is_some());
        assert!(cache.get(CACHE_CAPACITY as u64, parent).is_some());
    }
}
