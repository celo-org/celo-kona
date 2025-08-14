//! Execution verifier for Celo Kona
//!
//! This binary provides execution verification functionality for the Celo Kona project.

use alloy_celo_evm::CeloEvmFactory;
use alloy_network::Ethereum;
use alloy_primitives::{B256, Bytes, Sealable};
use alloy_provider::{
    Provider, ProviderBuilder, RootProvider, network::primitives::BlockTransactions,
};
use alloy_pubsub::Subscription;
use alloy_rlp::Decodable;
use alloy_rpc_types_engine::PayloadAttributes;
use alloy_rpc_types_eth::Header;
use alloy_transport_ipc::IpcConnect;
use anyhow::Result;
use celo_alloy_rpc_types_engine::CeloPayloadAttributes;
use celo_executor::CeloStatelessL2Builder;
use celo_registry::ROLLUP_CONFIGS;
use clap::{ArgAction, Parser};
use futures::stream::StreamExt;
use kona_cli::init_tracing_subscriber;
use kona_executor::TrieDBProvider;
use kona_mpt::{NoopTrieHinter, TrieNode, TrieProvider};
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::{
    runtime::Handle,
    sync::mpsc,
    task::JoinSet,
    time::{Duration, Instant, interval},
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

mod verified_block_tracker;
use verified_block_tracker::VerifiedBlockTracker;

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
    pub state_file: Option<String>,
}

/// The execution verifier command
#[tokio::main]
async fn main() -> Result<()> {
    let cli = ExecutionVerifierCommand::parse();

    if let Some(start_block) = cli.start_block {
        if start_block == 0 {
            return Err(anyhow::anyhow!(
                "start_block {} must be > 0 (need parent block)",
                start_block
            ));
        }
        if let Some(end_block) = cli.end_block {
            if start_block > end_block {
                return Err(anyhow::anyhow!(
                    "start_block {} must be <= end_block {}",
                    start_block,
                    end_block
                ));
            }
        }
    } else if let Some(end_block) = cli.end_block {
        return Err(anyhow::anyhow!("end-block {} provided without start-block", end_block));
    }

    // Use the stored starting block or the CLI-provided one, but only
    // if a persistence-file option is given
    let start_block = match cli.state_file {
        None => cli.start_block,
        Some(ref f) => read_verified_block(&f).or(cli.start_block),
    };

    // This filter shows all logs from this code but doesn't show logs from the block builder, since
    // it is very verbose.
    let filter = EnvFilter::new("trace").add_directive("block_builder=off".parse().unwrap());
    init_tracing_subscriber(cli.v, Some(filter))?;

    // Check if l2_rpc is a URL or a file path
    let provider: RootProvider<Ethereum> = match cli.l2_rpc.as_str() {
        url if url.starts_with("ws://") || url.starts_with("wss://") => {
            ProviderBuilder::new().connect(url).await?.root().clone()
        }
        file_path => {
            if !std::path::Path::new(file_path).exists() {
                return Err(anyhow::anyhow!(
                    "Invalid L2 RPC {}. Only WS/WSS URLs and ipc file paths are supported.",
                    cli.l2_rpc,
                ));
            }
            let ipc = IpcConnect::new(cli.l2_rpc);
            ProviderBuilder::new().connect_ipc(ipc).await?.root().clone()
        }
    };

    let provider = Arc::new(provider);

    let mut handles = JoinSet::new();

    let cancel_token = CancellationToken::new();
    let subscription = provider.subscribe_blocks().await?;

    let chain_id = provider
        .get_chain_id()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get chain ID: {}", e))?;
    let rollup_config = ROLLUP_CONFIGS
        .get(&chain_id)
        .ok_or_else(|| anyhow::anyhow!("Rollup config not found for chain ID {}", chain_id))?;

    let tracker = Arc::new(Mutex::new(VerifiedBlockTracker::new(start_block)));

    // Spawn the repeating task in the background
    let tracker_handle = tracker.clone();
    let cancel_token_clone = cancel_token.clone();
    let verified_block_store_task = tokio::spawn(async move {
        let mut interval = interval(Duration::from_secs(60));

        loop {
            tokio::select! {
                _ = cancel_token_clone.cancelled() => {
                    break;
                }
                _ = interval.tick() => {
                    let tracker = tracker_handle.clone();
                    if let Err(e) = persist_verified_block(tracker,cli.state_file.as_ref()).await {
                        eprintln!("Error storing verified block: {}", e);
                    }
                }
            }
        }
    });

    let metrics = Arc::new(Mutex::new(Metrics::new()));
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
        ));
        let first_head_block =
            first_head_rx.recv().await.ok_or_else(|| anyhow::anyhow!("Channel closed"))?;
        let end = first_head_block - 1;
        handles.spawn(verify_block_range(
            start_block,
            end,
            provider.clone(),
            rollup_config.clone(),
            cli.concurrency - 1,
            cancel_token.clone(),
            metrics.clone(),
            tracker.clone(),
        ));
    } else {
        handles.spawn(verify_new_heads(
            provider.clone(),
            rollup_config.clone(),
            subscription,
            cancel_token.clone(),
            None,
            metrics.clone(),
            tracker.clone(),
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
                return Err(anyhow::anyhow!("Task panicked: {}", e));
            }
            _ => {}
        }
    }

    verified_block_store_task.abort();

    metrics.lock().display_metrics();

    Ok(())
}

async fn verify_new_heads(
    provider: Arc<RootProvider<Ethereum>>,
    rollup_config: celo_registry::CeloRollupConfig,
    subscription: Subscription<Header>,
    cancel_token: CancellationToken,
    first_head_tx: Option<mpsc::Sender<u64>>,
    metrics: Arc<Mutex<Metrics>>,
    tracker: Arc<Mutex<VerifiedBlockTracker>>,
) -> Result<()> {
    let mut first_block = true;

    let mut stream = subscription.into_stream();

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

        let result = verify_block(
            header.number,
            provider.as_ref(),
            &rollup_config,
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
    }

    Ok(())
}

/// Verifies execution for a single block
async fn verify_block(
    block_number: u64,
    provider: &RootProvider<Ethereum>,
    rollup_config: &celo_registry::CeloRollupConfig,
    metrics: Arc<Mutex<Metrics>>,
    tracker: Arc<Mutex<VerifiedBlockTracker>>,
) -> Result<u64> {
    let start = Instant::now();
    // Create trie for this task
    let trie = Trie::new(provider);

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
        .map_err(|e| anyhow::anyhow!("Failed to get executing block {}: {}", block_number, e))?
        .ok_or_else(|| anyhow::anyhow!("Executing block {} not found", block_number))?;

    let encoded_executing_transactions = match executing_block.transactions {
        BlockTransactions::Hashes(transactions) => {
            let mut encoded_transactions = Vec::with_capacity(transactions.len());
            for tx_hash in transactions {
                let tx = provider
                    .client()
                    .request::<&[B256; 1], Bytes>("debug_getRawTransaction", &[tx_hash])
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("Failed to get raw transaction {}: {}", tx_hash, e)
                    })?;
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
                .op_rollup_config
                .is_holocene_active(executing_header.timestamp)
                .then(|| executing_header.extra_data[1..].try_into())
                .transpose()
                .map_err(|_| anyhow::anyhow!("Invalid header format for Holocene"))?,
        },
    };

    let mut executor = CeloStatelessL2Builder::new(
        rollup_config,
        CeloEvmFactory::default(),
        &trie,
        NoopTrieHinter,
        parent_header,
    );
    let outcome = executor
        .build_block(payload_attrs)
        .map_err(|e| anyhow::anyhow!("Failed to execute block {}: {}", block_number, e))?;

    // Verify the result
    if outcome.header.inner() != &executing_header.inner {
        metrics.lock().failed_block(start.elapsed());
        tracing::warn!(
            block_number = block_number,
            expected_header = ?executing_header.inner,
            actual_header = ?outcome.header.inner(),
            "Block verification failed header mismatch"
        );
    } else {
        tracker.lock().add_verified_block(block_number);
        metrics.lock().successful_block(start.elapsed());
    }
    Ok(block_number)
}

/// Verifies execution for a range of blocks concurrently
#[allow(clippy::too_many_arguments)]
async fn verify_block_range(
    start_block: u64,
    end_block: u64,
    provider: Arc<RootProvider<Ethereum>>,
    rollup_config: celo_registry::CeloRollupConfig,
    concurrency: usize,
    cancel_token: CancellationToken,
    metrics: Arc<Mutex<Metrics>>,
    tracker: Arc<Mutex<VerifiedBlockTracker>>,
) -> Result<()> {
    let mut handles = JoinSet::new();
    let mut next_block = start_block;

    // Spawn initial batch
    for _ in 0..concurrency {
        if cancel_token.is_cancelled() {
            break;
        }
        // let provider = provider.clone();
        if next_block <= end_block {
            let rollup_config = rollup_config.clone();
            let provider = provider.clone();
            let metrics = metrics.clone();
            let tracker = tracker.clone();
            handles.spawn(async move {
                verify_block(next_block, provider.as_ref(), &rollup_config, metrics, tracker).await
            });
            next_block += 1;
        }
    }

    // Process results and spawn new tasks continuously
    while handles.join_next().await.is_some() {
        // Spawn next task if available
        if next_block <= end_block && !cancel_token.is_cancelled() {
            let rollup_config = rollup_config.clone();
            let provider = provider.clone();
            let metrics = metrics.clone();
            let tracker = tracker.clone();
            handles.spawn(async move {
                verify_block(next_block, provider.as_ref(), &rollup_config, metrics, tracker).await
            });
            next_block += 1;
        }
    }

    Ok(())
}

async fn persist_verified_block(
    tracker: Arc<Mutex<VerifiedBlockTracker>>,
    file_path: Option<&String>,
) -> Result<()> {
    let mut tracker = tracker.lock();
    tracker.update_highest_verified_block();

    if let Some(highest_block) = tracker.highest_verified_block() {
        tracing::info!("Current highest verified block: {:?}", highest_block);
        if let Some(f) = file_path {
            return std::fs::write(f, format!("{highest_block}")).map_err(|e| {
                anyhow::anyhow!("Failed to write highest verified block to {}: {}", f, e)
            });
        }
    }
    Ok(())
}

fn read_verified_block(file_path: &String) -> Option<u64> {
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
        let preimage: Bytes = tokio::task::block_in_place(move || {
            Handle::current().block_on(async {
                let preimage: Bytes = self
                    .provider
                    .client()
                    .request("debug_dbGet", &[key])
                    .await
                    .map_err(|_| TrieError::PreimageNotFound)?;

                Ok(preimage)
            })
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
        let preimage: Bytes = tokio::task::block_in_place(move || {
            Handle::current().block_on(async {
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
            })
        })?;

        Ok(preimage)
    }

    fn header_by_hash(&self, hash: B256) -> Result<alloy_consensus::Header, Self::Error> {
        let encoded_header: Bytes = tokio::task::block_in_place(move || {
            Handle::current().block_on(async {
                let preimage: Bytes = self
                    .provider
                    .client()
                    .request("debug_getRawHeader", &[hash])
                    .await
                    .map_err(|_| TrieError::PreimageNotFound)?;

                Ok(preimage)
            })
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

struct Metrics {
    init_time: Instant,
    processed_blocks: u64,
    failed_blocks: u64,
    successful_processing_time: Duration,
    failed_processing_time: Duration,
}

impl Metrics {
    fn new() -> Self {
        Self {
            init_time: Instant::now(),
            processed_blocks: 0,
            failed_blocks: 0,
            successful_processing_time: Duration::new(0, 0),
            failed_processing_time: Duration::new(0, 0),
        }
    }

    fn successful_block(&mut self, duration: Duration) {
        self.processed_blocks += 1;
        self.successful_processing_time += duration;
    }
    fn failed_block(&mut self, duration: Duration) {
        self.failed_blocks += 1;
        self.failed_processing_time += duration;
    }
    fn average_block_processing_time(&self) -> Duration {
        if self.processed_blocks == 0 {
            return Duration::new(0, 0);
        }
        (self.successful_processing_time + self.failed_processing_time) /
            self.processed_blocks as u32
    }
    fn average_successful_block_processing_time(&self) -> Duration {
        let successful_blocks = self.processed_blocks - self.failed_blocks;
        if successful_blocks == 0 {
            return Duration::new(0, 0);
        }
        self.successful_processing_time / successful_blocks as u32
    }
    fn average_amortized_successful_block_processing_time(&self) -> Duration {
        let successful_blocks = self.processed_blocks - self.failed_blocks;
        if successful_blocks == 0 {
            return Duration::new(0, 0);
        }
        Instant::now().duration_since(self.init_time) / successful_blocks as u32
    }
    fn average_failed_block_processing_time(&self) -> Duration {
        if self.failed_blocks == 0 {
            return Duration::new(0, 0);
        }
        self.failed_processing_time / self.failed_blocks as u32
    }
    fn display_metrics(&self) {
        tracing::info!("Avg time per block: {:?}", self.average_block_processing_time());
        tracing::info!(
            "Avg time per successful block: {:?}",
            self.average_successful_block_processing_time()
        );
        tracing::info!(
            "Avg time per failed block: {:?}",
            self.average_failed_block_processing_time()
        );
        tracing::info!(
            "Avg amortized time per block: {:?}",
            self.average_amortized_successful_block_processing_time()
        );
        tracing::info!("Total blocks: {}", self.processed_blocks);
        tracing::info!("Failed blocks: {}", self.failed_blocks);
    }
}
