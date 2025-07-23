//! Execution verifier for Celo Kona
//!
//! This binary provides execution verification functionality for the Celo Kona project.

use alloy_celo_evm::CeloEvmFactory;
use alloy_consensus::Header;
use alloy_network::Ethereum;
use alloy_primitives::{B256, Bytes, Sealable};
use alloy_provider::{Provider, RootProvider, network::primitives::BlockTransactions};
use alloy_rlp::Decodable;
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_engine::PayloadAttributes;
use alloy_transport_http::{Client, Http};
use anyhow::Result;
use celo_alloy_rpc_types_engine::CeloPayloadAttributes;
use celo_executor::CeloStatelessL2Builder;
use celo_registry::ROLLUP_CONFIGS;
use clap::{ArgAction, Parser};
use kona_cli::init_tracing_subscriber;
use kona_executor::TrieDBProvider;
use kona_mpt::{NoopTrieHinter, TrieNode, TrieProvider};
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use tokio::{runtime::Handle, time::Instant};
use tracing_subscriber::EnvFilter;
use url::Url;

// use kona_registry::ROLLUP_CONFIGS;
// use kona_executor::CeloStatelessL2Builder;
// use alloy_celo_evm::CeloEvmFactory;
// use kona_mpt::NoopTrieHinter;

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
    /// The L2 archive EL to use.
    #[arg(long)]
    pub l2_rpc: Url,
    /// L2 inclusive starting block number to execute.
    #[arg(long)]
    pub start_block: u64,
    /// L2 inclusive ending block number to execute.
    #[arg(long)]
    pub end_block: u64,
}

/// The execution verifier command
#[tokio::main]
async fn main() -> Result<()> {
    let cli = ExecutionVerifierCommand::parse();
    init_tracing_subscriber(cli.v, None::<EnvFilter>)?;

    let http = Http::<Client>::new(cli.l2_rpc);
    let provider: RootProvider<Ethereum> = RootProvider::new(RpcClient::new(http, false));

    let trie = Trie::new(&provider);

    let mut parent_header = provider
        .get_block_by_number((cli.start_block - 1).into())
        .await
        .expect("Failed to get parent block")
        .expect("Block not found")
        .header
        .inner
        .seal_slow();

    let start = Instant::now();
    for block_number in cli.start_block..=cli.end_block {
        let chain_id = provider
            .get_chain_id()
            .await
            .expect("Failed to get chain ID");
        let rollup_config = ROLLUP_CONFIGS
            .get(&chain_id)
            .expect("Rollup config not found");

        let executing_block = provider
            .get_block_by_number(block_number.into())
            .await
            .expect("Failed to get parent block")
            .expect("Block not found");

        let encoded_executing_transactions = match executing_block.transactions {
            BlockTransactions::Hashes(transactions) => {
                let mut encoded_transactions = Vec::with_capacity(transactions.len());
                for tx_hash in transactions {
                    let tx = provider
                        .client()
                        .request::<&[B256; 1], Bytes>("debug_getRawTransaction", &[tx_hash])
                        .await
                        .expect("Block not found");
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
            rollup_config,
            CeloEvmFactory::default(),
            &trie,
            NoopTrieHinter,
            parent_header,
        );
        executor
            .build_block(payload_attrs)
            .expect("Failed to execute block");
        parent_header = executing_block.header.inner.seal_slow();
    }
    let elapsed = start.elapsed();
    println!("Total verification time to verify {} blocks took: {:?}", cli.end_block - cli.start_block, elapsed);
    println!("Time per block: {:?}", elapsed / (cli.end_block - cli.start_block) as u32);
    println!("Successfully verified execution for blocks {} to {}", cli.start_block, cli.end_block);
    Ok(())
}

/// A trie provider for the L2 execution layer.
#[derive(Debug)]
pub struct Trie<'a> {
    /// The RPC provider for the L2 execution layer.
    pub provider: &'a RootProvider,
}

impl<'a> Trie<'a> {
    /// Create a new [`Trie`] instance.
    pub fn new(provider: &'a RootProvider) -> Self {
        Self { provider }
    }
}

impl<'a> TrieProvider for &Trie<'a> {
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

impl<'a> TrieDBProvider for &Trie<'a> {
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

    fn header_by_hash(&self, hash: B256) -> Result<Header, Self::Error> {
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
        Header::decode(&mut encoded_header.as_ref()).map_err(TrieError::Rlp)
    }
}

/// An error type for the [`DiskTrieNodeProvider`] and [`ExecutorTestFixtureCreator`].
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
