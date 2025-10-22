//! Test utilities for the executor.

use crate::CeloStatelessL2Builder;
use alloy_celo_evm::CeloEvmFactory;
use alloy_consensus::Header;
use alloy_primitives::{B256, Bytes, Sealable};
use alloy_provider::{Provider, network::primitives::BlockTransactions};
use alloy_rpc_types_engine::PayloadAttributes;
use celo_alloy_consensus::CeloReceiptEnvelope;
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

/// Compares two headers field by field and returns a detailed error message if they differ.
fn compare_headers(produced: &Header, expected: &Header) -> Result<(), String> {
    let mut diffs = Vec::new();

    if produced.parent_hash != expected.parent_hash {
        diffs.push(format!(
            "  parent_hash: produced={}, expected={}",
            produced.parent_hash, expected.parent_hash
        ));
    }
    if produced.ommers_hash != expected.ommers_hash {
        diffs.push(format!(
            "  ommers_hash: produced={}, expected={}",
            produced.ommers_hash, expected.ommers_hash
        ));
    }
    if produced.beneficiary != expected.beneficiary {
        diffs.push(format!(
            "  beneficiary: produced={}, expected={}",
            produced.beneficiary, expected.beneficiary
        ));
    }
    if produced.state_root != expected.state_root {
        diffs.push(format!(
            "  state_root: produced={}, expected={}",
            produced.state_root, expected.state_root
        ));
    }
    if produced.transactions_root != expected.transactions_root {
        diffs.push(format!(
            "  transactions_root: produced={}, expected={}",
            produced.transactions_root, expected.transactions_root
        ));
    }
    if produced.receipts_root != expected.receipts_root {
        diffs.push(format!(
            "  receipts_root: produced={}, expected={}",
            produced.receipts_root, expected.receipts_root
        ));
    }
    if produced.logs_bloom != expected.logs_bloom {
        diffs.push(format!(
            "  logs_bloom: produced={}, expected={}",
            produced.logs_bloom, expected.logs_bloom
        ));
    }
    if produced.difficulty != expected.difficulty {
        diffs.push(format!(
            "  difficulty: produced={}, expected={}",
            produced.difficulty, expected.difficulty
        ));
    }
    if produced.number != expected.number {
        diffs.push(format!("  number: produced={}, expected={}", produced.number, expected.number));
    }
    if produced.gas_limit != expected.gas_limit {
        diffs.push(format!(
            "  gas_limit: produced={}, expected={}",
            produced.gas_limit, expected.gas_limit
        ));
    }
    if produced.gas_used != expected.gas_used {
        diffs.push(format!(
            "  gas_used: produced={}, expected={}",
            produced.gas_used, expected.gas_used
        ));
    }
    if produced.timestamp != expected.timestamp {
        diffs.push(format!(
            "  timestamp: produced={}, expected={}",
            produced.timestamp, expected.timestamp
        ));
    }
    if produced.extra_data != expected.extra_data {
        diffs.push(format!(
            "  extra_data: produced={}, expected={}",
            produced.extra_data, expected.extra_data
        ));
    }
    if produced.mix_hash != expected.mix_hash {
        diffs.push(format!(
            "  mix_hash: produced={}, expected={}",
            produced.mix_hash, expected.mix_hash
        ));
    }
    if produced.nonce != expected.nonce {
        diffs.push(format!("  nonce: produced={}, expected={}", produced.nonce, expected.nonce));
    }
    if produced.base_fee_per_gas != expected.base_fee_per_gas {
        diffs.push(format!(
            "  base_fee_per_gas: produced={:?}, expected={:?}",
            produced.base_fee_per_gas, expected.base_fee_per_gas
        ));
    }
    if produced.withdrawals_root != expected.withdrawals_root {
        diffs.push(format!(
            "  withdrawals_root: produced={:?}, expected={:?}",
            produced.withdrawals_root, expected.withdrawals_root
        ));
    }
    if produced.blob_gas_used != expected.blob_gas_used {
        diffs.push(format!(
            "  blob_gas_used: produced={:?}, expected={:?}",
            produced.blob_gas_used, expected.blob_gas_used
        ));
    }
    if produced.excess_blob_gas != expected.excess_blob_gas {
        diffs.push(format!(
            "  excess_blob_gas: produced={:?}, expected={:?}",
            produced.excess_blob_gas, expected.excess_blob_gas
        ));
    }
    if produced.parent_beacon_block_root != expected.parent_beacon_block_root {
        diffs.push(format!(
            "  parent_beacon_block_root: produced={:?}, expected={:?}",
            produced.parent_beacon_block_root, expected.parent_beacon_block_root
        ));
    }
    if produced.requests_hash != expected.requests_hash {
        diffs.push(format!(
            "  requests_hash: produced={:?}, expected={:?}",
            produced.requests_hash, expected.requests_hash
        ));
    }

    if diffs.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Header mismatch:\n  produced_hash={}\n  expected_hash={}\nDiffering fields:\n{}",
            produced.hash_slow(),
            expected.hash_slow(),
            diffs.join("\n")
        ))
    }
}

/// Formats receipt details for debugging
fn format_receipt_details(receipts: &[CeloReceiptEnvelope]) -> String {
    let mut output = Vec::new();

    for (i, receipt) in receipts.iter().enumerate() {
        output.push(format!("    Receipt #{}:", i));

        match receipt {
            CeloReceiptEnvelope::Legacy(r) => {
                output.push("      type: Legacy".to_string());
                output.push(format!("      status: {:?}", r.receipt.status));
                output
                    .push(format!("      cumulative_gas_used: {}", r.receipt.cumulative_gas_used));
                output.push(format!("      logs_count: {}", r.receipt.logs.len()));
                output.push(format!("      logs_bloom: {}", r.logs_bloom));
            }
            CeloReceiptEnvelope::Eip2930(r) => {
                output.push("      type: Eip2930".to_string());
                output.push(format!("      status: {:?}", r.receipt.status));
                output
                    .push(format!("      cumulative_gas_used: {}", r.receipt.cumulative_gas_used));
                output.push(format!("      logs_count: {}", r.receipt.logs.len()));
                output.push(format!("      logs_bloom: {}", r.logs_bloom));
            }
            CeloReceiptEnvelope::Eip1559(r) => {
                output.push("      type: Eip1559".to_string());
                output.push(format!("      status: {:?}", r.receipt.status));
                output
                    .push(format!("      cumulative_gas_used: {}", r.receipt.cumulative_gas_used));
                output.push(format!("      logs_count: {}", r.receipt.logs.len()));
                output.push(format!("      logs_bloom: {}", r.logs_bloom));
            }
            CeloReceiptEnvelope::Eip7702(r) => {
                output.push("      type: Eip7702".to_string());
                output.push(format!("      status: {:?}", r.receipt.status));
                output
                    .push(format!("      cumulative_gas_used: {}", r.receipt.cumulative_gas_used));
                output.push(format!("      logs_count: {}", r.receipt.logs.len()));
                output.push(format!("      logs_bloom: {}", r.logs_bloom));
            }
            CeloReceiptEnvelope::Cip64(r) => {
                output.push("      type: Cip64".to_string());
                output.push(format!("      status: {:?}", r.receipt.inner.status));
                output.push(format!(
                    "      cumulative_gas_used: {}",
                    r.receipt.inner.cumulative_gas_used
                ));
                output.push(format!("      logs_count: {}", r.receipt.inner.logs.len()));
                output.push(format!("      logs_bloom: {}", r.logs_bloom));
                output.push(format!("      base_fee: {:?}", r.receipt.base_fee));
            }
            CeloReceiptEnvelope::Deposit(r) => {
                output.push("      type: Deposit".to_string());
                output.push(format!("      status: {:?}", r.receipt.inner.status));
                output.push(format!(
                    "      cumulative_gas_used: {}",
                    r.receipt.inner.cumulative_gas_used
                ));
                output.push(format!("      logs_count: {}", r.receipt.inner.logs.len()));
                output.push(format!("      logs_bloom: {}", r.logs_bloom));
                output.push(format!("      deposit_nonce: {:?}", r.receipt.deposit_nonce));
                output.push(format!(
                    "      deposit_receipt_version: {:?}",
                    r.receipt.deposit_receipt_version
                ));
            }
        };

        // Show individual logs if there are any
        match receipt {
            CeloReceiptEnvelope::Legacy(r) |
            CeloReceiptEnvelope::Eip2930(r) |
            CeloReceiptEnvelope::Eip1559(r) |
            CeloReceiptEnvelope::Eip7702(r) => {
                if !r.receipt.logs.is_empty() {
                    output.push("      logs:".to_string());
                    for (log_idx, log) in r.receipt.logs.iter().enumerate() {
                        output.push(format!(
                            "        Log #{}: address={}, topics_count={}",
                            log_idx,
                            log.address,
                            log.data.topics().len()
                        ));
                    }
                }
            }
            CeloReceiptEnvelope::Cip64(r) => {
                if !r.receipt.inner.logs.is_empty() {
                    output.push("      logs:".to_string());
                    for (log_idx, log) in r.receipt.inner.logs.iter().enumerate() {
                        output.push(format!(
                            "        Log #{}: address={}, topics_count={}",
                            log_idx,
                            log.address,
                            log.data.topics().len()
                        ));
                    }
                }
            }
            CeloReceiptEnvelope::Deposit(r) => {
                if !r.receipt.inner.logs.is_empty() {
                    output.push("      logs:".to_string());
                    for (log_idx, log) in r.receipt.inner.logs.iter().enumerate() {
                        output.push(format!(
                            "        Log #{}: address={}, topics_count={}",
                            log_idx,
                            log.address,
                            log.data.topics().len()
                        ));
                    }
                }
            }
        };
    }

    output.join("\n")
}

/// Executes a [ExecutorTestFixture] stored at the passed `fixture_path` and asserts that the
/// produced block hash matches the expected block hash.
pub async fn run_test_fixture(fixture_path: PathBuf) {
    // Initialize tracing subscriber for test output
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();

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
    let rollup_config = fixture.op_executor_test_fixture.rollup_config;
    let celo_rollup_config = CeloRollupConfig(rollup_config);
    let mut executor = CeloStatelessL2Builder::new(
        &celo_rollup_config,
        CeloEvmFactory::default(),
        provider,
        NoopTrieHinter,
        fixture.op_executor_test_fixture.parent_header.seal_slow(),
    );

    let outcome = executor.build_block(fixture.executing_payload).unwrap();

    // First check if hashes match
    let produced_hash = outcome.header.hash();
    let expected_hash = fixture.op_executor_test_fixture.expected_block_hash;

    if produced_hash != expected_hash {
        // If we have the expected header, show detailed field comparison
        if let Some(ref expected_header) = fixture.expected_header {
            let header_cmp = compare_headers(outcome.header.inner(), expected_header);

            // If receipts_root differs, show detailed receipt information
            if outcome.header.inner().receipts_root != expected_header.receipts_root {
                let produced_receipts = &outcome.execution_result.receipts;
                let receipt_details = format_receipt_details(produced_receipts);
                let receipt_summary = format!(
                    "\nReceipts root mismatch:\n  Produced receipts_root: {}\n  Expected receipts_root: {}\n  Number of receipts: {}\n\n  Produced receipts:\n{}",
                    outcome.header.inner().receipts_root,
                    expected_header.receipts_root,
                    produced_receipts.len(),
                    receipt_details
                );

                // Show header errors with receipt details
                if let Err(header_err) = header_cmp {
                    panic!("{}{}", header_err, receipt_summary);
                } else {
                    panic!("Receipts root mismatch only!{}", receipt_summary);
                }
            }

            // If we have a header error without receipt issues, show it
            if let Err(e) = header_cmp {
                panic!("{}", e);
            }
        } else {
            // Fall back to hash-only comparison
            panic!(
                "Produced header hash does not match expected hash:\n  produced={}\n  expected={}",
                produced_hash, expected_hash
            );
        }
    }
}

/// The test fixture format for the [`CeloStatelessL2Builder`].
#[derive(Debug, Serialize, Deserialize)]
pub struct ExecutorTestFixture {
    /// [`kona_executor::test_utils::ExecutorTestFixture`]
    pub op_executor_test_fixture: OpExecutorTestFixture,
    /// The executing payload attributes.
    pub executing_payload: CeloPayloadAttributes,
    /// The expected header (optional, for detailed error reporting).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_header: Option<Header>,
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
            expected_header: Some(executing_header.inner.clone()),
        };

        let mut executor = CeloStatelessL2Builder::new(
            rollup_config,
            CeloEvmFactory::default(),
            self,
            NoopTrieHinter,
            parent_header,
        );
        let outcome = executor.build_block(payload_attrs).expect("Failed to execute block");

        // Use detailed header comparison with receipt details
        let header_cmp = compare_headers(outcome.header.inner(), &executing_header.inner);

        // If receipts_root differs, show detailed receipt information
        if outcome.header.inner().receipts_root != executing_header.inner.receipts_root {
            let produced_receipts = &outcome.execution_result.receipts;
            let receipt_details = format_receipt_details(produced_receipts);
            let receipt_summary = format!(
                "\nReceipts root mismatch:\n  Produced receipts_root: {}\n  Expected receipts_root: {}\n  Number of receipts: {}\n\n  Produced receipts:\n{}",
                outcome.header.inner().receipts_root,
                executing_header.inner.receipts_root,
                produced_receipts.len(),
                receipt_details
            );

            // Show header errors with receipt details
            if let Err(header_err) = header_cmp {
                panic!("{}{}", header_err, receipt_summary);
            } else {
                panic!("Receipts root mismatch only!{}", receipt_summary);
            }
        }

        // If we have a header error without receipt issues, show it
        if let Err(e) = header_cmp {
            panic!("{}", e);
        }
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
