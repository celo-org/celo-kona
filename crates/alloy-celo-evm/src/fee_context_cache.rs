//! Block-start fee-currency context cache.
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
//! This cache carries the block-start context across those per-transaction EVMs. It is
//! shared via [`CeloEvmFactory`](crate::CeloEvmFactory) (like the fee-currency
//! blocklist) and consulted in `CeloEvm::transact_raw`.
//!
//! # Scope: RPC replay only — consensus never touches this cache
//!
//! The population gate below trusts *transaction content* (`tx_type == DEPOSIT && caller ==
//! L1_INFO_DEPOSITOR`), and `debug_traceRawBlock` replays attacker-supplied blocks in which a
//! deposit's `from` is unauthenticated bytes (deposits carry no signature). A forged deposit
//! placed after a rate-update tx could therefore write a *mid-block* context under the key the
//! next canonical block will use. Consensus execution must never be exposed to that, so every
//! consensus path — block import, derivation, sequencing, and the kona proof executor — opts out
//! wholesale: `CeloBlockExecutorFactory::create_executor`, the single choke point all of them
//! obtain their EVM through, disables cache participation
//! (`CeloEvm::with_fee_context_cache_disabled`). Consensus loses nothing — a
//! single-EVM-per-block executor loads the correct context fresh at tx 0 by construction — and
//! gains isolation: it neither reads RPC-writable state nor performs the cache's extra
//! `Database::block_hash` lookup (which on a witness-recording proof DB could perturb the
//! recorded preimage set). Only the loose per-tx EVMs reth's RPC layer builds directly via
//! `EvmFactory::create_evm*` participate.
//!
//! # Design
//!
//! - **Key**: `(block_number, parent_hash)`. The block-start fee-currency context is a function of
//!   the parent state (plus the block env the directory/oracle calls run under): pre-execution
//!   system calls (beacon root / history storage) never touch the fee-currency directory or
//!   oracles, and nothing else runs before tx 0. Reorg siblings with different parents get
//!   different keys; header-field keys (number/timestamp/prevrandao) can collide across unsafe-head
//!   reorgs. (Two same-parent siblings share a key; their contexts can only differ if a registered
//!   rate contract reads header fields like `TIMESTAMP` — no deployed Celo fee-currency contract
//!   does, and self-healing below bounds the damage to one trace.)
//! - **Population** is restricted to EVMs whose *current transaction is the L1-info deposit*. In
//!   every honest replay that transaction is index 0, so nothing of the block has been committed to
//!   the DB the context was loaded from — the loaded context is block-start. Mid-block trace EVMs
//!   that *miss* the cache fall back to the pre-cache behavior (logged at debug level) and never
//!   write.
//! - **Self-healing**: the L1-info deposit's EVM never *reads* the cache — its fresh load is
//!   correct by construction — and its harvest *overwrites* the entry. Every block-trace request
//!   therefore repairs a stale or poisoned entry at tx 0 before its own later per-tx EVMs consult
//!   it. (`debug_traceTransaction` populates the same way: reth replays the preceding transactions,
//!   starting with the deposit, through one non-inspecting EVM before tracing the target in a fresh
//!   one.)
//! - **Call-style simulations bypass entirely** (`transact_raw` skips the cache when the base-fee
//!   check is disabled): `eth_call`/`eth_estimateGas`/`debug_traceCall` run at end-of-block state
//!   where the rates they load are the intended semantics — they must neither read block-start
//!   rates nor write end-of-block rates.
//!
//! Seeding also preserves warm/cold accounting: a hit skips the directory system calls
//! altogether, exactly like consensus transactions 1..n, so there is no warmth to
//! neutralize (no `transaction_id` bump needed).
//!
//! Misses degrade gracefully: genesis (no parent), a failing `Database::block_hash`,
//! or an evicted entry simply run the pre-cache behavior.
//!
//! # Residual risk (accepted)
//!
//! A caller with `debug`-namespace access can still poison an entry via a forged raw block and
//! race a *concurrent* trace request between its tx-0 repair and a later read, corrupting that
//! one trace response. This affects RPC output only (consensus is opted out), requires access to
//! a namespace reth documents as trusted-only, and self-heals on the next request — accepted
//! rather than adding request-scoped machinery. The real fix is upstream: reth reusing one EVM
//! per block in `debug_trace*`, after which this cache can be deleted.

use alloc::{collections::VecDeque, sync::Arc};
use alloy_primitives::{Address, B256, address};
use celo_revm::FeeCurrencyContext;
use spin::Mutex;

/// Sender of the L1-attributes deposit transaction — always transaction 0 of every
/// OP-stack (and Celo L2) block. Same address on all chains.
///
/// Mirrors kona's `L1_INFO_DEPOSITOR_ADDRESS` (`kona-protocol`, `info/variant.rs`), the
/// derivation-layer constant used to *construct* the attributes deposit; it is `pub(crate)`
/// upstream, hence the local copy. In an *honest* replay a deposit from this sender is at
/// block-start state, which is what gates cache population — but the pair is forgeable via
/// `debug_traceRawBlock` (deposits are unsigned), which is why consensus opts out of the cache
/// entirely (see module docs).
pub const L1_INFO_DEPOSITOR: Address = address!("0xDeaDDEaDdeAddEAddeadDEaDDEAdDeaDDeAD0001");

/// Maximum number of cached block contexts.
///
/// Bounds memory only; correctness never depends on residency. Concurrent tracing is
/// semaphore-limited well below this, and every block-trace entry point starts at tx 0,
/// which repopulates its own block's entry before the per-tx EVMs of that request
/// consult it.
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

    /// Inserts the block-start context for `(number, parent_hash)`, evicting the oldest
    /// entry when full. Re-inserting an existing key replaces its value in place — this is
    /// load-bearing: the L1-info deposit's EVM never seeds from the cache and always
    /// harvests, so each block-trace request *repairs* a stale or poisoned entry at tx 0
    /// (see module docs, "Self-healing").
    ///
    /// Callers must only pass contexts loaded from block-start state (see module docs);
    /// `CeloEvm::transact_raw` enforces this by gating on the L1-info deposit tx.
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
