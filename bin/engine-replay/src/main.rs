//! `engine-replay` — replay a canonical Celo L2 block range through celo-reth's payload **build**
//! path over the authenticated engine API, and record exact per-block timings.
//!
//! This is component 1 of `sequencer-perf-test-rig-plan.md`: a local stand-in for the mainnet
//! sequencer's build loop, so "does this change remove the state-root tail?" can be answered in
//! minutes instead of a deployment cycle.
//!
//! # Two steps, in this order
//!
//! ```text
//! engine-replay archive --rpc-url http://127.0.0.1:8545 --from A --to B --out blocks.jsonl
//! celo-reth stage unwind --datadir DIR --chain CHAIN to-block <A-1>
//! engine-replay replay --archive blocks.jsonl --engine-url http://127.0.0.1:8551 \
//!     --jwt DIR/jwt.hex --out timings.jsonl
//! ```
//!
//! Archiving comes first because `stage unwind` deletes the headers and bodies above its target,
//! which are the only local source of the attributes and expected hashes. After archiving, the
//! replay is fully offline.
//!
//! # Running the node under test
//!
//! ```text
//! celo-reth node --chain CHAIN --datadir DIR \
//!     --authrpc.addr 127.0.0.1 --authrpc.port 8551 \
//!     --disable-discovery --max-peers 0 --no-persist-peers --rollup.disable-tx-pool-gossip \
//!     --builder.deadline 600 \
//!     --metrics 127.0.0.1:9001
//! ```
//!
//! Not `--dev`: that spawns reth's local miner, which drives forkchoice itself and races the
//! driver. And `--builder.deadline` must be raised well above its 12 s default — under injected
//! read latency a single build can exceed it, after which the job is dropped and `getPayload`
//! fails. Leaving the default in place biases the matrix, because the slow configuration is the
//! one that trips it.
//!
//! # Exit codes
//!
//! * `0` — every block replayed and reproduced its canonical hash.
//! * `1` — an operational error (RPC unreachable, bad JWT, malformed archive).
//! * `3` — the correctness gate failed: a produced block's hash differed from canonical.
//! * `4` — the range is not replayable, or the node is not positioned for it. Never a node bug.

mod archive;
mod attrs;
mod engine;
mod metrics;
mod replay;
mod rpc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use std::{path::PathBuf, process::ExitCode, time::Duration};
use tracing::{error, info, warn};

/// Exit code for a failed correctness gate.
const EXIT_MISMATCH: u8 = 3;

/// Exit code for "this range cannot be replayed here", as opposed to "the node got it wrong".
const EXIT_NOT_REPLAYABLE: u8 = 4;

/// A precondition that makes the requested replay impossible, carrying its own exit code so a
/// caller can tell it apart from a genuine divergence.
#[derive(Debug)]
pub(crate) struct NotReplayable(pub(crate) String);

impl std::fmt::Display for NotReplayable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotReplayable {}

/// Replay canonical Celo L2 blocks through the engine API's build path, with per-block timings.
#[derive(Debug, Parser)]
#[command(name = "engine-replay", version, about, long_about = None)]
struct Cli {
    /// Log level: error, warn, info, debug or trace. Logs go to stderr; the run summary goes to
    /// stdout as one JSON object.
    #[arg(long, default_value = "info", global = true)]
    log_level: tracing::Level,

    /// JSON-RPC request timeout, in seconds. Must exceed the slowest expected single build.
    #[arg(long, default_value_t = 300, global = true)]
    timeout_secs: u64,

    /// What to do.
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Record a canonical block range from a synced node into a replay archive.
    ///
    /// Must run before the datadir is rewound. Needs the `debug` namespace
    /// (`--http.api eth,debug`). Every block is checked for replayability as it is archived, so a
    /// range that cannot be reproduced fails here rather than during a measurement run.
    Archive {
        /// Public JSON-RPC endpoint of a node that has the range, with `eth` and `debug` enabled.
        #[arg(long)]
        rpc_url: String,

        /// First block to archive, inclusive. Must be at least 1.
        #[arg(long)]
        from: u64,

        /// Last block to archive, inclusive.
        #[arg(long)]
        to: u64,

        /// Where to write the JSONL archive.
        #[arg(long)]
        out: PathBuf,
    },

    /// Replay an archive against a node positioned at the parent of its first block.
    ///
    /// The node must be rewound to that parent first; building on an interior block would route
    /// the state root through the historical provider rather than the tip path the sequencer
    /// uses. A given range can only be measured once per node process, because a resolved
    /// payload is cached by payload id — re-runs need a rewound datadir and a restart.
    Replay {
        /// Archive produced by `engine-replay archive`.
        #[arg(long)]
        archive: PathBuf,

        /// Authenticated engine API endpoint, e.g. `http://127.0.0.1:8551`.
        #[arg(long)]
        engine_url: String,

        /// Engine JWT secret. reth writes it to `<datadir>/jwt.hex` unless
        /// `--authrpc.jwtsecret` says otherwise.
        #[arg(long)]
        jwt: PathBuf,

        /// Where to write the per-block timings as JSONL.
        #[arg(long)]
        out: PathBuf,

        /// How many blocks behind the head to point `finalizedBlockHash` and `safeBlockHash`.
        ///
        /// The default finalizes the head, which lets the engine persist and prune freely and so
        /// keeps write pressure on underneath the builds. Raise it to hold blocks in memory
        /// instead.
        #[arg(long, default_value_t = replay::DEFAULT_FINALIZED_LAG)]
        finalized_lag: u64,

        /// Idle time to insert between blocks, in milliseconds. Defaults to Celo's block time.
        ///
        /// The single largest effect this driver has on its own measurement, so it must be held
        /// constant across any comparison. Replaying flat out (`--pace-ms 0`) never leaves an idle
        /// window for MDBX persistence, which then overlaps every build: on the dev chain that
        /// alone took max(fcu_build_us) from ~1.2 ms to 96-163 ms, a tail with no trie pressure
        /// behind it whatsoever. Ablating persistence at pace 0 removes it entirely. Use 0 for a
        /// fast functional check, never for a latency figure. Recorded in the run summary.
        #[arg(long, default_value_t = replay::DEFAULT_PACE_MS)]
        pace_ms: u64,

        /// Prometheus endpoint of the node under test, e.g. `http://127.0.0.1:9001`.
        ///
        /// When given, each block's record gains the node-side counters that moved across its
        /// build — which is where the trie read count lives. Sampled twice per block, always
        /// outside the timed spans, so it cannot inflate a measurement. Plain HTTP only.
        #[arg(long)]
        metrics_url: Option<String>,

        /// Series-name substring to keep. Repeatable. Defaults to `trie` and `payload`; pass
        /// `--metrics-filter ''` to record every series that moved.
        #[arg(long)]
        metrics_filter: Vec<String>,

        /// Also write the full unfiltered exposition text, before and after the replay, here.
        ///
        /// Two scrapes rather than one per block, so it stays small while keeping the whole metric
        /// surface available for offline exploration.
        #[arg(long, requires = "metrics_url")]
        metrics_raw: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt().with_max_level(cli.log_level).with_writer(std::io::stderr).init();

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        // The mismatch case has already logged its own diagnosis, which is more useful than the
        // error chain, so it is not re-printed here.
        Err(err) => match err.downcast_ref::<NotReplayable>() {
            Some(reason) => {
                error!("Not replayable: {reason}");
                ExitCode::from(EXIT_NOT_REPLAYABLE)
            }
            None if err.downcast_ref::<replay::MismatchStop>().is_some() => {
                ExitCode::from(EXIT_MISMATCH)
            }
            None => {
                error!("{err:?}");
                ExitCode::FAILURE
            }
        },
    }
}

/// Dispatch the chosen subcommand.
async fn run(cli: Cli) -> anyhow::Result<()> {
    let timeout = Duration::from_secs(cli.timeout_secs);
    match cli.command {
        Command::Archive { rpc_url, from, to, out } => {
            let client = rpc::plain_client(&rpc_url, timeout)?;
            archive::archive(&client, from, to, &out).await
        }
        Command::Replay {
            archive: path,
            engine_url,
            jwt,
            out,
            finalized_lag,
            pace_ms,
            metrics_url,
            metrics_filter,
            metrics_raw,
        } => {
            let blocks = archive::load(&path)?;
            let client = rpc::engine_client(&engine_url, &jwt, timeout)?;

            let scraper = metrics_url.map(|url| {
                // An explicit empty `--metrics-filter ''` means "keep everything"; an omitted flag
                // means "the useful default".
                let filters = if metrics_filter.is_empty() {
                    metrics::DEFAULT_FILTERS.iter().map(|f| (*f).to_string()).collect()
                } else {
                    metrics_filter.into_iter().filter(|f| !f.is_empty()).collect()
                };
                metrics::Scraper::new(url, filters)
            });

            // Fail fast if the endpoint is wrong: a whole run whose metrics silently did not
            // record is exactly the outcome this feature exists to prevent.
            if let Some(scraper) = &scraper {
                let raw = scraper.raw().await?;
                info!(bytes = raw.len(), "Metrics endpoint reachable");
                if let Some(path) = &metrics_raw {
                    std::fs::write(path, &raw).with_context(|| {
                        format!("failed to write {} for the pre-run scrape", path.display())
                    })?;
                }
            }

            let summary =
                replay::replay(&client, &blocks, &out, finalized_lag, pace_ms, scraper.as_ref())
                    .await;

            // Written even when the replay failed: a post-mortem wants the counters most then.
            if let (Some(scraper), Some(path)) = (&scraper, &metrics_raw) {
                match scraper.raw().await {
                    Ok(raw) => {
                        let mut text = std::fs::read_to_string(path).unwrap_or_default();
                        text.push_str("\n# ---- engine-replay: post-run scrape ----\n");
                        text.push_str(&raw);
                        if let Err(err) = std::fs::write(path, text) {
                            warn!(
                                "failed to append the post-run scrape to {}: {err}",
                                path.display()
                            );
                        }
                    }
                    Err(err) => warn!("post-run scrape failed: {err:?}"),
                }
            }

            let summary = summary?;
            println!(
                "{}",
                serde_json::to_string(&summary).context("failed to render the run summary")?
            );
            Ok(())
        }
    }
}
