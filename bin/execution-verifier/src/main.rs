//! Execution verifier for Celo Kona
//!
//! This binary provides execution verification functionality for the Celo Kona project.

mod metrics;
mod rpc_timeout;

use alloy_celo_evm::CeloEvmFactory;
use alloy_network::Ethereum;
use alloy_primitives::{B256, Bytes, Sealable};
use alloy_provider::{
    Provider, ProviderBuilder, RootProvider, network::primitives::BlockTransactions,
};
use alloy_pubsub::Subscription;
use alloy_rlp::Decodable;
use alloy_rpc_client::ClientBuilder;
use alloy_rpc_types_engine::PayloadAttributes;
use alloy_rpc_types_eth::Header;
use alloy_transport_ipc;
use alloy_transport_ws;
use anyhow::Result;
use celo_alloy_rpc_types_engine::CeloPayloadAttributes;
use celo_executor::CeloStatelessL2Builder;
use celo_otel::{logger::init_tracing, metrics::build_meter_provider, resource::build_resource};
use celo_registry::ROLLUP_CONFIGS;
use clap::{ArgAction, Parser};
use futures::stream::StreamExt;
use kona_executor::TrieDBProvider;
use kona_mpt::{NoopTrieHinter, TrieNode, TrieProvider};
use metrics::Metrics;
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use opentelemetry::global;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicUsize},
};
use tokio::{
    runtime::Handle,
    sync::Mutex,
    sync::mpsc,
    task::JoinSet,
    time::{Duration, Instant, interval},
};

use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

mod verified_block_tracker;
use verified_block_tracker::VerifiedBlockTracker;

use rpc_timeout::RpcTimeoutLayer;

const PERSISTANCE_INTERVAL: Duration = Duration::from_secs(10);
const RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// the version string injected by Cargo at compile time
pub const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

/// the packag name string injected by Cargo at compile time
pub const PKG_NAME: &str = env!("CARGO_PKG_NAME");

/// The execution verifier command
#[derive(Parser, Debug, Clone)]
#[command(
    about = "Verifies local execution for a range of blocks retrieved from the provided L2 RPC"
)]
pub struct ExecutionVerifierCommand {
    /// Verbosity level (0-5).
    /// If set to 0, no logs are printed.
    /// By default, the verbosity level is set to 3 (info level).
    #[arg(long, short, default_value = "3", action = ArgAction::Count)]
    pub v: u8,
    /// The rpc connection to use, can be either an http or https URL or a file path to an IPC
    /// socket.
    #[arg(long)]
    pub l2_rpc: String,
    /// L2 inclusive starting block number to execute.
    #[arg(long)]
    pub start_block: Option<u64>,
    /// L2 inclusive ending block number to execute.
    #[arg(long)]
    pub end_block: Option<u64>,
    /// Number of concurrent tasks to run.
    #[arg(long, default_value = "25")]
    pub concurrency: usize,
    /// File that persists the highest verified block number.
    /// If this exists already, the persisted number will be
    /// read and overwrite the start-block option
    #[arg(long)]
    pub state_file: Option<PathBuf>,
    /// enable otel metrics and log exports, if true then all the OTEL_
    /// env-vars can be used as per the standard
    #[arg(long, default_value_t = false)]
    pub telemetry: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = ExecutionVerifierCommand::parse();

    if let Some(start_block) = cli.start_block {
        if start_block == 0 {
            return Err(anyhow::anyhow!(
                "start_block {start_block} must be > 0 (need parent block)"
            ));
        }
        if let Some(end_block) = cli.end_block
            && start_block > end_block
        {
            return Err(anyhow::anyhow!(
                "start_block {start_block} must be <= end_block {end_block}"
            ));
        }
    } else if let Some(end_block) = cli.end_block {
        return Err(anyhow::anyhow!("end-block {end_block} provided without start-block"));
    }

    // Construct the OTel resources.
    let otel_resource = build_resource(PKG_NAME.to_string(), PKG_VERSION.to_string());

    // set up the metrics
    // If cli.telemetry is false this will not use any OTLP exporters
    global::set_meter_provider(build_meter_provider(otel_resource.clone(), cli.telemetry));

    // This filter shows all logs from this code but doesn't show logs from the block builder,
    // since it is very verbose.
    // set up the logs
    // If cli.telemetry is false this will not use any OTLP exporters
    let filter = EnvFilter::new("trace").add_directive("block_builder=off".parse().unwrap());
    init_tracing(cli.v, Some(filter), otel_resource.clone(), cli.telemetry)?;

    if cli.telemetry {
        tracing::info!("OTLP export for tracing and metrics enabled");
    }
    let cancel_token = CancellationToken::new();
    run(cli, cancel_token).await
}

async fn run(cli: ExecutionVerifierCommand, cancel_token: CancellationToken) -> anyhow::Result<()> {
    // Use the stored starting block or the CLI-provided one, but only
    // if a persistence-file option is given
    let start_block = match cli.state_file {
        None => cli.start_block,
        Some(ref f) => read_verified_block(f).map_or(cli.start_block, |verified_block| {
            let new_start_block = Some(verified_block + 1);
            tracing::info!(
                persisted_block_number = verified_block,
                cli_start_block_number = cli.start_block,
                start_block_number = new_start_block,
                "Found persisted highest verified block number, overwriting `start-block` argument"
            );
            new_start_block
        }),
    };

    tracing::info!(start_block_number = start_block, "Using start-block");

    // Check if l2_rpc is a URL or a file path and create client with timeout layer
    let provider: RootProvider<Ethereum> = match cli.l2_rpc.as_str() {
        url if url.starts_with("ws://") || url.starts_with("wss://") => {
            let ws_connect = alloy_transport_ws::WsConnect::new(url);
            let client = ClientBuilder::default()
                .layer(RpcTimeoutLayer::new(RPC_TIMEOUT))
                .ws(ws_connect)
                .await?;
            ProviderBuilder::new().connect_client(client).root().clone()
        }
        file_path => {
            if !std::path::Path::new(file_path).exists() {
                return Err(anyhow::anyhow!(
                    "Invalid L2 RPC {}. Only WS/WSS URLs and ipc file paths are supported.",
                    cli.l2_rpc,
                ));
            }
            let ipc_connect = alloy_transport_ipc::IpcConnect::new(file_path.to_string());
            let client = ClientBuilder::default()
                .layer(RpcTimeoutLayer::new(RPC_TIMEOUT))
                .ipc(ipc_connect)
                .await?;
            ProviderBuilder::new().connect_client(client).root().clone()
        }
    };

    let provider = Arc::new(provider);

    let mut handles = JoinSet::new();

    let subscription = provider.subscribe_blocks().await?;

    let chain_id = provider
        .get_chain_id()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get chain ID: {e}"))?;
    let rollup_config = Arc::new(
        ROLLUP_CONFIGS
            .get(&chain_id)
            .ok_or_else(|| anyhow::anyhow!("Rollup config not found for chain ID {chain_id}"))?
            .clone(),
    );

    let tracker = Arc::new(VerifiedBlockTracker::new(start_block));

    // Spawn the repeating task in the background
    let persist_highest_block_task = cli.state_file.clone().map(|state_file| {
        let tracker = tracker.clone();
        let cancel = cancel_token.clone();
        // let state_file = p.clone();
        return tokio::spawn(async move {
            let mut interval = interval(PERSISTANCE_INTERVAL);
            let mut last_verified_block: Option<u64> = None;

            loop {
                tokio::select! {
                   _ = cancel.cancelled() => {
                       // TODO: try persist a last time
                       break;
                   }
                   _ = interval.tick() => {
                       if let Some(cur) = tracker.highest() {
                           // Only persist if changed since last tick
                           if last_verified_block.map_or(true, |prev| prev != cur) {
                               match persist_verified_block(cur, &state_file) {
                                   Ok(()) => {
                                       // Update the cached last value only after a successful persist
                                       last_verified_block = Some(cur);
                                   }
                                   Err(e) => {
                                       eprintln!("Error storing verified block: {e}");
                                       // Leave last_verified_block unchanged so we retry next tick
                                   }
                               }

                           }
                       }
                   }
                }
            }
        });
    });
    let metrics = Arc::new(Mutex::new(Metrics::new(Some(tracker.clone()))));
    if let (Some(start_block), Some(end_block)) = (start_block, cli.end_block) {
        handles.spawn(verify_block_range(
            start_block,
            end_block,
            provider.clone(),
            rollup_config.clone(),
            cli.concurrency,
            cancel_token.clone(),
            metrics.clone(),
            tracker.clone(),
        ));
    } else if let Some(start_block) = start_block {
        // Use dynamic concurrency for verify_new_heads
        // verify_block_range is running => 1
        // verify_block_range completes  => full capacity
        let verify_new_heads_concurrency = Arc::new(AtomicUsize::new(1));

        // Used to communicate the first head block so that we can set the end of the block range.
        let (first_head_tx, mut first_head_rx) = mpsc::channel(1);
        handles.spawn(verify_new_heads(
            provider.clone(),
            rollup_config.clone(),
            subscription,
            cancel_token.clone(),
            Some(first_head_tx.clone()),
            metrics.clone(),
            tracker.clone(),
            verify_new_heads_concurrency.clone(),
        ));
        let first_head_block =
            first_head_rx.recv().await.ok_or_else(|| anyhow::anyhow!("Channel closed"))?;
        let end = first_head_block - 1;
        let concurrency_handle = verify_new_heads_concurrency.clone();
        handles.spawn({
            let cancel_token = cancel_token.clone();
            async move {
                let result = verify_block_range(
                    start_block,
                    end,
                    provider.clone(),
                    rollup_config.clone(),
                    std::cmp::max(1, cli.concurrency - 1),
                    cancel_token.clone(),
                    metrics.clone(),
                    tracker.clone(),
                )
                .await;

                // Restore full concurrency for verify_new_heads after verify_block_range completes.
                concurrency_handle.store(cli.concurrency, std::sync::atomic::Ordering::Relaxed);

                result
            }
        });
    } else {
        handles.spawn(verify_new_heads(
            provider.clone(),
            rollup_config.clone(),
            subscription,
            cancel_token.clone(),
            None,
            metrics.clone(),
            tracker.clone(),
            Arc::new(AtomicUsize::new(cli.concurrency)),
        ));
    };

    // Process results as they complete, cancel on first error
    while let Some(result) = handles.join_next().await {
        match result {
            Ok(Err(e)) => {
                // Cancel any outstanding tasks, and wait for all tasks to finish
                cancel_token.cancel();
                handles.join_all().await;
                return Err(e);
            }
            Err(e) => {
                // Cancel any outstanding tasks, and wait for all tasks to finish
                cancel_token.cancel();
                handles.join_all().await;
                return Err(anyhow::anyhow!("Task panicked: {e}"));
            }
            _ => {}
        }
    }

    if let Some(t) = persist_highest_block_task {
        t.abort();
    };
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn verify_new_heads(
    provider: Arc<RootProvider<Ethereum>>,
    rollup_config: Arc<celo_registry::CeloRollupConfig>,
    subscription: Subscription<Header>,
    cancel_token: CancellationToken,
    first_head_tx: Option<mpsc::Sender<u64>>,
    metrics: Arc<Mutex<Metrics>>,
    tracker: Arc<VerifiedBlockTracker>,
    concurrency: Arc<AtomicUsize>,
) -> Result<()> {
    let mut first_block = true;

    let mut stream = subscription.into_stream();

    let mut last_verified_height: Option<u64> = None;

    while let Some(header) = stream.next().await {
        if cancel_token.is_cancelled() {
            break;
        }
        let num = header.number;

        if first_block {
            if let Some(first_head_tx) = &first_head_tx {
                first_head_tx.send(num).await?;
            }
            first_block = false;
        }

        let (start_block, end_block) = last_verified_height
            .map_or((num, num), |last_verified_height| (last_verified_height + 1, num));

        let current_concurrency = concurrency.load(std::sync::atomic::Ordering::Relaxed);
        if end_block > start_block {
            // Some heights skipped since last iteration
            tracing::info!(
                last_verified = start_block - 1,
                latest = end_block,
                gap = end_block - start_block + 1, // includes the latest head
                concurrency = current_concurrency,
                "Subscription gap detected, backfilling missing blocks"
            );
        }

        // XXX: why clone here?
        let result = verify_block_range(
            start_block,
            end_block,
            provider.clone(),
            rollup_config.clone(),
            current_concurrency,
            cancel_token.clone(),
            metrics.clone(),
            tracker.clone(),
        )
        .await;
        match result {
            Ok(_) => tracing::info!(block_number = header.number, "Head block verified"),
            Err(e) => tracing::warn!(
                block_number = header.number,
                "Head block verification failed: {}",
                e
            ),
        }
        last_verified_height = Some(num);
    }

    Ok(())
}

/// Verifies execution for a single block
async fn verify_block(
    block_number: u64,
    provider: Arc<RootProvider<Ethereum>>,
    rollup_config: Arc<celo_registry::CeloRollupConfig>,
    metrics: Arc<Mutex<Metrics>>,
    tracker: Arc<VerifiedBlockTracker>,
) -> Result<u64> {
    let start = Instant::now();
    tracing::debug!(block_number = block_number, "requested to verify block");

    // Fetch parent block
    let parent_block = provider
        .get_block_by_number((block_number - 1).into())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get parent block {}: {}", block_number - 1, e))?
        .ok_or_else(|| anyhow::anyhow!("Parent block {} not found", block_number - 1))?;
    let parent_header = parent_block.header.inner.seal_slow();

    // Fetch executing block
    let executing_block = provider
        .get_block_by_number(block_number.into())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get executing block {block_number}: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("Executing block {block_number} not found"))?;

    let encoded_executing_transactions = match executing_block.transactions {
        BlockTransactions::Hashes(transactions) => {
            let mut encoded_transactions = Vec::with_capacity(transactions.len());
            for tx_hash in transactions {
                let tx = provider
                    .client()
                    .request::<&[B256; 1], Bytes>("debug_getRawTransaction", &[tx_hash])
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to get raw transaction {tx_hash}: {e}"))?;
                encoded_transactions.push(tx);
            }
            encoded_transactions
        }
        _ => panic!("Only BlockTransactions::Hashes are supported."),
    };

    let executing_header = executing_block.header.clone();

    let payload_attrs = CeloPayloadAttributes {
        op_payload_attributes: OpPayloadAttributes {
            payload_attributes: PayloadAttributes {
                timestamp: executing_header.timestamp,
                parent_beacon_block_root: executing_header.parent_beacon_block_root,
                prev_randao: executing_header.mix_hash,
                withdrawals: Default::default(),
                suggested_fee_recipient: executing_header.beneficiary,
            },
            gas_limit: Some(executing_header.gas_limit),
            transactions: Some(encoded_executing_transactions),
            no_tx_pool: None,
            eip_1559_params: rollup_config
                .is_holocene_active(executing_header.timestamp)
                .then(|| executing_header.extra_data[1..].try_into())
                .transpose()
                .map_err(|_| anyhow::anyhow!("Invalid header format for Holocene"))?,
            min_base_fee: None,
        },
    };

    // since the trie implementation uses handle.block_on() directly,
    // it has to be executed in a blocking-thread, not a worker thread:
    let outcome = tokio::task::spawn_blocking(move || {
        // Create trie for this task
        let trie = Trie::new(&provider);
        let mut executor = CeloStatelessL2Builder::new(
            &rollup_config,
            CeloEvmFactory::default(),
            &trie,
            NoopTrieHinter,
            parent_header,
        );
        executor
            .build_block(payload_attrs)
            .map_err(|e| anyhow::anyhow!("Failed to execute block {block_number}: {e}"))
    })
    .await??;

    // Verify the result
    if outcome.header.inner() != &executing_header.inner {
        tracing::warn!(
            block_number = block_number,
            expected_header = ?executing_header.inner,
            actual_header = ?outcome.header.inner(),
            "Block verification failed header mismatch"
        );
        //OPTIM: sync barrier
        metrics.lock().await.block_verification_completed(false, start.elapsed());
    } else {
        //OPTIM: sync barrier
        metrics.lock().await.block_verification_completed(true, start.elapsed());
    }
    // TODO: result logging?
    tracker.mark_verified(block_number).await;
    Ok(block_number)
}

/// Alternative helper for spawning block verification tasks with logging.
/// Currently unused but kept as a cleaner alternative to inline error handling.
#[allow(dead_code)]
async fn verify_task_and_log(
    block_number: u64,
    provider: Arc<RootProvider<Ethereum>>,
    rollup_config: Arc<celo_registry::CeloRollupConfig>,
    metrics: Arc<Mutex<Metrics>>,
    tracker: Arc<VerifiedBlockTracker>,
) {
    match verify_block(block_number, provider, rollup_config, metrics, tracker).await {
        Ok(block_number) => {
            tracing::debug!(block_number = block_number, "block verification task completed")
        }
        Err(e) => tracing::error!(error = %e, "block verification task failed"),
    }
}

/// Verifies execution for a range of blocks concurrently
#[allow(clippy::too_many_arguments)]
async fn verify_block_range(
    start_block: u64,
    end_block: u64,
    provider: Arc<RootProvider<Ethereum>>,
    rollup_config: Arc<celo_registry::CeloRollupConfig>,
    concurrency: usize,
    cancel_token: CancellationToken,
    metrics: Arc<Mutex<Metrics>>,
    tracker: Arc<VerifiedBlockTracker>,
) -> Result<()> {
    let mut handles = JoinSet::new();
    let mut next_block = start_block;
    tracing::debug!(
        start_block = start_block,
        end_block = end_block,
        concurrency = concurrency,
        "requested to verify block range"
    );

    // Spawn initial batch
    for _ in 0..concurrency {
        if cancel_token.is_cancelled() {
            break;
        }
        if next_block <= end_block {
            handles.spawn(verify_block(
                next_block,
                provider.clone(),
                rollup_config.clone(),
                metrics.clone(),
                tracker.clone(),
            ));
            next_block += 1;
        }
    }

    loop {
        match handles.join_next().await {
            None => {
                tracing::debug!(
                    start_block = start_block,
                    end_block = end_block,
                    "no more work to handle in verify-block-range"
                );
                break;
            }
            Some(result) => {
                match result {
                    Ok(Ok(block_number)) => {
                        tracing::debug!(
                            block_number = block_number,
                            "block verification task completed"
                        );
                    }
                    Ok(Err(e)) => {
                        tracing::error!(error = %e, "block verification task failed");
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "block verification task panicked");
                    }
                }

                // Spawn next task if available
                if next_block <= end_block && !cancel_token.is_cancelled() {
                    handles.spawn(verify_block(
                        next_block,
                        provider.clone(),
                        rollup_config.clone(),
                        metrics.clone(),
                        tracker.clone(),
                    ));
                    next_block += 1;
                }
            }
        };
    }

    Ok(())
}

fn persist_verified_block<P: AsRef<Path>>(highest_block: u64, file_path: P) -> Result<()> {
    let path = file_path.as_ref();
    let temp_path = path.with_extension("tmp");

    // Write to temporary file first
    std::fs::write(&temp_path, format!("{highest_block}")).map_err(|e| {
        anyhow::anyhow!(
            "Failed to write highest verified block to temp file {}: {}",
            temp_path.display(),
            e
        )
    })?;

    // Atomically rename to final file
    return std::fs::rename(&temp_path, path).map_err(|e| {
        anyhow::anyhow!(
            "Failed to rename temp file {} to {}: {}",
            temp_path.display(),
            path.display(),
            e
        )
    });
}

fn read_verified_block<P: AsRef<Path>>(file_path: P) -> Option<u64> {
    let content = std::fs::read_to_string(file_path).ok()?;
    content.trim().parse().ok()
}

/// A trie provider for the L2 execution layer.
#[derive(Debug)]
pub struct Trie<'a> {
    /// The RPC provider for the L2 execution layer.
    pub provider: &'a RootProvider<Ethereum>,
}

impl<'a> Trie<'a> {
    /// Create a new [`Trie`] instance.
    pub const fn new(provider: &'a RootProvider) -> Self {
        Self { provider }
    }
}

impl TrieProvider for &Trie<'_> {
    type Error = TrieError;

    fn trie_node_by_hash(&self, key: B256) -> Result<TrieNode, Self::Error> {
        // Fetch the preimage from the L2 chain provider.
        let preimage: Bytes = Handle::current().block_on(async {
            let preimage: Bytes = self
                .provider
                .client()
                .request("debug_dbGet", &[key])
                .await
                .map_err(|_| TrieError::PreimageNotFound)?;

            Ok(preimage)
        })?;

        // Decode the preimage into a trie node.
        TrieNode::decode(&mut preimage.as_ref()).map_err(TrieError::Rlp)
    }
}

impl TrieDBProvider for &Trie<'_> {
    fn bytecode_by_hash(&self, hash: B256) -> Result<Bytes, Self::Error> {
        // geth hashdb scheme code hash key prefix
        const CODE_PREFIX: u8 = b'c';

        // Fetch the preimage from the L2 chain provider.
        let preimage: Bytes = Handle::current().block_on(async {
            // Attempt to fetch the code from the L2 chain provider.
            let code_hash = [&[CODE_PREFIX], hash.as_slice()].concat();
            let code = self
                .provider
                .client()
                .request::<&[Bytes; 1], Bytes>("debug_dbGet", &[code_hash.into()])
                .await;

            // Check if the first attempt to fetch the code failed. If it did, try fetching the
            // code hash preimage without the geth hashdb scheme prefix.
            let code = match code {
                Ok(code) => code,
                Err(_) => self
                    .provider
                    .client()
                    .request::<&[B256; 1], Bytes>("debug_dbGet", &[hash])
                    .await
                    .map_err(|_| TrieError::PreimageNotFound)?,
            };

            Ok(code)
        })?;

        Ok(preimage)
    }

    fn header_by_hash(&self, hash: B256) -> Result<alloy_consensus::Header, Self::Error> {
        let encoded_header: Bytes = Handle::current().block_on(async {
            let preimage: Bytes = self
                .provider
                .client()
                .request("debug_getRawHeader", &[hash])
                .await
                .map_err(|_| TrieError::PreimageNotFound)?;

            Ok(preimage)
        })?;

        // Decode the Header.
        alloy_consensus::Header::decode(&mut encoded_header.as_ref()).map_err(TrieError::Rlp)
    }
}

/// An error type for the [`TrieProvider`] and [`TrieDBProvider`].
#[derive(Debug, thiserror::Error)]
pub enum TrieError {
    /// The preimage was not found in the key-value store.
    #[error("Preimage not found")]
    PreimageNotFound,
    /// Failed to decode the RLP-encoded data.
    #[error("Failed to decode RLP: {0}")]
    Rlp(alloy_rlp::Error),
    /// Failed to write back to the key-value store.
    #[error("Failed to write back to key value store")]
    KVStore,
}
