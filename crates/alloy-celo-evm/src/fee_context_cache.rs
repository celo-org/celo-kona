//! Block-start fee-currency context: resolver and memo.
//!
//! # Why this exists
//!
//! Celo's consensus rule for CIP-64 is that the fee-currency exchange rates are read
//! **once per block, from block-start state**, and reused for every transaction in the
//! block. Execution paths that run a whole block through one EVM (the block executor
//! used by import/sequencing/derivation, and reth's parity `trace_*` path via
//! `TxTracer`) get this for free: `load_fee_currency_context` in `celo-revm` loads at
//! tx 0 and the `updated_at_block` check short-circuits every later transaction.
//!
//! reth's `debug_trace*` path breaks the pattern: it creates a **fresh EVM per
//! transaction** and commits each transaction's state to the shared DB before the next
//! (`DebugApi::trace_block` and friends). Each fresh EVM re-loads the fee-currency
//! context from **mid-block** state. If an earlier transaction in the block updated an
//! oracle rate, later CIP-64 transactions are validated against the wrong rate —
//! surfacing as `max fee per gas less than block base fee` errors on
//! `debug_traceBlockByHash`, or silently wrong gas/fee amounts in traces.
//!
//! # Mechanism: a trusted resolver plus a memo
//!
//! [`CeloEvm::transact_raw`](crate::CeloEvm) pins those per-tx replay EVMs to block-start rates by
//! asking a **[`FeeContextResolver`]** for the block's context, keyed `(block_number,
//! parent_hash)`. The resolver computes the context **solely from canonical-chain state** — the
//! std-only node layer (`celo-reth`) wires one backed by the state provider: look up the canonical
//! header at `block_number`, verify its parent, load state at `parent_hash`, and run the
//! directory/oracle view calls under that block's env. Because the value derives only from
//! canonical state, no RPC caller can influence it (a forged `debug_traceBlock` body cannot change
//! a rate), and historical blocks resolve the same as recent ones.
//!
//! The resolver's work (a provider read plus contract view calls) is memoized by the
//! [`FeeCurrencyContextCache`] so it runs at most once per block across all the per-tx EVMs of a
//! trace — not once per transaction. The memo is shared via
//! [`CeloEvmFactory`](crate::CeloEvmFactory) (like the fee-currency blocklist). Its only writer is
//! the resolver path in `transact_raw`, so every cached value is canonical-derived; there is no
//! writer that trusts transaction content, hence nothing to poison.
//!
//! # Scope: RPC replay only — consensus never participates
//!
//! Every consensus path — block import, derivation, sequencing, and the kona proof executor — opts
//! out wholesale: `CeloBlockExecutorFactory::create_executor`, the single choke point all of them
//! obtain their EVM through, disables participation (`CeloEvm::with_fee_context_cache_disabled`).
//! Consensus loses nothing — a single-EVM-per-block executor loads the correct context fresh at
//! tx 0 by construction — and gains isolation: it never consults the resolver/memo and never pays
//! the extra `Database::block_hash` lookup (which on a witness-recording proof DB could perturb the
//! recorded preimage set). no-std consumers (kona) wire no resolver at all. Only the loose per-tx
//! EVMs reth's RPC layer builds directly via `EvmFactory::create_evm*` participate.
//!
//! **Key**: `(block_number, parent_hash)`. Reorg siblings with different parents get different
//! keys; header-field keys (number/timestamp/prevrandao) can collide across unsafe-head reorgs.
//! Seeding a memo hit skips the directory system calls, so there is no warmth to neutralize (no
//! `transaction_id` bump needed) — the same observable state as consensus transactions 1..n.
//!
//! **Call-style simulations bypass entirely** (`transact_raw` skips this when the base-fee check
//! is disabled): `eth_call`/`eth_estimateGas`/`debug_traceCall` run at end-of-block state where the
//! rates they load are the intended semantics.
//!
//! When the block-start context cannot be obtained, `transact_raw` **refuses** rather than fall
//! back to current-state (mid-block) rates: the block-start-rates rule is absolute and a
//! mid-block per-tx EVM cannot reconstruct block-start rates from its own state. This covers a
//! failing `Database::block_hash`, an unresolvable block (a resolver that returns `None` — a
//! non-canonical/forged block, or a canonical block whose state the node has pruned), and a node
//! with no resolver wired. Genesis (block 0) is the one benign exception: it carries no
//! transactions and its state *is* block-start, so the handler's fresh load is already correct and
//! pinning is skipped. The real fix is upstream: reth reusing one EVM per block in `debug_trace*`,
//! after which the resolver and memo can be deleted.

use alloc::{collections::VecDeque, sync::Arc};
use alloy_primitives::B256;
use celo_revm::FeeCurrencyContext;
use spin::Mutex;

/// Resolves the block-start fee-currency context for a block from trusted (canonical-chain) state.
///
/// Consulted by [`CeloEvm::transact_raw`](crate::CeloEvm) on reth's per-tx `debug_trace*` replay
/// EVMs (see the module docs) to obtain block-start CIP-64 rates instead of loading mid-block ones.
/// A resolver is wired only by the std-only node layer (`celo-reth`), backed by the state provider;
/// no-std consumers (kona) and consensus executors never use one.
///
/// Implementations **must** derive the returned context solely from canonical-chain state for the
/// given `(block_number, parent_hash)` — never from any caller-supplied block body — so that a
/// value can never be influenced by an RPC caller (e.g. a forged `debug_traceBlock` payload).
pub trait FeeContextResolver: Send + Sync {
    /// Returns the block-start [`FeeCurrencyContext`] for the block at `block_number` whose parent
    /// is `parent_hash`, or `None` if it cannot be resolved from canonical state (unknown or
    /// non-canonical block, pruned history, provider error).
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
    /// when full. Re-inserting an existing key replaces its value in place.
    ///
    /// The only caller is `CeloEvm::transact_raw`, which inserts exactly what a
    /// [`FeeContextResolver`] returned — a value derived solely from canonical state (see module
    /// docs), never from transaction content.
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
