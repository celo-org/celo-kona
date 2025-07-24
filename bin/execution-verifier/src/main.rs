//! Execution verifier for Celo Kona
//!
//! This binary provides execution verification functionality for the Celo Kona project.
mod rpc_trie_provider;
mod leveldb_trie_provider;

use alloy_celo_evm::CeloEvmFactory;
use alloy_network::Ethereum;
use alloy_primitives::{B256, Bytes, Sealable};
use alloy_provider::{Provider, RootProvider, network::primitives::BlockTransactions};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_engine::PayloadAttributes;
use alloy_transport_http::{Client, Http};
use anyhow::Result;
use celo_alloy_rpc_types_engine::CeloPayloadAttributes;
use celo_executor::CeloStatelessL2Builder;
use celo_registry::ROLLUP_CONFIGS;
use clap::{ArgAction, Parser};
use kona_cli::init_tracing_subscriber;
use kona_mpt::{NoopTrieHinter};
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use std::sync::Arc;
use tokio::{sync::Semaphore, time::Instant};
use tracing_subscriber::EnvFilter;
use url::Url;
use std::path::PathBuf;
use crate::rpc_trie_provider::RPCTrieProvider;
use crate::leveldb_trie_provider::LevelDBTrieProvider;
use kona_executor::TrieDBProvider;

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
    /// The L2 archive node RPC to use.
    #[arg(long)]
    pub l2_rpc: Url,
    /// The path to the leveldb database to use.
    #[arg(long)]
    pub leveldb_path: PathBuf,
    /// L2 inclusive starting block number to execute.
    #[arg(long)]
    pub start_block: u64,
    /// L2 inclusive ending block number to execute.
    #[arg(long)]
    pub end_block: u64,
    /// Number of concurrent tasks to run.
    #[arg(long, default_value = "25")]
    pub concurrency: usize,
}

/// The execution verifier command
#[tokio::main]
async fn main() -> Result<()> {
    let cli = ExecutionVerifierCommand::parse();
    init_tracing_subscriber(cli.v, None::<EnvFilter>)?;

    let http = Http::<Client>::new(cli.l2_rpc);
    let provider: RootProvider<Ethereum> = RootProvider::new(RpcClient::new(http, false));
    let provider = Arc::new(provider);

    let chain_id = provider
        .get_chain_id()
        .await
        .expect("Failed to get chain ID");
    let rollup_config = ROLLUP_CONFIGS
        .get(&chain_id)
        .expect("Rollup config not found");

    let leveldb_trie = Arc::new(LevelDBTrieProvider::new(&cli.leveldb_path));

        // let leveldb_trie: Option<Arc<LevelDBTrieProvider>> = if let Some(leveldb_path) = cli.leveldb_path {
        //     Some(Arc::new(LevelDBTrieProvider::new(&leveldb_path)))
        // } else {
        //     None
        // };
    // let leveldb_trie = Arc::new(leveldb_trie);

    // Create semaphore to limit concurrency to 500
    let semaphore = Arc::new(Semaphore::new(cli.concurrency));

    let start = Instant::now();
    let mut tasks = Vec::new();

    for block_number in cli.start_block..=cli.end_block {
        let provider = Arc::clone(&provider);
        let rollup_config = rollup_config.clone();
        let semaphore = Arc::clone(&semaphore);
        let leveldb_trie = Arc::clone(&leveldb_trie);

        let task = tokio::spawn(async move {
            let _permit = semaphore.acquire().await.unwrap();


            // // Create trie for this task
            // let trie: dyn TrieDBProvider = if let Some(leveldb_trie) = leveldb_trie.as_ref() {
            //     leveldb_trie.as_ref()
            // } else {
            //     RPCTrieProvider::new(provider.as_ref())
            // };

            // Fetch parent block
            let parent_block = provider
                .get_block_by_number((block_number - 1).into())
                .await
                .expect("Failed to get parent block")
                .expect("Parent block not found");
            let parent_header = parent_block.header.inner.seal_slow();

            // Fetch executing block
            let executing_block = provider
                .get_block_by_number(block_number.into())
                .await
                .expect("Failed to get executing block")
                .expect("Executing block not found");

            let encoded_executing_transactions = match executing_block.transactions {
                BlockTransactions::Hashes(transactions) => {
                    let mut encoded_transactions = Vec::with_capacity(transactions.len());
                    for tx_hash in transactions {
                        let tx = provider
                            .client()
                            .request::<&[B256; 1], Bytes>("debug_getRawTransaction", &[tx_hash])
                            .await
                            .expect("Failed to get raw transaction");
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
                        .then(|| {
                            executing_header.extra_data[1..]
                                .try_into()
                                .expect("Invalid header format for Holocene")
                        }),
                },
            };

            let mut executor = CeloStatelessL2Builder::new(
                &rollup_config,
                CeloEvmFactory::default(),
                leveldb_trie.as_ref(),
                NoopTrieHinter,
                parent_header,
            );
            let outcome = executor
                .build_block(payload_attrs)
                .expect("Failed to execute block");

            // Verify the result
            if outcome.header.inner() != &executing_header.inner {
                return Err(anyhow::anyhow!(
                    "Block {} verification failed: produced header does not match expected header",
                    block_number
                ));
            }

            println!("Successfully verified block {}", block_number);
            Ok(block_number)
        });

        tasks.push(task);
    }

    // Wait for all tasks to complete and count results
    let mut failed_blocks = 0;

    for task in tasks {
        match task.await {
            Ok(Ok(_)) => (),
            Ok(Err(e)) => {
                failed_blocks += 1;
                eprintln!("Block verification failed: {}", e);
            }
            Err(e) => {
                failed_blocks += 1;
                eprintln!("Task panicked: {}", e);
            }
        }
    }

    let elapsed = start.elapsed();

    if failed_blocks > 0 {
        println!(
            "Verification completed with {} failures out of {} total blocks",
            failed_blocks,
            cli.end_block - cli.start_block + 1
        );
    } else {
        println!(
            "Successfully verified execution for all {} blocks ({} to {})",
            cli.end_block - cli.start_block + 1,
            cli.start_block,
            cli.end_block
        );
    }

    println!("Total verification time: {:?}", elapsed);
    println!(
        "Average time per block: {:?}",
        elapsed / (cli.end_block - cli.start_block + 1) as u32
    );

    Ok(())
}

