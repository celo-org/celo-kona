//! Tests for [`CeloExecutor`]'s implementation of the driver's [`Executor`] trait.

use alloy_consensus::{Header, Sealable};
use alloy_primitives::{B256, Bytes};
use celo_alloy_consensus::CeloReceiptEnvelope;
use celo_executor::CeloEvmFactory;
use celo_genesis::CeloRollupConfig;
use celo_proof::executor::CeloExecutor;
use kona_driver::Executor;
use kona_executor::{
    ExecutorError, NoopTrieDBProvider, TrieDBError, TrieDBProvider,
    test_utils::{DiskTrieNodeProvider, LoadedExecutorTestFixture, load_test_fixture},
};
use kona_genesis::RollupConfig;
use kona_mpt::{NoopTrieHinter, TrieNode, TrieProvider};
use std::{path::PathBuf, sync::Arc};

type NoopExecutor<'a> = CeloExecutor<'a, NoopTrieDBProvider, NoopTrieHinter>;

fn noop_executor(config: &CeloRollupConfig) -> NoopExecutor<'_> {
    CeloExecutor::new(config, NoopTrieDBProvider, NoopTrieHinter, CeloEvmFactory::default(), None)
}

#[test]
fn provider_errors_are_not_invalid_payloads() {
    let error = ExecutorError::TrieDBError(TrieDBError::Provider("missing witness".into()));

    assert!(!NoopExecutor::is_invalid_payload_error(&error));
}

#[test]
fn block_gas_limit_exceeded_is_an_invalid_payload() {
    assert!(NoopExecutor::is_invalid_payload_error(&ExecutorError::BlockGasLimitExceeded));
}

#[tokio::test]
async fn execute_payload_without_safe_head_reports_missing_executor() {
    let config = CeloRollupConfig::new(RollupConfig::default());
    let mut executor = noop_executor(&config);

    let result = executor.execute_payload(Default::default()).await;

    assert!(matches!(result, Err(ExecutorError::MissingExecutor)), "got {result:?}");
}

#[test]
fn compute_output_root_without_safe_head_reports_missing_executor() {
    let config = CeloRollupConfig::new(RollupConfig::default());
    let mut executor = noop_executor(&config);

    let result = executor.compute_output_root();

    assert!(matches!(result, Err(ExecutorError::MissingExecutor)), "got {result:?}");
}

/// A cloneable handle to the fixture's disk-backed provider, which `CeloExecutor` requires.
#[derive(Debug, Clone)]
struct SharedDiskProvider(Arc<DiskTrieNodeProvider>);

impl TrieProvider for SharedDiskProvider {
    type Error = <DiskTrieNodeProvider as TrieProvider>::Error;

    fn trie_node_by_hash(&self, key: B256) -> Result<TrieNode, Self::Error> {
        self.0.trie_node_by_hash(key)
    }
}

impl TrieDBProvider for SharedDiskProvider {
    fn bytecode_by_hash(&self, code_hash: B256) -> Result<Bytes, Self::Error> {
        self.0.bytecode_by_hash(code_hash)
    }

    fn header_by_hash(&self, hash: B256) -> Result<Header, Self::Error> {
        self.0.header_by_hash(hash)
    }
}

#[tokio::test]
async fn execute_payload_seals_the_block_and_keeps_cip64_receipts() {
    let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../executor/testdata/sepolia-cip64-erc20-transfer-tx_block-2346450.tar.gz");
    // `_fixture_dir` keeps the untarred fixture directory alive while the provider is in use.
    let LoadedExecutorTestFixture { fixture_dir: _fixture_dir, fixture, provider } =
        load_test_fixture(fixture_path).await;
    let config = CeloRollupConfig::new(fixture.rollup_config.clone());
    let mut executor = CeloExecutor::new(
        &config,
        SharedDiskProvider(Arc::new(provider)),
        NoopTrieHinter,
        CeloEvmFactory::default(),
        None,
    );
    let tx_count = fixture.executing_payload.transactions.as_ref().expect("fixture txs").len();

    executor.update_safe_head(fixture.parent_header.clone().seal_slow());
    let outcome =
        executor.execute_payload(fixture.executing_payload.clone()).await.expect("block executes");

    assert_eq!(outcome.header.hash(), outcome.header.inner().hash_slow(), "seal is not the hash");
    assert_eq!(outcome.header.hash(), fixture.expected_block_hash);
    assert_eq!(outcome.execution_result.receipts.len(), tx_count);
    assert!(
        outcome
            .execution_result
            .receipts
            .iter()
            .any(|receipt| matches!(receipt, CeloReceiptEnvelope::Cip64(_))),
        "no CIP-64 receipt survived the conversion"
    );
}
