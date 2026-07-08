//! Celo L1 → L2 state-dump import for the `celo-reth import-celo-state` subcommand.

use alloy_consensus::{EMPTY_OMMER_ROOT_HASH, EMPTY_ROOT_HASH, Header};
use alloy_primitives::{B64, B256, U256, address, b256, bloom, bytes};
use clap::Parser;
use reth_chainspec::EthChainSpec;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::common::{Environment, EnvironmentArgs};
use reth_config::{Config, config::EtlConfig};
use reth_db::{DatabaseEnv, init_db};
use reth_db_common::init::{init_from_state_dump, insert_genesis_header};
use reth_node_builder::NodeTypesWithDBAdapter;
use reth_node_core::args::{DatabaseArgs, DatadirArgs, StaticFilesArgs, StorageArgs};
use reth_optimism_node::OpNode;
use reth_primitives_traits::{SealedHeader, header::HeaderMut};
use reth_provider::{
    BalConfig, BalStoreHandle, BlockHashReader, BlockNumReader, ChainSpecProvider, DBProvider,
    DatabaseProviderFactory, InMemoryBalStore, MetadataWriter, ProviderFactory,
    StageCheckpointWriter, StaticFileProviderFactory, StaticFileWriter, StorageSettings,
    StorageSettingsCache,
    providers::{RocksDBProvider, StaticFileProviderBuilder},
};
use reth_stages_types::{StageCheckpoint, StageId};
use reth_static_file_types::StaticFileSegment;
use std::{io::BufReader, path::PathBuf};
use tracing::{info, warn};

use crate::chainspec::CeloChainSpecParser;

pub use celo_revm::constants::CELO_MAINNET_CHAIN_ID;

/// Block number at which the Celo L1 → L2 migration occurs on Celo Mainnet.
pub const CEL2_MIGRATION_BLOCK_NUMBER: u64 = 31_056_500;

/// Hash of [`CEL2_HEADER`].
pub const CEL2_HEADER_HASH: B256 =
    b256!("0x7586014e20c69b3fa7c9070baf1a7edd95833db57853126f32593b455da2e5c5");

/// Cel2 migration header on Celo Mainnet (block `31_056_500`).
///
/// `extra_data` decodes to the ASCII string "Celo L2 migration".
pub const CEL2_HEADER: Header = Header {
    difficulty: U256::ZERO,
    extra_data: bytes!("43656c6f204c32206d6967726174696f6e"),
    gas_limit: 50_000_000,
    gas_used: 0,
    logs_bloom: bloom!(
        "00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
    ),
    nonce: B64::ZERO,
    number: CEL2_MIGRATION_BLOCK_NUMBER,
    parent_hash: b256!("0x4ecb0660a3b5e8bba3b3851e8926e9a44d0a61fe141e04c6e3e1c01644ce7c20"),
    receipts_root: EMPTY_ROOT_HASH,
    state_root: b256!("0xed980641a4bd4d2e84c6c8db980b7f05e95733c92be2e0045db3735efeb1d807"),
    timestamp: 1_742_957_258,
    transactions_root: EMPTY_ROOT_HASH,
    ommers_hash: EMPTY_OMMER_ROOT_HASH,
    beneficiary: address!("0x4200000000000000000000000000000000000011"),
    withdrawals_root: Some(EMPTY_ROOT_HASH),
    mix_hash: B256::ZERO,
    base_fee_per_gas: Some(0x5d240390e),
    blob_gas_used: Some(0),
    excess_blob_gas: Some(0),
    parent_beacon_block_root: Some(b256!(
        "0x6cb2e365f9d78b9071b90e8a1f4675d378cd0867b858571dc1b172ef1d3e085c"
    )),
    requests_hash: None,
    block_access_list_hash: None,
    slot_number: None,
};

/// Initialize a Celo Mainnet reth database from an L1 state dump.
///
/// Equivalent to `op-reth init-state --without-ovm` for Celo Mainnet but with the chain spec,
/// migration header, and migration block hardcoded — the user only supplies a datadir and the
/// merged state-dump file produced by `scripts/append_l2_allocs.py`.
#[derive(Debug, Parser)]
pub struct ImportCeloStateCommand {
    /// All datadir related arguments.
    #[command(flatten)]
    pub datadir: DatadirArgs,

    /// The path to the configuration file to use.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// All database related arguments.
    #[command(flatten)]
    pub db: DatabaseArgs,

    /// All static files related arguments.
    #[command(flatten)]
    pub static_files: StaticFilesArgs,

    /// JSONL state-dump file (must include the L2 allocs and the Cel2 state root as the first
    /// line — see `scripts/append_l2_allocs.py`).
    #[arg(value_name = "STATE_DUMP_FILE", verbatim_doc_comment)]
    pub state: PathBuf,
}

impl ImportCeloStateCommand {
    /// Execute the import.
    ///
    /// The `runtime` is forwarded to `open_env_skip_init_and_consistency_check` for parallel
    /// storage I/O.
    pub async fn execute(self, runtime: reth_tasks::Runtime) -> eyre::Result<()> {
        let chain = CeloChainSpecParser::parse("celo")?;

        if chain.chain_id() != CELO_MAINNET_CHAIN_ID {
            return Err(eyre::eyre!(
                "import-celo-state only supports Celo Mainnet (chain id {CELO_MAINNET_CHAIN_ID}), \
                 got {}",
                chain.chain_id()
            ));
        }

        info!(
            target: "reth::cli",
            chain = %chain.chain(),
            chain_id = chain.chain_id(),
            genesis_hash = ?chain.genesis_hash(),
            migration_block = CEL2_MIGRATION_BLOCK_NUMBER,
            "Importing Celo state",
        );
        info!(target: "reth::cli", "Active hardforks:\n{}", chain.display_hardforks());

        // v1 storage only — `--storage.v2` is not exposed by this subcommand.
        let env_args = EnvironmentArgs::<CeloChainSpecParser> {
            datadir: self.datadir,
            config: self.config,
            chain,
            db: self.db,
            static_files: self.static_files,
            storage: StorageArgs::default(),
        };

        // Open the storage environment WITHOUT writing the chainspec's genesis allocation
        // state. See [`open_env_skip_init_and_consistency_check`] for the precise rationale;
        // in short, `init_genesis_with_settings` would write the Celo Mainnet genesis alloc
        // — which includes one storage slot at the Registry contract — and the L1 final
        // state dump we ingest below writes a different value for that same slot, leaving
        // `HashedStorages` with a duplicate dup-entry that crashes the state-root
        // computation in `HashBuilder::add_leaf`.
        let Environment { config, provider_factory, .. } =
            open_env_skip_init_and_consistency_check(env_args, runtime)?;

        // Refuse anything but a truly uninitialized datadir before writing ANY metadata, so an
        // accidental run on an existing node can't clobber its state / settings / checkpoints.
        // Two reject cases:
        //   - a synced datadir: `last_block_number` is the tip (> 0);
        //   - a genesis-only / partially-initialized datadir (e.g. after a stock `init`, or an
        //     aborted import that already committed the bootstrap block): `last_block_number` is
        //     still 0, but a genesis header already exists. We insert the genesis header ourselves
        //     below, so its presence here means the DB was already initialized — importing onto it
        //     would mix state and then fail the state-root check after mutating the datadir.
        let (last_block_number, genesis_present) = {
            let provider = provider_factory.provider()?;
            (provider.last_block_number()?, provider.block_hash(0)?.is_some())
        };
        if last_block_number != 0 || genesis_present {
            return Err(eyre::eyre!(
                "import-celo-state requires an empty, uninitialized data directory \
                 (tip block #{last_block_number}, genesis header present: {genesis_present}); \
                 wipe the datadir and retry"
            ));
        }

        // Emit only the non-state side-effects of `init_genesis` that `setup_without_evm`
        // and `init_from_state_dump` rely on: storage settings, the canonical genesis
        // header, stage checkpoints at block 0, and empty block ranges for the static-file
        // segments that the import does not touch directly. We deliberately do NOT call
        // `insert_genesis_state` / `insert_genesis_hashes` / `insert_genesis_history` /
        // `compute_state_root` — those are the steps that would conflict with the L1 dump.
        {
            let provider_rw = provider_factory.database_provider_rw()?;
            provider_rw.write_storage_settings(StorageSettings::v1())?;
            // `write_storage_settings` persists the metadata but does NOT update the in-memory
            // storage-settings cache. The cache is seeded by `ProviderFactory::new`, which at the
            // current reth pin defaults a fresh datadir to v1 — so it already matches. We set it
            // explicitly so a future reth that defaults the cache to v2 (and/or makes the state
            // dump cache-sensitive) can't leave the cache diverging from the v1 metadata/import
            // layout we write here. See the https://github.com/celo-org/celo-kona/pull/198#discussion_r3328416058.
            provider_rw.set_storage_settings_cache(StorageSettings::v1());
            insert_genesis_header(&provider_rw, provider_factory.chain_spec().as_ref())?;
            let genesis_block_number = provider_factory.chain_spec().genesis_header().number;
            let checkpoint = StageCheckpoint::new(genesis_block_number);
            for stage in StageId::ALL {
                provider_rw.save_stage_checkpoint(stage, checkpoint)?;
            }
            let sf_provider = provider_rw.static_file_provider();
            sf_provider
                .get_writer(genesis_block_number, StaticFileSegment::Receipts)?
                .user_header_mut()
                .set_block_range(genesis_block_number, genesis_block_number);
            sf_provider
                .get_writer(genesis_block_number, StaticFileSegment::Transactions)?
                .user_header_mut()
                .set_block_range(genesis_block_number, genesis_block_number);
            provider_rw.commit()?;
        }

        let static_file_provider = provider_factory.static_file_provider();

        // Write the Cel2 migration header. `init_from_state_dump` below opens its own
        // provider and commits in chunks, so the header must be committed up front.
        {
            let provider_rw = provider_factory.database_provider_rw()?;

            reth_cli_commands::init_state::without_evm::setup_without_evm(
                &provider_rw,
                SealedHeader::new(CEL2_HEADER, CEL2_HEADER_HASH),
                |number| {
                    let mut header = Header::default();
                    header.set_number(number);
                    header
                },
            )?;

            // SAFETY: it's safe to commit static files; on crash they are unwound according to
            // database checkpoints. Required so the migration header is visible to the state
            // dump import below.
            static_file_provider.commit()?;
            provider_rw.commit()?;
        }

        // Verify the migration header is actually present in the database under the expected
        // hash before running the (expensive) state dump. A mismatch means setup_without_evm
        // did not persist the header we expect, and continuing would leave the datadir unusable.
        let stored = provider_factory
            .provider()?
            .block_hash(CEL2_MIGRATION_BLOCK_NUMBER)?
            .ok_or_else(|| {
                eyre::eyre!(
                    "migration block #{CEL2_MIGRATION_BLOCK_NUMBER} missing from database \
                     after setup_without_evm"
                )
            })?;
        if stored != CEL2_HEADER_HASH {
            return Err(eyre::eyre!(
                "migration block hash mismatch at #{CEL2_MIGRATION_BLOCK_NUMBER}: \
                 stored {stored:?} != expected {CEL2_HEADER_HASH:?}"
            ));
        }

        info!(target: "reth::cli", path = ?self.state, "Initiating state dump");

        let reader = BufReader::new(reth_fs_util::open(self.state)?);
        let hash = init_from_state_dump(reader, &provider_factory, config.stages.etl)?;

        info!(
            target: "reth::cli",
            state_root = ?hash,
            migration_hash = ?stored,
            "Migration block written",
        );
        Ok(())
    }
}

/// Open the storage environment for read-write access while skipping both
/// `init_genesis_with_settings` and the static-file/database consistency check that
/// `EnvironmentArgs::init` runs.
///
/// # Why this exists
///
/// `EnvironmentArgs::init` ends with two side effects that break Celo's offline
/// migration tools — `import-celo-state` and `celo-migrate-v2`:
///
/// 1. `init_genesis_with_settings` writes the chainspec genesis alloc into `PlainAccountState` /
///    `PlainStorageState` / `HashedAccounts` / `HashedStorages` / history / trie. For Celo Mainnet
///    the alloc includes one storage slot on the Registry at
///    `0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103`, which collides with the
///    L1 dump's value for that same slot and causes `HashBuilder::add_leaf` to panic with `key ==
///    self.key`.
///
/// 2. `create_provider_factory`'s static-file/database consistency check is destructive: it asserts
///    (`assert_ne!(unwind_target, PipelineTarget::Unwind(0), …)`) when the only safe heal would be
///    a full unwind to block 0. A freshly imported Celo datadir has SenderRecovery's stage
///    checkpoint at the migration block (31,056,500) but an empty `TransactionSenders` static-file
///    segment, so the check picks an unwind-to-zero target and the assertion fires — even though
///    `celo-migrate-v2` knows about that inconsistency and will fix it via
///    `seed_transaction_senders`.
///
/// This helper inlines `EnvironmentArgs::init`'s setup (resolve paths, load `Config`,
/// open MDBX / static files / RocksDB, build the `ProviderFactory`) and stops before
/// either of those two side effects runs. Callers are responsible for any post-open
/// initialization (e.g. `import-celo-state` writes the genesis header and stage
/// checkpoints itself; `celo-migrate-v2` writes nothing because the datadir already has
/// them).
pub(crate) fn open_env_skip_init_and_consistency_check(
    args: EnvironmentArgs<CeloChainSpecParser>,
    runtime: reth_tasks::Runtime,
) -> eyre::Result<Environment<OpNode>> {
    let data_dir = args.datadir.clone().resolve_datadir(args.chain.chain());
    let db_path = data_dir.db();
    let sf_path = data_dir.static_files();
    let rocksdb_path = data_dir.rocksdb();

    reth_fs_util::create_dir_all(&db_path)?;
    reth_fs_util::create_dir_all(&sf_path)?;
    reth_fs_util::create_dir_all(&rocksdb_path)?;

    let config_path = args.config.clone().unwrap_or_else(|| data_dir.config());

    let mut config = Config::from_path(config_path)
        .inspect_err(
            |err| warn!(target: "reth::cli", %err, "Failed to load config file, using default"),
        )
        .unwrap_or_default();

    if config.stages.etl.dir.is_none() {
        config.stages.etl.dir = Some(EtlConfig::from_datadir(data_dir.data_dir()));
    }
    if config.stages.era.folder.is_none() {
        config.stages.era = config.stages.era.with_datadir(data_dir.data_dir());
    }

    info!(target: "reth::cli", ?db_path, ?sf_path, "Opening storage");
    let genesis_block_number = args.chain.genesis().number.unwrap_or_default();

    let db = init_db(db_path, args.db.database_args())?;
    let sfp = StaticFileProviderBuilder::read_write(sf_path)
        .with_metrics()
        .with_genesis_block_number(genesis_block_number)
        .build()?;

    let rocksdb_provider = {
        let mut builder = RocksDBProvider::builder(data_dir.rocksdb())
            .with_default_tables()
            .with_database_log_level(args.db.log_level)
            .with_read_only(false);
        if let Some(cache_size) = args.db.rocksdb_block_cache_size {
            builder = builder.with_block_cache_size(cache_size);
        }
        builder.build()?
    };

    // Mirror upstream `EnvironmentArgs::create_provider_factory`: attach an in-memory BAL store.
    // Unused by Celo (BAL/EIP-7928 is inactive) and by these offline commands, but kept in lockstep
    // with upstream so the factory setup stays faithful to what the normal node start path builds.
    let balstore_cache_size =
        args.db.balstore_cache_size.unwrap_or(BalConfig::DEFAULT_IN_MEMORY_RETENTION_DISTANCE);
    let bal_store = BalStoreHandle::new(InMemoryBalStore::new(
        BalConfig::with_in_memory_retention_distance(balstore_cache_size),
    ));
    let provider_factory = ProviderFactory::<NodeTypesWithDBAdapter<OpNode, DatabaseEnv>>::new(
        db,
        args.chain,
        sfp,
        rocksdb_provider,
        runtime,
    )?
    .with_prune_modes(config.prune.segments.clone())
    .with_minimum_pruning_distance(config.prune.minimum_pruning_distance)
    .with_bal_store(bal_store);

    Ok(Environment { config, provider_factory, data_dir })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cel2_header_hash_matches() {
        assert_eq!(CEL2_HEADER.hash_slow(), CEL2_HEADER_HASH);
    }

    #[test]
    fn cel2_extra_data_decodes_to_celo_l2_migration() {
        assert_eq!(
            std::str::from_utf8(CEL2_HEADER.extra_data.as_ref()).unwrap(),
            "Celo L2 migration",
        );
    }

    #[test]
    fn parse_minimal_cli() {
        let cmd = ImportCeloStateCommand::parse_from([
            "import-celo-state",
            "--datadir",
            "/tmp/celo-data",
            "/tmp/state.jsonl",
        ]);
        assert_eq!(cmd.state.to_str().unwrap(), "/tmp/state.jsonl");
        assert!(cmd.config.is_none());
    }
}
