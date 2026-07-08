//! Test utilities for the executor.

use crate::{CeloBlockBuildingOutcome, CeloStatelessL2Builder};
use alloy_celo_evm::CeloEvmFactory;
use alloy_consensus::Header;
use alloy_primitives::{B256, Bytes, Sealable, Sealed};
use alloy_provider::{Provider, network::primitives::BlockTransactions};
use alloy_rpc_types_engine::PayloadAttributes;
use celo_genesis::CeloRollupConfig;
use celo_registry::ROLLUP_CONFIGS;
use futures_util::stream::{StreamExt, TryStreamExt};
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
    let celo_rollup_config = CeloRollupConfig::new(fixture.rollup_config.clone());
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

/// Reconstructs the [`OpPayloadAttributes`] that replay the block with `header`.
///
/// The `extra_data` layout is fork-dependent: Holocene packs the EIP-1559 params
/// into bytes `1..9` (after the version byte), Jovian appends the min base fee in
/// bytes `9..17`. Errors on `extra_data` too short for the active forks.
///
/// Fork of the inline payload-attrs reconstruction in kona's
/// `ExecutorTestFixtureCreator::create_static_fixture` (op-reth v2.3.1); upstream keeps it
/// inline with no free helper to wrap, and the only divergence is that upstream's panics on
/// malformed `extra_data` become `Err`s here. Re-diff against upstream when bumping op-reth.
pub fn payload_attributes_from_header(
    rollup_config: &CeloRollupConfig,
    header: &Header,
    encoded_transactions: Vec<Bytes>,
) -> Result<OpPayloadAttributes, String> {
    let eip_1559_params = rollup_config
        .is_holocene_active(header.timestamp)
        .then(|| {
            header.extra_data.get(1..9).and_then(|b| b.try_into().ok()).ok_or_else(|| {
                "Invalid header extra_data for Holocene (EIP-1559 params in bytes 1..9)".to_string()
            })
        })
        .transpose()?;
    let min_base_fee = rollup_config
        .is_jovian_active(header.timestamp)
        .then(|| {
            header
                .extra_data
                .get(9..17)
                .and_then(|b| <[u8; 8]>::try_from(b).ok())
                .map(u64::from_be_bytes)
                .ok_or_else(|| {
                    "Invalid header extra_data for Jovian (min base fee in bytes 9..17)".to_string()
                })
        })
        .transpose()?;

    Ok(OpPayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp: header.timestamp,
            parent_beacon_block_root: header.parent_beacon_block_root,
            prev_randao: header.mix_hash,
            withdrawals: Default::default(),
            suggested_fee_recipient: header.beneficiary,
            slot_number: None,
        },
        gas_limit: Some(header.gas_limit),
        transactions: Some(encoded_transactions),
        no_tx_pool: None,
        eip_1559_params,
        min_base_fee,
    })
}

/// Fetches everything needed to replay `block_number` through the
/// [`CeloStatelessL2Builder`]: the sealed parent header, the executing block's
/// consensus header, and the payload attributes reconstructed via
/// [`payload_attributes_from_header`].
pub async fn fetch_block_replay_inputs<P: Provider>(
    provider: &P,
    rollup_config: &CeloRollupConfig,
    block_number: u64,
) -> Result<(Sealed<Header>, Header, OpPayloadAttributes), String> {
    let (executing_block, parent_block) = tokio::try_join!(
        async {
            provider
                .get_block_by_number(block_number.into())
                .await
                .map_err(|e| format!("Failed to fetch executing block {block_number}: {e}"))
        },
        async {
            provider
                .get_block_by_number((block_number - 1).into())
                .await
                .map_err(|e| format!("Failed to fetch parent block {}: {e}", block_number - 1))
        },
    )?;
    let executing_block =
        executing_block.ok_or_else(|| format!("Block {block_number} not found"))?;
    let parent_block =
        parent_block.ok_or_else(|| format!("Parent block {} not found", block_number - 1))?;

    let BlockTransactions::Hashes(tx_hashes) = executing_block.transactions else {
        return Err("Only BlockTransactions::Hashes are supported.".to_string());
    };
    // Bound the per-block `debug_getRawTransaction` fan-out: the execution verifier already
    // replays many blocks concurrently, so an unbounded `try_join_all` here would cause
    // many simultaneous RPC calls.
    const RAW_TX_FETCH_CONCURRENCY: usize = 2;
    let encoded_transactions = futures_util::stream::iter(tx_hashes.into_iter().map(|tx_hash| {
        let client = provider.client();
        async move {
            client
                .request::<[B256; 1], Bytes>("debug_getRawTransaction", [tx_hash])
                .await
                .map_err(|e| format!("Failed to get raw transaction {tx_hash}: {e}"))
        }
    }))
    .buffered(RAW_TX_FETCH_CONCURRENCY)
    .try_collect::<Vec<_>>()
    .await?;

    let executing_header = executing_block.header.inner;
    let parent_header = parent_block.header.inner.seal_slow();
    let payload_attrs =
        payload_attributes_from_header(rollup_config, &executing_header, encoded_transactions)?;
    Ok((parent_header, executing_header, payload_attrs))
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

    let (parent_header, executing_header, payload_attrs) =
        fetch_block_replay_inputs(&creator.provider, rollup_config, creator.block_number)
            .await
            .expect("Failed to fetch block replay inputs");

    let fixture_path = creator.data_dir.join("fixture.json");
    let fixture = ExecutorTestFixture {
        rollup_config: rollup_config.op_rollup_config.clone(),
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
        &executing_header,
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
