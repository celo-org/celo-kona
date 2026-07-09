//! Test utilities for the executor.

use crate::{CeloBlockBuildingOutcome, CeloStatelessL2Builder};
use alloy_celo_evm::CeloEvmFactory;
use alloy_consensus::Header;
use alloy_eips::BlockId;
use alloy_primitives::{B256, Bytes, Sealable, Sealed, keccak256, map::HashMap};
use alloy_provider::{Provider, network::primitives::BlockTransactions};
use alloy_rpc_types_debug::ExecutionWitness;
use alloy_rpc_types_engine::PayloadAttributes;
use celo_genesis::CeloRollupConfig;
use celo_registry::ROLLUP_CONFIGS;
use futures_util::stream::{StreamExt, TryStreamExt};
use kona_executor::{
    ExecutorResult, TrieDBProvider,
    test_utils::{DiskTrieNodeProvider, ExecutorTestFixtureCreator, TestTrieNodeProviderError},
};
use kona_mpt::{NoopTrieHinter, TrieNode, TrieProvider};
use kona_protocol::Predeploys;
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use rocksdb::{DB, Options};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::fs;

/// The test fixture format for the [`CeloStatelessL2Builder`].
///
/// Identical to kona's `ExecutorTestFixture` except that `rollup_config` is a
/// [`CeloRollupConfig`], so Celo-only settings — notably `upgrade18_time` and the CGT v2
/// activation-artifact param overrides — survive the round-trip through the tarball. A
/// [`CeloRollupConfig`] deserializes from a plain OP `rollup.json` document, so fixtures
/// generated before those fields existed still load.
#[derive(Debug, Serialize, Deserialize)]
pub struct CeloExecutorTestFixture {
    /// The rollup configuration for the executing chain.
    pub rollup_config: CeloRollupConfig,
    /// The parent block header.
    pub parent_header: alloy_consensus::Header,
    /// The executing payload attributes.
    pub executing_payload: OpPayloadAttributes,
    /// The expected block hash.
    pub expected_block_hash: B256,
}

/// A loaded [`CeloExecutorTestFixture`] and its backing temporary fixture directory.
#[derive(Debug)]
pub struct LoadedCeloExecutorTestFixture {
    /// Keeps the untarred fixture directory alive while the `RocksDB` provider is open.
    pub fixture_dir: tempfile::TempDir,
    /// The deserialized fixture metadata and payload.
    pub fixture: CeloExecutorTestFixture,
    /// Trie/database provider backed by the fixture's key-value store.
    pub provider: DiskTrieNodeProvider,
}

/// Loads a [`CeloExecutorTestFixture`] stored at the passed `fixture_path`.
pub async fn load_test_fixture(fixture_path: PathBuf) -> LoadedCeloExecutorTestFixture {
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

    let kv_store = DB::open(&fixture_kv_options(), fixture_dir.path().join("kv"))
        .unwrap_or_else(|e| panic!("Failed to open database at {fixture_dir:?}: {e}"));
    let fixture: CeloExecutorTestFixture =
        serde_json::from_slice(&fs::read(fixture_dir.path().join("fixture.json")).await.unwrap())
            .expect("Failed to deserialize fixture");

    LoadedCeloExecutorTestFixture {
        fixture_dir,
        fixture,
        provider: DiskTrieNodeProvider::new(kv_store),
    }
}

/// The `RocksDB` settings the fixture key-value store is created with and must be reopened with.
fn fixture_kv_options() -> Options {
    let mut options = Options::default();
    options.set_compression_type(rocksdb::DBCompressionType::Snappy);
    options.create_if_missing(true);
    options
}

/// Loads and executes a test fixture from a tarball path.
///
/// Returns the execution outcome and the fixture data for further inspection.
pub async fn load_and_execute_fixture(
    fixture_path: PathBuf,
) -> (CeloBlockBuildingOutcome, CeloExecutorTestFixture) {
    let (outcome, fixture) = execute_fixture_with(fixture_path, |_| {}).await;
    (outcome.expect("Failed to execute block"), fixture)
}

/// Loads a test fixture, applies `mutate` to its [`CeloRollupConfig`], and executes the block.
///
/// The mutation hook exists so tests can prove that a fixture actually *constrains* a piece of
/// consensus: perturb one config value, and the produced block hash must stop matching
/// `expected_block_hash`.
pub async fn execute_fixture_with(
    fixture_path: PathBuf,
    mutate: impl FnOnce(&mut CeloRollupConfig),
) -> (ExecutorResult<CeloBlockBuildingOutcome>, CeloExecutorTestFixture) {
    // `_fixture_dir` keeps the untarred fixture directory alive while the provider is in use.
    let LoadedCeloExecutorTestFixture { fixture_dir: _fixture_dir, mut fixture, provider } =
        load_test_fixture(fixture_path).await;

    mutate(&mut fixture.rollup_config);

    let mut executor = CeloStatelessL2Builder::new(
        &fixture.rollup_config,
        CeloEvmFactory::default(),
        provider,
        NoopTrieHinter,
        fixture.parent_header.clone().seal_slow(),
    );

    let outcome = executor.build_block(fixture.executing_payload.clone());
    (outcome, fixture)
}

/// Executes a [`CeloExecutorTestFixture`] stored at the passed `fixture_path` and asserts that the
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

/// Where the fixture creator sources the trie-node, bytecode and header preimages it needs to
/// statelessly re-execute the target block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PreimageSource {
    /// Probe `debug_executionWitness`, and fall back to [`Self::DbGet`] if the node does not
    /// serve it.
    #[default]
    Auto,
    /// Fetch preimages on demand via geth's raw database accessor, `debug_dbGet`. Requires an
    /// archival op-geth node; reth does not implement this method.
    DbGet,
    /// Collect every preimage up front from `debug_executionWitness`, which reth (and therefore a
    /// `celo-reth` dev node) serves. The witness covers execution and state root recomputation,
    /// but not quite everything this executor reads: the creator supplements it with an
    /// `eth_getProof` for the `L2ToL1MessagePasser` account, so the node must also serve
    /// historical proofs (`--rpc.eth-proof-window`).
    Witness,
}

/// A [`TrieDBProvider`] served entirely from an in-memory preimage map, keyed by `keccak256` of
/// the value — the same keying the fixture's on-disk key-value store uses.
#[derive(Debug)]
struct WitnessTrieNodeProvider {
    preimages: HashMap<B256, Bytes>,
}

impl WitnessTrieNodeProvider {
    fn get(&self, hash: B256) -> Result<&Bytes, TestTrieNodeProviderError> {
        self.preimages.get(&hash).ok_or_else(|| {
            tracing::error!(%hash, "preimage absent from the execution witness");
            TestTrieNodeProviderError::PreimageNotFound
        })
    }
}

impl TrieProvider for WitnessTrieNodeProvider {
    type Error = TestTrieNodeProviderError;

    fn trie_node_by_hash(&self, key: B256) -> Result<TrieNode, Self::Error> {
        use alloy_rlp::Decodable;
        TrieNode::decode(&mut self.get(key)?.as_ref()).map_err(TestTrieNodeProviderError::Rlp)
    }
}

impl TrieDBProvider for WitnessTrieNodeProvider {
    fn bytecode_by_hash(&self, code_hash: B256) -> Result<Bytes, Self::Error> {
        self.get(code_hash).cloned()
    }

    fn header_by_hash(&self, hash: B256) -> Result<alloy_consensus::Header, Self::Error> {
        use alloy_rlp::Decodable;
        alloy_consensus::Header::decode(&mut self.get(hash)?.as_ref())
            .map_err(TestTrieNodeProviderError::Rlp)
    }
}

/// Creates a static test fixture for the [`CeloStatelessL2Builder`] with the configuration
/// provided.
///
/// `rollup_config` overrides the [`static@ROLLUP_CONFIGS`] registry lookup, which only knows the
/// production Celo chains — a dev chain has to supply its own `rollup.json`. That path is also
/// how the Upgrade 18 activation-artifact param overrides reach the executor.
pub async fn create_static_fixture(
    provider_url: &str,
    block_number: u64,
    base_fixture_directory: PathBuf,
    rollup_config: Option<CeloRollupConfig>,
    preimage_source: PreimageSource,
) {
    let creator =
        ExecutorTestFixtureCreator::new(provider_url, block_number, base_fixture_directory);

    let rollup_config = match rollup_config {
        Some(config) => config,
        None => {
            let chain_id = creator.provider.get_chain_id().await.expect("Failed to get chain ID");
            ROLLUP_CONFIGS
                .get(&chain_id)
                .unwrap_or_else(|| {
                    panic!(
                        "No rollup config for chain {chain_id}; pass one explicitly with \
                         --rollup-config"
                    )
                })
                .clone()
        }
    };

    let (parent_header, executing_header, payload_attrs) =
        fetch_block_replay_inputs(&creator.provider, &rollup_config, creator.block_number)
            .await
            .expect("Failed to fetch block replay inputs");

    let fixture_path = creator.data_dir.join("fixture.json");
    let fixture = CeloExecutorTestFixture {
        rollup_config: rollup_config.clone(),
        parent_header: parent_header.inner().clone(),
        expected_block_hash: executing_header.hash_slow(),
        executing_payload: payload_attrs.clone(),
    };

    let witness = match preimage_source {
        PreimageSource::DbGet => None,
        PreimageSource::Witness => Some(
            fetch_execution_witness(&creator)
                .await
                .unwrap_or_else(|e| panic!("debug_executionWitness unavailable: {e}")),
        ),
        PreimageSource::Auto => match fetch_execution_witness(&creator).await {
            Ok(witness) => Some(witness),
            Err(e) => {
                tracing::info!(err = %e, "debug_executionWitness unavailable, falling back to debug_dbGet");
                None
            }
        },
    };

    // The executor is generic over its provider, so the two preimage sources take separate
    // arms rather than a boxed provider.
    let produced_header = if let Some(witness) = witness {
        let mut preimages = witness_preimages(witness);
        preimages.extend(fetch_message_passer_account_proof(&creator).await);
        store_preimages(&creator, &preimages).await;
        let provider = WitnessTrieNodeProvider { preimages };
        build_block(&rollup_config, provider, parent_header, payload_attrs)
    } else {
        build_block(&rollup_config, creator, parent_header, payload_attrs)
    };

    assert_eq!(
        produced_header.inner(),
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

/// Builds the fixture's block and returns the sealed header it produced.
fn build_block<P: TrieDBProvider + std::fmt::Debug>(
    rollup_config: &CeloRollupConfig,
    provider: P,
    parent_header: alloy_consensus::Sealed<alloy_consensus::Header>,
    payload_attrs: OpPayloadAttributes,
) -> alloy_consensus::Sealed<alloy_consensus::Header> {
    let mut executor = CeloStatelessL2Builder::new(
        rollup_config,
        CeloEvmFactory::default(),
        provider,
        NoopTrieHinter,
        parent_header,
    );
    executor.build_block(payload_attrs).expect("Failed to execute block").header
}

/// Asks the node for the target block's execution witness.
async fn fetch_execution_witness(
    creator: &ExecutorTestFixtureCreator,
) -> Result<ExecutionWitness, String> {
    creator
        .provider
        .client()
        .request::<[String; 1], ExecutionWitness>(
            "debug_executionWitness",
            [format!("0x{:x}", creator.block_number)],
        )
        .await
        .map_err(|e| e.to_string())
}

/// Fetches the account-trie path to the `L2ToL1MessagePasser` predeploy at the parent block.
///
/// The execution witness alone is not sufficient for [`CeloStatelessL2Builder`]. To seal an
/// Isthmus block the builder reads that predeploy's account out of the parent's account trie
/// (its storage root becomes the header's `withdrawals_root`), but the node derives the same
/// root from hashed-state cursors rather than by walking the trie, so the path never enters the
/// witness — unless the block happens to write to the account, as the Upgrade 18 transition
/// does. The account proof supplies exactly those nodes.
///
/// Requires the node to serve historical proofs (`--rpc.eth-proof-window`). Returns an empty map
/// on failure: for blocks that do touch the predeploy the witness already covers it, and if it
/// does not, the build fails with a loud missing-preimage error anyway.
async fn fetch_message_passer_account_proof(
    creator: &ExecutorTestFixtureCreator,
) -> HashMap<B256, Bytes> {
    let parent = BlockId::number(creator.block_number - 1);
    match creator
        .provider
        .get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, Vec::new())
        .block_id(parent)
        .await
    {
        Ok(proof) => {
            proof.account_proof.into_iter().map(|node| (keccak256(node.as_ref()), node)).collect()
        }
        Err(e) => {
            tracing::warn!(err = %e, "eth_getProof for the L2ToL1MessagePasser account failed");
            HashMap::default()
        }
    }
}

/// Indexes the witness preimages by `keccak256` of their value.
///
/// Trie nodes, contract bytecode and ancestor headers are all addressed by their keccak hash, so
/// one flat map serves `trie_node_by_hash`, `bytecode_by_hash` and `header_by_hash` alike. The
/// witness's fourth field, `keys`, holds the preimages of hashed account addresses and storage
/// slots; [`TrieDB`](kona_executor::TrieDB) hashes those itself and never looks them up, so they
/// are dropped rather than baked into the checked-in tarball.
fn witness_preimages(witness: ExecutionWitness) -> HashMap<B256, Bytes> {
    witness
        .state
        .into_iter()
        .chain(witness.codes)
        .chain(witness.headers)
        .map(|preimage| (keccak256(preimage.as_ref()), preimage))
        .collect()
}

/// Writes the collected preimages into the fixture's key-value store, which is what the
/// checked-in tarball ships and `load_test_fixture` reads back.
async fn store_preimages(creator: &ExecutorTestFixtureCreator, preimages: &HashMap<B256, Bytes>) {
    let kv_store = creator.kv_store.lock().await;
    for (hash, preimage) in preimages {
        kv_store.put(hash, preimage.as_ref()).expect("Failed to write preimage");
    }
}
