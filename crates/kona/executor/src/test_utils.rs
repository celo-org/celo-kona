//! Test utilities for the executor.

use crate::{CeloBlockBuildingOutcome, CeloStatelessL2Builder};
use alloy_celo_evm::CeloEvmFactory;
use alloy_primitives::{B256, Bytes, Sealable};
use alloy_provider::{Provider, network::primitives::BlockTransactions};
use alloy_rpc_types_engine::PayloadAttributes;
use celo_genesis::CeloRollupConfig;
use celo_registry::ROLLUP_CONFIGS;
use kona_executor::test_utils::{
    ExecutorTestFixture, ExecutorTestFixtureCreator, LoadedExecutorTestFixture, load_test_fixture,
};
use kona_mpt::NoopTrieHinter;
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use std::path::PathBuf;
use tokio::fs;

/// Loads and executes a test fixture from a tarball path.
///
/// Returns the execution outcome and the fixture data for further inspection.
pub async fn load_and_execute_fixture(
    fixture_path: PathBuf,
) -> (CeloBlockBuildingOutcome, ExecutorTestFixture) {
    // `_fixture_dir` keeps the untarred fixture directory alive while the provider is in use.
    let LoadedExecutorTestFixture { fixture_dir: _fixture_dir, fixture, provider } =
        load_test_fixture(fixture_path).await;

    // Wrap RollupConfig to CeloRollupConfig
    let celo_rollup_config = CeloRollupConfig(fixture.rollup_config.clone());
    let mut executor = CeloStatelessL2Builder::new(
        &celo_rollup_config,
        CeloEvmFactory::default(),
        provider,
        NoopTrieHinter,
        fixture.parent_header.clone().seal_slow(),
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
        fixture.expected_block_hash,
        "Produced header does not match the expected header"
    );
}

/// Creates a static test fixture for the [`CeloStatelessL2Builder`] with the configuration
/// provided.
pub async fn create_static_fixture(
    provider_url: &str,
    block_number: u64,
    base_fixture_directory: PathBuf,
) {
    let creator =
        ExecutorTestFixtureCreator::new(provider_url, block_number, base_fixture_directory);

    let chain_id = creator.provider.get_chain_id().await.expect("Failed to get chain ID");
    let rollup_config = ROLLUP_CONFIGS.get(&chain_id).expect("Rollup config not found");

    let executing_block = creator
        .provider
        .get_block_by_number(creator.block_number.into())
        .await
        .expect("Failed to get parent block")
        .expect("Block not found");
    let parent_block = creator
        .provider
        .get_block_by_number((creator.block_number - 1).into())
        .await
        .expect("Failed to get parent block")
        .expect("Block not found");

    let executing_header = executing_block.header;
    let parent_header = parent_block.header.inner.seal_slow();

    let encoded_executing_transactions = match executing_block.transactions {
        BlockTransactions::Hashes(transactions) => {
            let mut encoded_transactions = Vec::with_capacity(transactions.len());
            for tx_hash in transactions {
                let tx = creator
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

    let payload_attrs = OpPayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp: executing_header.timestamp,
            parent_beacon_block_root: executing_header.parent_beacon_block_root,
            prev_randao: executing_header.mix_hash,
            withdrawals: Default::default(),
            suggested_fee_recipient: executing_header.beneficiary,
            slot_number: None,
        },
        gas_limit: Some(executing_header.gas_limit),
        transactions: Some(encoded_executing_transactions),
        no_tx_pool: None,
        eip_1559_params: rollup_config.is_holocene_active(executing_header.timestamp).then(|| {
            executing_header.extra_data[1..9]
                .try_into()
                .expect("Invalid header format for Holocene")
        }),
        min_base_fee: rollup_config.is_jovian_active(executing_header.timestamp).then(|| {
            // The min base fee is the bytes 9-17 of the extra data.
            executing_header.extra_data[9..17]
                .try_into()
                .map(u64::from_be_bytes)
                .expect("Invalid header format for Jovian")
        }),
    };

    let fixture_path = creator.data_dir.join("fixture.json");
    let fixture = ExecutorTestFixture {
        rollup_config: rollup_config.0.clone(),
        parent_header: parent_header.inner().clone(),
        expected_block_hash: executing_header.hash_slow(),
        executing_payload: payload_attrs.clone(),
    };

    let mut executor = CeloStatelessL2Builder::new(
        rollup_config,
        CeloEvmFactory::default(),
        creator,
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
