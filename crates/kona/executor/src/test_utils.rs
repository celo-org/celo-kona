//! Test utilities for the executor.

use crate::{CeloBlockBuildingOutcome, CeloStatelessL2Builder};
use alloy_celo_evm::CeloEvmFactory;
use alloy_consensus::Header;
use alloy_primitives::{B256, Bytes, Sealable};
use alloy_provider::{Provider, network::primitives::BlockTransactions};
use alloy_rpc_types_engine::PayloadAttributes;
use celo_alloy_rpc_types_engine::CeloPayloadAttributes;
use celo_genesis::CeloRollupConfig;
use celo_registry::ROLLUP_CONFIGS;
use kona_executor::{
    TrieDBProvider,
    test_utils::{
        DiskTrieNodeProvider, ExecutorTestFixture as OpExecutorTestFixture,
        ExecutorTestFixtureCreator as OpExecutorTestFixtureCreator, TestTrieNodeProviderError,
    },
};
use kona_mpt::{NoopTrieHinter, TrieNode, TrieProvider};
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use rocksdb::{DB, Options};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::fs;

/// Loads and executes a test fixture from a tarball path.
///
/// Returns the execution outcome and the fixture data for further inspection.
pub async fn load_and_execute_fixture(
    fixture_path: PathBuf,
) -> (CeloBlockBuildingOutcome, ExecutorTestFixture) {
    // First, untar the fixture.
    let fixture_dir = tempfile::tempdir().expect("Failed to create temporary directory");
    tokio::process::Command::new("tar")
        .arg("-xvf")
        .arg(fixture_path.as_path())
        .arg("-C")
        .arg(fixture_dir.path())
        .arg("--strip-components=1")
        .output()
        .await
        .expect("Failed to untar fixture");

    let mut options = Options::default();
    options.set_compression_type(rocksdb::DBCompressionType::Snappy);
    options.create_if_missing(true);
    let kv_store = DB::open(&options, fixture_dir.path().join("kv"))
        .unwrap_or_else(|e| panic!("Failed to open database at {fixture_dir:?}: {e}"));
    let provider = DiskTrieNodeProvider::new(kv_store);
    let fixture: ExecutorTestFixture =
        serde_json::from_slice(&fs::read(fixture_dir.path().join("fixture.json")).await.unwrap())
            .expect("Failed to deserialize fixture");

    // Wrap RollupConfig to CeloRollupConfig
    let rollup_config = fixture.op_executor_test_fixture.rollup_config.clone();
    let celo_rollup_config = CeloRollupConfig(rollup_config);
    let mut executor = CeloStatelessL2Builder::new(
        &celo_rollup_config,
        CeloEvmFactory::default(),
        provider,
        NoopTrieHinter,
        fixture.op_executor_test_fixture.parent_header.clone().seal_slow(),
    );

    let outcome =
        executor.build_block(fixture.executing_payload.clone()).expect("Failed to execute block");

    (outcome, fixture)
}

/// Executes a [ExecutorTestFixture] stored at the passed `fixture_path` and asserts that the
/// produced block hash matches the expected block hash.
pub async fn run_test_fixture(fixture_path: PathBuf) {
    let (outcome, fixture) = load_and_execute_fixture(fixture_path).await;

    assert_eq!(
        outcome.header.hash(),
        fixture.op_executor_test_fixture.expected_block_hash,
        "Produced header does not match the expected header"
    );
}

/// The test fixture format for the [`CeloStatelessL2Builder`].
#[derive(Debug, Serialize, Deserialize)]
pub struct ExecutorTestFixture {
    /// [`kona_executor::test_utils::ExecutorTestFixture`]
    pub op_executor_test_fixture: OpExecutorTestFixture,
    /// The executing payload attributes.
    pub executing_payload: CeloPayloadAttributes,
}

/// A test fixture creator for the [`CeloStatelessL2Builder`].
#[derive(Debug)]
pub struct ExecutorTestFixtureCreator {
    /// [`kona_executor::test_utils::ExecutorTestFixtureCreator`]
    pub op_executor_test_fixture_creator: OpExecutorTestFixtureCreator,
}

impl ExecutorTestFixtureCreator {
    /// Creates a new [`ExecutorTestFixtureCreator`] with the given parameters.
    pub fn new(provider_url: &str, block_number: u64, base_fixture_directory: PathBuf) -> Self {
        Self {
            op_executor_test_fixture_creator: OpExecutorTestFixtureCreator::new(
                provider_url,
                block_number,
                base_fixture_directory,
            ),
        }
    }
}

impl ExecutorTestFixtureCreator {
    /// Create a static test fixture with the configuration provided.
    pub async fn create_static_fixture(self) {
        let chain_id = self
            .op_executor_test_fixture_creator
            .provider
            .get_chain_id()
            .await
            .expect("Failed to get chain ID");
        let rollup_config = ROLLUP_CONFIGS.get(&chain_id).expect("Rollup config not found");

        let executing_block = self
            .op_executor_test_fixture_creator
            .provider
            .get_block_by_number(self.op_executor_test_fixture_creator.block_number.into())
            .await
            .expect("Failed to get parent block")
            .expect("Block not found");
        let parent_block = self
            .op_executor_test_fixture_creator
            .provider
            .get_block_by_number((self.op_executor_test_fixture_creator.block_number - 1).into())
            .await
            .expect("Failed to get parent block")
            .expect("Block not found");

        let executing_header = executing_block.header;
        let parent_header = parent_block.header.inner.seal_slow();

        let encoded_executing_transactions = match executing_block.transactions {
            BlockTransactions::Hashes(transactions) => {
                let mut encoded_transactions = Vec::with_capacity(transactions.len());
                for tx_hash in transactions {
                    let tx = self
                        .op_executor_test_fixture_creator
                        .provider
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
                eip_1559_params: rollup_config.is_holocene_active(executing_header.timestamp).then(
                    || {
                        executing_header.extra_data[1..]
                            .try_into()
                            .expect("Invalid header format for Holocene")
                    },
                ),
                min_base_fee: None,
            },
        };

        let fixture_path = self.op_executor_test_fixture_creator.data_dir.join("fixture.json");
        let fixture = ExecutorTestFixture {
            op_executor_test_fixture: OpExecutorTestFixture {
                rollup_config: rollup_config.0.clone(),
                parent_header: parent_header.inner().clone(),
                expected_block_hash: executing_header.hash_slow(),
                executing_payload: payload_attrs.op_payload_attributes.clone(),
            },
            executing_payload: payload_attrs.clone(),
        };

        let mut executor = CeloStatelessL2Builder::new(
            rollup_config,
            CeloEvmFactory::default(),
            self,
            NoopTrieHinter,
            parent_header,
        );
        let outcome = executor.build_block(payload_attrs).expect("Failed to execute block");

        assert_eq!(
            outcome.header.inner(),
            &executing_header.inner,
            "Produced header (left) does not match the expected header (right)"
        );
        fs::write(fixture_path.as_path(), serde_json::to_vec(&fixture).unwrap()).await.unwrap();

        // Tar the fixture.
        let data_dir = fixture_path.parent().unwrap();
        tokio::process::Command::new("tar")
            .arg("-czf")
            .arg(data_dir.with_extension("tar.gz").file_name().unwrap())
            .arg(data_dir.file_name().unwrap())
            .current_dir(data_dir.parent().unwrap())
            .output()
            .await
            .expect("Failed to tar fixture");

        // Remove the leftover directory.
        fs::remove_dir_all(data_dir).await.expect("Failed to remove temporary directory");
    }
}

impl TrieProvider for ExecutorTestFixtureCreator {
    type Error = TestTrieNodeProviderError;

    fn trie_node_by_hash(&self, key: B256) -> Result<TrieNode, Self::Error> {
        self.op_executor_test_fixture_creator.trie_node_by_hash(key)
    }
}

impl TrieDBProvider for ExecutorTestFixtureCreator {
    fn bytecode_by_hash(&self, hash: B256) -> Result<Bytes, Self::Error> {
        self.op_executor_test_fixture_creator.bytecode_by_hash(hash)
    }

    fn header_by_hash(&self, hash: B256) -> Result<Header, Self::Error> {
        self.op_executor_test_fixture_creator.header_by_hash(hash)
    }
}
