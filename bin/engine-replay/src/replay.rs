//! The replay loop: drive a canonical range through the payload **build** path and time it.
//!
//! Why the build path and not `newPayload` validation: with `noTxPool` set, op-reth's builder
//! skips the pool loop but still falls through to `builder.finish(state_provider, None)`, whose
//! `None` arm computes the state root inline. That is the same code the sequencer runs when it
//! seals a block, and the thing whose tail this rig exists to measure.
//!
//! Two properties make the numbers trustworthy:
//!
//! * **The node must be positioned at the parent of the first replayed block.** If the forkchoice
//!   head is an interior block, op-reth builds against a `HistoricalStateProvider` and roots
//!   through a revert overlay — strictly more work than the tip path a sequencer takes, so the
//!   measurement would be of a path production never runs. The preflight check enforces this.
//! * **Each block is measured exactly once per node process.** The payload id is a hash of (parent,
//!   attributes) and a resolved payload is cached by id, so a second pass over the same range on
//!   the same live node returns ready futures and reports near-zero build times. Re-runs need a
//!   rewound datadir and a fresh process.

use crate::{
    NotReplayable,
    archive::ArchivedBlock,
    attrs::{engine_version, payload_attributes},
    engine::{self, BuiltPayload},
    metrics::{self, Sample, Scraper},
    rpc,
};
use alloy_consensus::Header;
use alloy_primitives::B256;
use alloy_rpc_types_engine::ForkchoiceState;
use anyhow::Context;
use jsonrpsee::core::client::ClientT;
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufWriter, Write},
    path::Path,
    time::{Duration, Instant},
};
use tracing::{error, info, warn};

/// How many blocks behind the head to point `finalizedBlockHash`.
///
/// Zero — finalized tracking the head — lets the engine persist and prune freely, which keeps
/// the MDBX write path busy underneath the builds. That is deliberate: contention with
/// persistence is one of the two candidate mechanisms for the state-root tail, so the default
/// should not accidentally switch it off.
pub(crate) const DEFAULT_FINALIZED_LAG: u64 = 0;

/// Idle time inserted between blocks, in milliseconds.
///
/// Defaults to Celo's block time. Pacing turned out to be the single largest effect this driver has
/// on its own measurement, and an unpaced default manufactures a 100x tail out of nothing.
///
/// Measured on the stock dev genesis, release build, 30 blocks, 3 reps per arm against one archive
/// and one datadir (`fcu_build_us`; the tail lives in the forkchoice-update half, not
/// `getPayload`):
///
/// | pace    | arm                       | p50        | max            | bp stalls | saves |
/// |---------|---------------------------|------------|----------------|-----------|-------|
/// | 0       | default                   | 369-475 us | 102.8-111.0 ms | 2         | 2     |
/// | 0       | backpressure off only     | 306-515 us | 12.6-21.4 ms   | 0         | 1     |
/// | 0       | persistence off           | 249-258 us | 0.49-0.56 ms   | 0         | 0     |
/// | 1000 ms | default                   | 618-633 us | 0.98-1.77 ms   | 0         | 10    |
///
/// The last row is the one to internalise. At a real block time persistence runs *five times more
/// often* (10 saves against 2, 1473-1746 ms of save time in total) and yet there is no tail at all,
/// because each save finishes inside its idle window before the gap can grow. The tail is not
/// caused by persistence being expensive; it is caused by a driver that leaves nowhere to put it.
///
/// "backpressure off only" is `--engine.persistence-backpressure-threshold 1000000`, which leaves
/// saves running; "persistence off" is `--engine.persistence-threshold 1000000`.
///
/// **Two additive mechanisms, both gated on there being no idle window.**
///
/// 1. *Blocking*, and it owns the ~100 ms component. When `persistence_gap()` reaches the
///    backpressure threshold (default 16) while a save is in flight, the engine loop stops reading
///    incoming messages and blocks on the persistence receiver — `should_backpressure()` at
///    `tree/mod.rs:506`, the stall at `:554-561`. At pace 0 the first save is still in flight ~15
///    blocks later, so the gap reaches 16 around block 17 and the FCU that arrives next eats the
///    remainder of the save. Measured: exactly 2 stalls per 30-block run totalling 151-175 ms.
///    Disabling only this takes the max from ~105 ms to ~17 ms.
/// 2. *Competition*, which owns the residual 5-20 ms spikes. Saves still cost ~150 ms of device and
///    CPU time on their own thread; removing them takes the max from ~17 ms to ~0.5 ms.
///
/// A caution about how this was arrived at, because the failure mode generalises: the blocking half
/// was initially *dismissed* on the grounds that `backpressure_stall_duration_count` was 0 for
/// every block — a reading taken during a per-block-scraped run, whose max was 25 ms rather than
/// 105 ms. Scraping had slowed each block enough that persistence kept up and the gap never reached
/// 16, so the counter was honestly 0 in a regime that no longer had the phenomenon in it. **Do not
/// read a mechanism counter out of a scraped run and apply it to an unscraped one.** Scrape once
/// after the run for cumulative counters, which is what produced the table above.
///
/// Note also that `--engine.persistence-threshold N` silently moves the backpressure threshold too,
/// since its default is `max(16, 2 * N)` (`node/core/src/args/engine.rs:298-302`). Ablating with
/// that flag alone therefore disables both mechanisms at once and cannot attribute between them.
///
/// One macOS-specific inflation, which matters for how much of this transfers to production: every
/// static-file segment commit calls `File::sync_all` (`nippy-jar/src/writer.rs:322-323, 361, 398`;
/// `static_file/manager.rs:578`), and on Apple targets Rust lowers that to `fcntl(F_FULLFSYNC)`
/// (`std/src/sys/fs/unix.rs:1387`), a full device cache flush. Measured on this volume: 7093 us
/// versus 879 us for a plain `fsync`, an 8x penalty per call. So a save is far more expensive here
/// than on a Linux NVMe host, the gap drains more slowly, and backpressure is correspondingly
/// easier to trigger. Treat persistence figures from this host as an upper bound.
///
/// Give the node its real block time and both mechanisms stop firing.
///
/// Two consequences worth stating plainly:
///
/// * A tail measured at pace 0 says nothing about a sequencer. Hold pacing constant across any
///   comparison, and prefer the real block time as the baseline.
/// * `--engine.suppress-persistence-during-build` does not help here (max 105-112 ms, unchanged),
///   and the reason is instructive: it defers persistence only from the FCU-with-attributes until
///   the next FCU clears the build, a window this driver closes in ~911 us. Persistence then runs
///   anyway and lands in the *following* block's build. That flag is built for a chain whose build
///   occupies most of its block interval — which is a condition a snapshot run has to demonstrate
///   before the flag means anything.
///
/// Scraping is a second, separate perturbation on top of pacing, and a larger one than the idle
/// time it introduces, because it also makes the node render ~800 KB of exposition twice per block.
/// Use scraping to characterise what a build *did* (read counts, phase split), never to compare the
/// latency of two configurations. Both settings are recorded in the run summary so a run cannot be
/// misread later.
pub(crate) const DEFAULT_PACE_MS: u64 = 1000;

/// One replayed block's measurements. One JSON object per line, flushed as it is produced.
#[derive(Debug, Serialize)]
struct BlockTiming {
    /// Block height.
    block: u64,
    /// Transactions handed to the builder in the attributes.
    txs_supplied: usize,
    /// Transactions the produced block actually contains.
    txs_included: usize,
    /// Gas the produced block consumed.
    gas_used: u64,
    /// `engine_forkchoiceUpdatedV3` with attributes. Nominally just enqueues the build — but this
    /// is empirically where the entire tail lives, so do not treat it as bookkeeping. The call
    /// awaits two chained oneshots across four thread/task hops (RPC task, orchestrator task,
    /// engine OS thread, payload-builder-service task), none of which is inside the node's own
    /// build histograms. Scheduling delay on any hop shows up here in full and in no node-side
    /// series.
    fcu_build_us: u64,
    /// `engine_getPayload{V3,V4}`. Awaits the in-flight build. Despite the shape of the API this
    /// is *not* where the observed latency has been: on the dev chain it sits at ~0.3 ms while
    /// `fcu_build_us` reaches 100 ms+. Timing only this call would miss the tail entirely.
    get_payload_us: u64,
    /// `fcu_build_us + get_payload_us` — the end-to-end build latency. Compare this across
    /// configs; neither half alone is meaningful, because the split between them depends on how
    /// much of the build finished inside the FCU handler.
    ///
    /// Worth watching against the node's own build histogram rather than in isolation: a large gap
    /// between this and the node-side figure is scheduling delay across those four hops, which is
    /// a contention signal in its own right (see `DEFAULT_PACE_MS`).
    build_us: u64,
    /// `engine_newPayload{V3,V4}`. Cheap for a locally built block (already in the tree).
    new_payload_us: u64,
    /// The head-advancing `engine_forkchoiceUpdatedV3`, no attributes.
    fcu_advance_us: u64,
    /// Whether the produced block hash equalled canonical.
    hash_match: bool,
    /// Present only on a mismatch, to separate a harness problem from a consensus bug.
    #[serde(skip_serializing_if = "Option::is_none")]
    mismatch: Option<Mismatch>,
    /// Node-side counters that moved across this build, when `--metrics-url` is given. This is
    /// where the trie read count per build lives, which the driver's clock cannot see.
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics: Option<Sample>,
}

/// Field-by-field diagnosis of a block-hash mismatch.
///
/// A mismatch is not automatically a consensus bug. The two harness-side causes to rule out
/// first are visible here: a differing `txs_dropped` means the builder silently skipped a
/// supplied sequencer transaction that failed validation, and a differing `receipts_root` with a
/// matching `state_root` points at execution divergence rather than at the trie.
#[derive(Debug, Serialize)]
struct Mismatch {
    /// Canonical block hash.
    expected_hash: B256,
    /// Hash the node produced.
    actual_hash: B256,
    /// Canonical state root.
    expected_state_root: B256,
    /// State root the node produced.
    actual_state_root: B256,
    /// Canonical receipts root.
    expected_receipts_root: B256,
    /// Receipts root the node produced.
    actual_receipts_root: B256,
    /// Canonical gas used.
    expected_gas_used: u64,
    /// Gas used by the produced block.
    actual_gas_used: u64,
    /// Supplied transactions the builder did not include.
    txs_dropped: usize,
}

/// Machine-readable run summary, printed to stdout as a single JSON object.
///
/// Quantiles are computed exactly from the per-block samples. That is the whole point of the
/// driver measuring for itself: no decaying estimator, no rolling window, no `avg_over_time` of
/// a windowed maximum quietly averaging the peaks away.
///
/// The summary also states the instrument's own configuration (`paced_ms`, `scraped`), because both
/// change the numbers materially — see `pace_ms` — and a run whose pacing is not recorded is not
/// comparable to another.
#[derive(Debug, Serialize)]
pub(crate) struct Summary {
    /// Blocks replayed.
    pub(crate) blocks: usize,
    /// Blocks whose hash matched canonical.
    pub(crate) hash_matches: usize,
    /// Idle time inserted between blocks, in milliseconds.
    paced_ms: u64,
    /// Whether node-side counters were scraped per block.
    scraped: bool,
    /// Arithmetic mean of `build_us`.
    build_us_mean: u64,
    /// Median `build_us`.
    build_us_p50: u64,
    /// 90th percentile `build_us`.
    build_us_p90: u64,
    /// 99th percentile `build_us`.
    build_us_p99: u64,
    /// Largest `build_us`.
    build_us_max: u64,
    /// Ratio of the maximum to the mean — the amplification the rig is trying to reproduce.
    build_tail_ratio: f64,
    /// Wall time of the whole replay.
    wall_us: u64,
}

/// Replay `blocks` against the engine endpoint `client`, writing timings to `out`.
///
/// Returns the summary. Stops at the first hash mismatch: once a produced block differs from
/// canonical, advancing the head would diverge the chain and every later measurement would be
/// taken against state the canonical chain never had.
pub(crate) async fn replay<C: ClientT + Sync>(
    client: &C,
    blocks: &[ArchivedBlock],
    out: &Path,
    finalized_lag: u64,
    pace_ms: u64,
    scraper: Option<&Scraper>,
) -> anyhow::Result<Summary> {
    let first = &blocks[0];
    preflight(client, first).await?;

    let file = File::create(out)
        .with_context(|| format!("failed to create the timings file {}", out.display()))?;
    let mut writer = BufWriter::new(file);

    // Hashes the node is known to have, so a lagged `finalizedBlockHash` never names a block the
    // node has never seen.
    let mut known: BTreeMap<u64, B256> = BTreeMap::new();
    known.insert(first.number - 1, first.parent_hash);

    let mut samples: Vec<u64> = Vec::with_capacity(blocks.len());
    let mut hash_matches = 0usize;
    let run_start = Instant::now();

    for block in blocks {
        let header = block.decode_header()?;
        let version = engine_version(&header)?;
        let attributes = payload_attributes(&header, block.transactions.clone())?;

        // Build on top of the parent, which the preflight check and the previous iteration
        // guarantee is the node's current head.
        let build_state =
            forkchoice_state(&known, block.number - 1, block.parent_hash, finalized_lag);

        // Sampled outside every timed span, so scraping cannot inflate a measurement.
        let counters_before = match scraper {
            Some(scraper) => Some(scraper.sample().await?),
            None => None,
        };

        let started = Instant::now();
        let updated = engine::forkchoice_updated_v3(client, build_state, Some(attributes)).await?;
        let fcu_build_us = started.elapsed().as_micros() as u64;
        let payload_id = updated.payload_id.with_context(|| {
            format!(
                "engine_forkchoiceUpdatedV3 for block {} accepted the attributes but returned no \
                 payloadId",
                block.number,
            )
        })?;

        let started = Instant::now();
        let built = engine::get_payload(client, version, payload_id).await?;
        let get_payload_us = started.elapsed().as_micros() as u64;
        let build_us = fcu_build_us + get_payload_us;

        // Sampled straight after the build resolves, still outside every timed span, so the delta
        // brackets exactly this block's build.
        let counters = match (scraper, &counters_before) {
            (Some(scraper), Some(before)) => Some(metrics::delta(before, &scraper.sample().await?)),
            _ => None,
        };

        let hash_match = built.block_hash() == block.hash;
        let mismatch = (!hash_match).then(|| diagnose(&header, &built, block.transactions.len()));

        // Only advance once the produced block is known to be canonical.
        let (new_payload_us, fcu_advance_us) = if hash_match {
            let started = Instant::now();
            engine::new_payload(client, &built).await?;
            let new_payload_us = started.elapsed().as_micros() as u64;

            let advance_state = advance_head(&mut known, block.number, block.hash, finalized_lag);
            let started = Instant::now();
            engine::forkchoice_updated_v3(client, advance_state, None).await?;
            (new_payload_us, started.elapsed().as_micros() as u64)
        } else {
            (0, 0)
        };

        let timing = BlockTiming {
            block: block.number,
            txs_supplied: block.transactions.len(),
            txs_included: built.tx_count(),
            gas_used: built.gas_used(),
            fcu_build_us,
            get_payload_us,
            build_us,
            new_payload_us,
            fcu_advance_us,
            hash_match,
            mismatch,
            metrics: counters,
        };
        serde_json::to_writer(&mut writer, &timing)?;
        writer.write_all(b"\n")?;
        // Flush per block: a rig that is killed mid-run must keep the samples it took.
        writer.flush()?;

        if !hash_match {
            report_mismatch(&timing);
            writer.flush()?;
            return Err(anyhow::Error::new(MismatchStop));
        }

        samples.push(build_us);
        hash_matches += 1;

        // Held constant across a matrix, or swept deliberately. See DEFAULT_PACE_MS.
        if pace_ms > 0 {
            tokio::time::sleep(Duration::from_millis(pace_ms)).await;
        }
        if block.number % 100 == 0 {
            info!(
                block = block.number,
                build_ms = build_us as f64 / 1_000.0,
                txs = timing.txs_included,
                "Replayed"
            );
        }
    }

    writer.flush()?;
    Ok(summarise(
        samples,
        hash_matches,
        run_start.elapsed().as_micros() as u64,
        pace_ms,
        scraper.is_some(),
    ))
}

/// Marker error meaning "the correctness gate failed", mapped to its own exit code.
#[derive(Debug)]
pub(crate) struct MismatchStop;

impl std::fmt::Display for MismatchStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a replayed block did not reproduce its canonical hash")
    }
}

impl std::error::Error for MismatchStop {}

/// Refuse to start unless the node is on the right chain and parked exactly at the parent of the
/// first archived block.
async fn preflight<C: ClientT + Sync>(client: &C, first: &ArchivedBlock) -> anyhow::Result<()> {
    let node_chain_id = rpc::chain_id(client).await?;
    if node_chain_id != first.chain_id {
        return Err(anyhow::Error::new(NotReplayable(format!(
            "the archive was taken from chain {} but the node reports chain {node_chain_id}; \
             replaying it would silently change fork times and the base-fee floor",
            first.chain_id,
        ))));
    }

    let (head_number, head_hash, _) = rpc::canonical_head(client).await?;
    let want_number = first.number - 1;
    if head_number != want_number || head_hash != first.parent_hash {
        return Err(anyhow::Error::new(NotReplayable(format!(
            "the node's head is block {head_number} ({head_hash}) but the archive starts at block \
             {}, whose parent is block {want_number} ({}). Rewind the datadir first:\n    \
             celo-reth stage unwind --datadir <DIR> --chain <CHAIN> to-block {want_number}\n\
             Building on an interior block would route the state root through the historical \
             provider instead of the tip path the sequencer uses, so the measurement would not be \
             of the code under test.",
            first.number, first.parent_hash,
        ))));
    }

    info!(chain_id = node_chain_id, head = head_number, "Node is positioned for replay");
    Ok(())
}

/// Record a newly canonical block as the head, then derive the forkchoice state that advances to
/// it.
///
/// The insert has to happen *before* the state is derived, or a lag of 0 finalizes a stale
/// ancestor instead of the new head. `known` is trimmed to the lag window so a long range does not
/// accumulate a hash per block.
fn advance_head(
    known: &mut BTreeMap<u64, B256>,
    number: u64,
    hash: B256,
    lag: u64,
) -> ForkchoiceState {
    known.insert(number, hash);
    while known.len() > lag as usize + 2 {
        let oldest = *known.keys().next().expect("map is non-empty");
        known.remove(&oldest);
    }
    forkchoice_state(known, number, hash, lag)
}

/// Build a forkchoice state with `finalized` (and `safe`) lagging the head by `lag` blocks.
fn forkchoice_state(
    known: &BTreeMap<u64, B256>,
    head_number: u64,
    head_hash: B256,
    lag: u64,
) -> ForkchoiceState {
    let target = head_number.saturating_sub(lag);
    let anchor = known.range(..=target).next_back().map_or(head_hash, |(_, hash)| *hash);
    ForkchoiceState {
        head_block_hash: head_hash,
        safe_block_hash: anchor,
        finalized_block_hash: anchor,
    }
}

/// Compare the produced block against canonical, field by field.
fn diagnose(canonical: &Header, built: &BuiltPayload, txs_supplied: usize) -> Mismatch {
    Mismatch {
        expected_hash: canonical.hash_slow(),
        actual_hash: built.block_hash(),
        expected_state_root: canonical.state_root,
        actual_state_root: built.state_root(),
        expected_receipts_root: canonical.receipts_root,
        actual_receipts_root: built.receipts_root(),
        expected_gas_used: canonical.gas_used,
        actual_gas_used: built.gas_used(),
        txs_dropped: txs_supplied.saturating_sub(built.tx_count()),
    }
}

/// Log a mismatch with the interpretation a reader needs, so it is not mistaken for a consensus
/// bug when it is a harness problem.
fn report_mismatch(timing: &BlockTiming) {
    let Some(mismatch) = &timing.mismatch else { return };
    error!(
        block = timing.block,
        expected = %mismatch.expected_hash,
        actual = %mismatch.actual_hash,
        "Replayed block did not reproduce its canonical hash"
    );
    if mismatch.txs_dropped > 0 {
        warn!(
            dropped = mismatch.txs_dropped,
            supplied = timing.txs_supplied,
            "The builder skipped supplied sequencer transactions. This is a harness or replay-\
             position problem, not a state-root bug: transactions that fail validation are \
             silently dropped and change transactionsRoot, gasUsed and stateRoot together."
        );
    } else if mismatch.actual_state_root != mismatch.expected_state_root &&
        mismatch.actual_receipts_root == mismatch.expected_receipts_root
    {
        error!(
            expected_state_root = %mismatch.expected_state_root,
            actual_state_root = %mismatch.actual_state_root,
            "Execution agreed (matching receiptsRoot) but the state root differs. This is the \
             shape a genuine state-root bug takes."
        );
    } else {
        error!(
            expected_receipts_root = %mismatch.expected_receipts_root,
            actual_receipts_root = %mismatch.actual_receipts_root,
            expected_gas_used = mismatch.expected_gas_used,
            actual_gas_used = mismatch.actual_gas_used,
            "Execution itself diverged."
        );
    }
}

/// Exact quantiles over the collected samples.
fn summarise(
    mut samples: Vec<u64>,
    hash_matches: usize,
    wall_us: u64,
    paced_ms: u64,
    scraped: bool,
) -> Summary {
    let blocks = samples.len();
    samples.sort_unstable();
    let mean = if blocks == 0 {
        0
    } else {
        (samples.iter().map(|s| u128::from(*s)).sum::<u128>() / blocks as u128) as u64
    };
    let max = samples.last().copied().unwrap_or_default();
    Summary {
        blocks,
        hash_matches,
        paced_ms,
        scraped,
        build_us_mean: mean,
        build_us_p50: percentile(&samples, 0.50),
        build_us_p90: percentile(&samples, 0.90),
        build_us_p99: percentile(&samples, 0.99),
        build_us_max: max,
        build_tail_ratio: if mean == 0 { 0.0 } else { max as f64 / mean as f64 },
        wall_us,
    }
}

/// Nearest-rank percentile of a sorted slice.
fn percentile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[rank]
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::b256;

    const A: B256 = b256!("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    const B: B256 = b256!("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

    #[test]
    fn test_zero_lag_finalizes_the_head() {
        let mut known = BTreeMap::new();
        known.insert(10, A);
        let state = forkchoice_state(&known, 10, A, 0);
        assert_eq!(state.head_block_hash, A);
        assert_eq!(state.finalized_block_hash, A);
        assert_eq!(state.safe_block_hash, A);
    }

    #[test]
    fn test_lag_points_at_a_block_the_node_has() {
        let mut known = BTreeMap::new();
        known.insert(10, A);
        known.insert(11, B);
        let state = forkchoice_state(&known, 11, B, 1);
        assert_eq!(state.head_block_hash, B);
        assert_eq!(state.finalized_block_hash, A, "lag 1 finalizes block 10");
    }

    /// A lag reaching below the oldest known block must fall back to the head rather than name a
    /// hash the node has never seen.
    #[test]
    fn test_lag_below_the_anchor_falls_back_to_the_head() {
        let mut known = BTreeMap::new();
        known.insert(10, A);
        let state = forkchoice_state(&known, 10, A, 1_000);
        assert_eq!(state.finalized_block_hash, A);
    }

    /// Advancing must record the new head *before* deriving the state, otherwise a lag of 0
    /// finalizes the previous block forever — which is silent, since the resulting forkchoice
    /// state is still valid.
    #[test]
    fn test_advancing_finalizes_the_new_head_not_its_parent() {
        let anchor = b256!("0x0000000000000000000000000000000000000000000000000000000000000009");
        let mut known = BTreeMap::from([(9u64, anchor)]);

        let state = advance_head(&mut known, 10, A, 0);
        assert_eq!(state.head_block_hash, A);
        assert_eq!(state.finalized_block_hash, A, "lag 0 must finalize block 10, not block 9");

        let state = advance_head(&mut known, 11, B, 0);
        assert_eq!(state.finalized_block_hash, B);
    }

    /// With a lag, advancing keeps exactly the window it needs and no more.
    #[test]
    fn test_advancing_trims_to_the_lag_window() {
        let mut known = BTreeMap::from([(0u64, B256::ZERO)]);
        for number in 1..=200u64 {
            let hash = B256::with_last_byte(number as u8);
            let state = advance_head(&mut known, number, hash, 3);
            assert_eq!(state.head_block_hash, hash);
            if number > 3 {
                assert_eq!(
                    state.finalized_block_hash,
                    B256::with_last_byte((number - 3) as u8),
                    "lag 3 must finalize three blocks back"
                );
            }
        }
        assert!(known.len() <= 5, "the map must stay bounded, got {}", known.len());
    }

    #[test]
    fn test_percentiles_are_exact_nearest_rank() {
        // Odd length: the median is unambiguous.
        let sorted = vec![1, 2, 3, 4, 5, 6, 7, 8, 900];
        assert_eq!(percentile(&sorted, 0.0), 1);
        assert_eq!(percentile(&sorted, 0.5), 5);
        assert_eq!(percentile(&sorted, 1.0), 900);
        assert_eq!(percentile(&[], 0.5), 0);

        // Even length: nearest rank rounds half away from zero, so it reports the upper of the
        // two middle samples rather than interpolating. Deliberate — a rig should never invent a
        // value that was not measured.
        let sorted = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 100];
        assert_eq!(percentile(&sorted, 0.5), 6);
        assert_eq!(percentile(&sorted, 1.0), 100);
    }

    /// The tail ratio is the quantity the plan's calibration target is written in, so it must be
    /// computed from the raw samples rather than from any windowed estimate.
    #[test]
    fn test_summary_reports_the_tail_ratio() {
        // Nine fast blocks and one 100x outlier: mean 19, max 100.
        let summary =
            summarise(vec![10; 9].into_iter().chain([100]).collect(), 10, 1_000, 0, false);
        assert_eq!(summary.blocks, 10);
        assert_eq!(summary.build_us_mean, 19);
        assert_eq!(summary.build_us_max, 100);
        assert!((summary.build_tail_ratio - 100.0 / 19.0).abs() < 1e-9);
    }
}
