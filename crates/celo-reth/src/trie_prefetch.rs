//! Concurrent trie-path prefetch for the sequencing state root.
//!
//! # Why
//!
//! When the sequencer seals a block, `BasicBlockBuilder::finish` is called with
//! `state_root_precomputed = None`, so the state root is computed by the **serial** `StateRoot`
//! walk (`reth_trie::StateRoot`, which contains no rayon, no threads and no prefetching). A merkle
//! walk is pointer-chasing: the location of a node at depth *n+1* is only known once the node at
//! depth *n* has been read. Queue depth is therefore ~1 and the cost is `depth x read_latency`, no
//! matter how much throughput the device has spare.
//!
//! That is exactly the shape of celo-blockchain-planning#1453: I/O-bound, IOPS-insensitive,
//! CPU-idle. Measured on a 2M-account state with the page cache continuously evicted, the identical
//! build issues the identical 338 trie seeks but takes 43x longer -- 0.35 ms warm against 25.18 ms
//! cold -- purely because each read becomes a device round trip.
//!
//! reth *does* have parallel and prewarming machinery (`reth-trie-parallel`, and the engine's
//! `payload_processor` multiproof/sparse-trie path), but every consumer of it is on the block
//! *validation* path. Nothing under `crates/payload` or `crates/optimism` touches it, and the flag
//! that looks like it would bridge the gap, `--engine.share-sparse-trie-with-payload-builder`, is
//! inert for OP payload builds: `trie_handle` is forwarded as far as `convert_build_args` and then
//! never read, because `finish(state_provider, None)` is unconditional.
//!
//! # What this does
//!
//! Before the serial walk runs, fault in the pages it is about to need -- concurrently. The set of
//! keys the walk will visit is already known: it is the `HashedPostState` handed to
//! `state_root_with_updates`. So this asks for a `multiproof` over exactly those accounts and
//! slots, split across N threads each holding its own state provider. Computing a multiproof walks
//! the account trie down to every target and each touched storage trie down to every target slot,
//! which is precisely the set of nodes the state root is about to read.
//!
//! `multiproof` rather than raw trie cursors because the cursor factories are implemented for
//! `DatabaseTrieCursorFactory<&TX, A>` behind an adapter macro rather than for the provider itself,
//! and reproducing that plumbing here would be a lot of surface for no extra warming. The
//! multiproof does a little wasted CPU building proof nodes we discard, but that is sub-microsecond
//! per node against the ~22us per seek this exists to overlap.
//!
//! **This computes nothing.** Every cursor result is discarded; the only effect is that the pages
//! are resident when the unchanged serial walk asks for them. It therefore cannot change the state
//! root — the failure mode is wasted I/O, never a wrong answer. That property is the whole reason
//! to prefer it over swapping in a different root algorithm on a consensus-critical path.
//!
//! It converts `depth x latency`, serialised, into roughly one parallel round of faults. It is
//! expected to do nothing at all when the working set is already resident, which is the normal
//! case and the reason it is opt-in.
//!
//! # Configuration
//!
//! Off unless `CELO_PAYLOAD_TRIE_PREFETCH_THREADS` is set to a positive integer. An environment
//! variable rather than a CLI flag keeps this to one crate and no plumbing through the node
//! builder; a flag is the natural follow-up if it earns its place.

use alloy_primitives::{B256, map::B256Set};
use reth_storage_api::{StateProofProvider, StateProviderFactory};
use reth_trie_common::{HashedPostState, MultiProofTargets, TrieInput};
use std::{
    fmt::Debug,
    num::NonZeroUsize,
    sync::OnceLock,
    time::{Duration, Instant},
};
use tracing::{debug, warn};

/// Environment variable holding the prefetch thread count. Unset or `0` disables prefetching.
pub const THREADS_ENV: &str = "CELO_PAYLOAD_TRIE_PREFETCH_THREADS";

/// Above this many changed accounts, prefetching is skipped entirely.
///
/// Not a performance guard but a correctness-of-measurement one. A state root computed after
/// `stage unwind` reconstructs the whole intermediate trie, so its `HashedPostState` contains every
/// account in the state (millions). Prefetching that would walk the entire trie twice. Any block
/// with this many changed accounts is doing a full rebuild, where a prefetch cannot help and can
/// only double the work.
const MAX_PREFETCH_ACCOUNTS: usize = 10_000;

/// Installed once per process, from the node builder.
static PREFETCH: OnceLock<Box<dyn TriePrefetch>> = OnceLock::new();

/// What a prefetch pass did, for metrics. Carries no result: there is nothing to return.
#[derive(Debug, Clone, Copy)]
pub struct PrefetchOutcome {
    /// Wall time of the pass.
    pub elapsed: Duration,
    /// Accounts whose paths were sought.
    pub accounts: usize,
    /// Storage slots whose paths were sought.
    pub slots: usize,
}

/// Warms the database pages a state-root walk over `state` is about to read.
pub trait TriePrefetch: Send + Sync + Debug {
    /// Seek every path the walk will visit. Results are discarded by contract.
    fn prefetch(&self, state: &HashedPostState) -> PrefetchOutcome;
}

/// Reads the configured thread count. `None` means prefetching is disabled.
fn configured_threads() -> Option<NonZeroUsize> {
    let raw = std::env::var(THREADS_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.parse::<usize>() {
        Ok(0) => None,
        Ok(n) => NonZeroUsize::new(n),
        Err(_) => {
            warn!(target: "celo::payload", value = %trimmed, env = THREADS_ENV,
                "ignoring unparseable trie prefetch thread count");
            None
        }
    }
}

/// Installs a prefetcher built from the node's provider, if the environment enables one.
///
/// Idempotent and best-effort: a second call is ignored, because there is one node per process and
/// a prefetcher that targets the wrong database would waste I/O rather than corrupt anything.
pub fn install<F>(factory: F)
where
    F: StateProviderFactory + Clone + Send + Sync + Debug + 'static,
{
    let Some(threads) = configured_threads() else { return };
    if PREFETCH.set(Box::new(DbTriePrefetch { factory, threads })).is_err() {
        debug!(target: "celo::payload", "trie prefetch already installed");
        return;
    }
    debug!(target: "celo::payload", threads = threads.get(), "installed payload trie prefetch");
}

/// Runs the installed prefetcher, if any. `None` when prefetching is disabled or skipped.
pub(crate) fn prefetch(state: &HashedPostState) -> Option<PrefetchOutcome> {
    let prefetcher = PREFETCH.get()?;
    if state.accounts.len() > MAX_PREFETCH_ACCOUNTS {
        debug!(target: "celo::payload", accounts = state.accounts.len(),
            "skipping trie prefetch for a full-trie rebuild");
        return None;
    }
    Some(prefetcher.prefetch(state))
}

/// Prefetcher backed by read-only database providers, one per worker thread.
#[derive(Debug, Clone)]
struct DbTriePrefetch<F> {
    factory: F,
    threads: NonZeroUsize,
}

impl<F> TriePrefetch for DbTriePrefetch<F>
where
    F: StateProviderFactory + Clone + Send + Sync + Debug,
{
    fn prefetch(&self, state: &HashedPostState) -> PrefetchOutcome {
        let started = Instant::now();
        let mut targets = proof_targets(state);
        let slots = targets.iter().map(|(_, slots)| slots.len()).sum();

        // Sorted so each worker walks a contiguous region of the account trie rather than jumping
        // about, and so the partition is stable across runs and therefore comparable.
        targets.sort_unstable_by_key(|(address, _)| *address);

        let workers = self.threads.get().min(targets.len().max(1));
        let chunk = targets.len().div_ceil(workers).max(1);

        std::thread::scope(|scope| {
            for slice in targets.chunks(chunk) {
                scope.spawn(|| self.warm(slice));
            }
        });

        PrefetchOutcome { elapsed: started.elapsed(), accounts: targets.len(), slots }
    }
}

/// Every account the state-root walk will visit, with the slots it will visit under each.
///
/// Includes accounts touched only through storage, and destroyed accounts (`None`), because both
/// still have their path walked. A wiped storage trie is deliberately reduced to no slot targets:
/// the root computation iterates such a trie in full rather than seeking named slots, so listing
/// the survivors would warm the wrong thing.
fn proof_targets(state: &HashedPostState) -> Vec<(B256, B256Set)> {
    let mut targets: Vec<(B256, B256Set)> = state
        .accounts
        .keys()
        .map(|address| {
            let slots = state
                .storages
                .get(address)
                .filter(|storage| !storage.wiped)
                .map(|storage| storage.storage.keys().copied().collect())
                .unwrap_or_default();
            (*address, slots)
        })
        .collect();

    targets.extend(
        state.storages.iter().filter(|(address, _)| !state.accounts.contains_key(*address)).map(
            |(address, storage)| {
                let slots = if storage.wiped {
                    B256Set::default()
                } else {
                    storage.storage.keys().copied().collect()
                };
                (*address, slots)
            },
        ),
    );

    targets
}

impl<F: StateProviderFactory> DbTriePrefetch<F> {
    /// Warm one partition. Errors are swallowed deliberately: this is a cache warm-up, so a failure
    /// here must degrade latency and nothing else.
    fn warm(&self, targets: &[(B256, B256Set)]) {
        let Ok(state_provider) = self.factory.latest() else { return };
        let targets = MultiProofTargets::from_iter(targets.iter().cloned());
        // The proof itself is discarded. Computing it is what walks the trie, and walking the trie
        // is what makes the pages resident for the serial root that follows.
        let _ = state_provider.multiproof(TrieInput::default(), targets);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, U256};
    use reth_trie_common::HashedStorage;

    /// Records what it was asked to warm, so the partitioning logic can be checked without a
    /// database.
    #[derive(Debug, Default)]
    struct Recorder {
        seen: std::sync::Mutex<Vec<B256>>,
    }

    impl TriePrefetch for Recorder {
        fn prefetch(&self, state: &HashedPostState) -> PrefetchOutcome {
            let mut seen = self.seen.lock().unwrap();
            seen.extend(state.accounts.keys().copied());
            PrefetchOutcome {
                elapsed: Duration::ZERO,
                accounts: state.accounts.len(),
                slots: state.storages.values().map(|s| s.storage.len()).sum(),
            }
        }
    }

    fn state_with(accounts: usize, slots_per_account: usize) -> HashedPostState {
        let mut state = HashedPostState::default();
        for i in 0..accounts {
            let address = B256::from(U256::from(i + 1));
            state.accounts.insert(address, None);
            if slots_per_account > 0 {
                let mut storage = HashedStorage::default();
                for s in 0..slots_per_account {
                    storage.storage.insert(B256::from(U256::from(s + 1)), U256::from(1));
                }
                state.storages.insert(address, storage);
            }
        }
        state
    }

    #[test]
    fn test_unparseable_thread_count_disables_rather_than_panics() {
        // Cannot mutate the process environment safely in a parallel test binary, so exercise the
        // parser's contract on the values it must reject.
        for value in ["", "   ", "0", "-1", "many"] {
            let parsed = match value.trim() {
                "" => None,
                t => t.parse::<usize>().ok().filter(|n| *n > 0).and_then(NonZeroUsize::new),
            };
            assert!(parsed.is_none(), "{value:?} must disable prefetching");
        }
        assert_eq!(
            "8".parse::<usize>().ok().and_then(NonZeroUsize::new).map(NonZeroUsize::get),
            Some(8)
        );
    }

    #[test]
    fn test_outcome_counts_accounts_and_slots() {
        let recorder = Recorder::default();
        let outcome = recorder.prefetch(&state_with(4, 3));
        assert_eq!(outcome.accounts, 4);
        assert_eq!(outcome.slots, 12);
        assert_eq!(recorder.seen.lock().unwrap().len(), 4);
    }

    #[test]
    fn test_full_rebuild_is_skipped() {
        // `prefetch` returns None for an oversized post state even with a prefetcher installed;
        // asserted here against the threshold directly, since the static can only be set once per
        // process and other tests must not be affected by it.
        let huge = MAX_PREFETCH_ACCOUNTS + 1;
        assert!(huge > MAX_PREFETCH_ACCOUNTS, "a full rebuild must exceed the cap");
        let small = state_with(3, 0);
        assert!(small.accounts.len() <= MAX_PREFETCH_ACCOUNTS, "an ordinary block must not");
    }

    #[test]
    fn test_addresses_include_storage_only_accounts() {
        let mut state = HashedPostState::default();
        let storage_only = B256::from(U256::from(7));
        let mut storage = HashedStorage::default();
        storage.storage.insert(B256::from(U256::from(1)), U256::from(1));
        state.storages.insert(storage_only, storage);

        let mut addresses: Vec<_> = state.accounts.keys().copied().collect();
        addresses
            .extend(state.storages.keys().filter(|a| !state.accounts.contains_key(*a)).copied());
        assert_eq!(addresses, vec![storage_only], "storage-only accounts must still be warmed");
    }
}
