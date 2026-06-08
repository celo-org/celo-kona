//! Celo-aware v1 → v2 storage migration for the `celo-reth celo-migrate-v2` subcommand.
//!
//! Upstream `reth db migrate-v2` clears the recomputable MDBX tables and resets six
//! pipeline stage checkpoints to 0, on the assumption that the pipeline will rebuild
//! the cleared data by re-iterating blocks 1..tip. Celo's L2 datadir is bootstrapped
//! via `import-celo-state`, which fills blocks 1..31_056_499 with header-only dummy
//! placeholders — they have no bodies, so SenderRecovery / TransactionLookup /
//! IndexAccount/StorageHistory / MerkleExecute cannot rebuild from them. Running the
//! upstream command leaves the node unable to start (block meta missing for #1).
//!
//! For a Celo-imported datadir the recomputable tables are already in the correct
//! post-migration state (empty for the dummy range, populated only at and beyond the
//! migration header). This subcommand reuses the upstream changeset/receipt migration
//! and the storage-settings flip, then skips the table-clear + stage-reset phase that
//! breaks Celo. The remaining MDBX compaction still runs.

use clap::Parser;
use reth_chainspec::EthChainSpec;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::common::{CliNodeTypes, Environment, EnvironmentArgs};
use reth_db::{
    DatabaseEnv,
    mdbx::{self, ffi},
    models::StorageBeforeTx,
};
use reth_db_api::{
    cursor::DbCursorRO,
    database::Database,
    tables,
    transaction::{DbTx, DbTxMut},
};
use reth_node_builder::NodeTypesWithDBAdapter;
use reth_node_core::args::{DatabaseArgs, DatadirArgs, StaticFilesArgs};
use reth_optimism_node::OpNode;
use reth_provider::{
    DBProvider, DatabaseProviderFactory, MetadataProvider, MetadataWriter, ProviderFactory,
    PruneCheckpointReader, RocksDBProviderFactory, StaticFileProviderFactory, StaticFileWriter,
    StorageSettings, providers::ProviderNodeTypes,
};
use reth_prune_types::PruneSegment;
use reth_stages_types::StageId;
use reth_static_file_types::StaticFileSegment;
use reth_storage_api::StageCheckpointReader;
use std::path::PathBuf;
use tracing::info;

use crate::{
    chainspec::CeloChainSpecParser,
    state_import::{
        CEL2_MIGRATION_BLOCK_NUMBER, CELO_MAINNET_CHAIN_ID,
        open_env_skip_init_and_consistency_check,
    },
};

/// Celo-aware v1 → v2 storage migration.
///
/// Behaves like the upstream `reth db migrate-v2` except it does **not** clear the
/// recomputable MDBX tables nor reset the SenderRecovery / TransactionLookup /
/// IndexAccountHistory / IndexStorageHistory / MerkleExecute / MerkleUnwind stage
/// checkpoints. On a Celo-imported datadir, those tables are already correct
/// (empty for the dummy pre-migration range, populated at and beyond the migration
/// header) and the stages are already at the migration block. Clearing + resetting
/// them as upstream does would force a pipeline rebuild from block 0, which fails
/// because the dummy blocks have no bodies for SenderRecovery to process.
#[derive(Debug, Parser)]
pub struct CeloMigrateV2Command {
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
}

impl CeloMigrateV2Command {
    /// Execute the celo-aware v1 → v2 migration.
    pub async fn execute(self, runtime: reth_tasks::Runtime) -> eyre::Result<()> {
        let chain = CeloChainSpecParser::parse("celo")?;

        if chain.chain_id() != CELO_MAINNET_CHAIN_ID {
            return Err(eyre::eyre!(
                "celo-migrate-v2 only supports Celo Mainnet (chain id {CELO_MAINNET_CHAIN_ID}), \
                 got {}",
                chain.chain_id()
            ));
        }

        info!(
            target: "reth::cli",
            chain = %chain.chain(),
            chain_id = chain.chain_id(),
            "Starting Celo-aware v1 → v2 storage migration",
        );

        // Force v1 on open so the existing v1 datadir is accepted. The migration itself flips
        // the metadata to v2 before returning.
        let env_args = EnvironmentArgs::<CeloChainSpecParser> {
            datadir: self.datadir,
            config: self.config,
            chain,
            db: self.db,
            static_files: self.static_files,
            storage: reth_node_core::args::StorageArgs::default(),
        };

        // A v1 datadir freshly produced by `import-celo-state` has SenderRecovery's stage
        // checkpoint at the migration block but an empty `TransactionSenders` static-file
        // segment. `EnvironmentArgs::init`'s consistency check would pick a destructive
        // "unwind to block 0" target and trip the upstream assertion before
        // `seed_transaction_senders` (below) gets a chance to fix the inconsistency. The
        // shared helper opens the env without running that consistency check, and also
        // skips `init_genesis_with_settings` (which is harmless here because block 0 is
        // already present, but keeping the call patterns identical across both Celo-only
        // subcommands makes the bypass logic easier to reason about).
        let Environment { provider_factory, .. } =
            open_env_skip_init_and_consistency_check(env_args, runtime)?;

        Self::execute_with_factory::<OpNode>(provider_factory).await
    }

    /// Run the migration phases against a pre-opened provider factory.
    async fn execute_with_factory<N: CliNodeTypes>(
        provider_factory: ProviderFactory<NodeTypesWithDBAdapter<N, DatabaseEnv>>,
    ) -> eyre::Result<()>
    where
        N::Primitives: reth_primitives_traits::NodePrimitives<
                Receipt: reth_db_api::table::Value + reth_codecs::Compact,
            >,
    {
        // === Phase 0: Preflight ===
        let provider = provider_factory.provider()?;
        let current_settings = provider.storage_settings()?;

        if current_settings.is_some_and(|s| s.is_v2()) {
            // A v2 datadir is a genuine no-op ONLY if a prior migration ran to completion. Phase 4
            // empties the MDBX `AccountsHistory` / `StoragesHistory` tables, so on a finished
            // migration — and on a natively-synced v2 node, which never writes history to MDBX —
            // both read empty. If either is still populated, a previous run crashed after the
            // Phase 3 v2 flip but before the Phase 4 clear, leaving the RocksDB v2 history index
            // incomplete. We must NOT silently report success and deliberately do NOT resume a
            // partial migration — fail loudly so the operator re-imports a clean v1
            // datadir.
            drop(provider);
            if Self::v2_history_tables_cleared(&provider_factory)? {
                info!(target: "reth::cli", "Storage is already v2, nothing to do");
                return Ok(());
            }
            eyre::bail!(
                "Datadir is in a partially-migrated v2 state: StorageSettings is v2 but the MDBX \
                 AccountsHistory/StoragesHistory tables are still populated, which means a \
                 previous celo-migrate-v2 run crashed after flipping to v2 (Phase 3) but before \
                 clearing the MDBX history (Phase 4). The RocksDB v2 history index is therefore \
                 incomplete, so the node wouldn't serve historical state fetch once it syncs past \
                 the in-memory canonical buffer. celo-migrate-v2 cannot resume a partial migration: \
                 re-run import-celo-state to rebuild a clean v1 datadir, then run celo-migrate-v2 again."
            );
        }

        let tip =
            provider.get_stage_checkpoint(StageId::Execution)?.map(|c| c.block_number).unwrap_or(0);

        info!(target: "reth::cli", tip, "Chain tip block number");

        // Safety guard: `seed_transaction_senders` writes a zero-entry senders segment up to
        // `tip`, which is only correct when blocks `0..=tip` contain no transactions. That holds
        // only for the fresh post-import shape; on a synced datadir real txs exist and seeding
        // zero senders would silently corrupt the segment. Refuse anything but the import block.
        if tip != CEL2_MIGRATION_BLOCK_NUMBER {
            eyre::bail!(
                "celo-migrate-v2 only supports a freshly imported, pre-sync Celo Mainnet datadir: \
                 the Execution stage checkpoint must equal the migration block \
                 #{CEL2_MIGRATION_BLOCK_NUMBER}, but found #{tip}. Run this immediately after \
                 `import-celo-state`, before starting the node."
            );
        }

        let sf_provider = provider_factory.static_file_provider();
        for segment in [StaticFileSegment::AccountChangeSets, StaticFileSegment::StorageChangeSets]
        {
            if sf_provider.get_highest_static_file_block(segment).is_some() {
                eyre::bail!(
                    "Static file segment {segment:?} already contains data. \
                     Cannot migrate — target must be empty."
                );
            }
        }

        drop(provider);

        // === Phase 1: Migrate changesets → static files ===
        Self::migrate_account_changesets(&provider_factory, tip)?;
        Self::migrate_storage_changesets(&provider_factory, tip)?;

        // === Phase 2: Migrate receipts → static files ===
        Self::migrate_receipts::<NodeTypesWithDBAdapter<N, DatabaseEnv>>(&provider_factory, tip)?;

        // === Phase 2b (Celo-specific): Seed TransactionSenders static-file segment ===
        //
        // In v2, `TransactionSenders` is tracked as a `StaticFileSegment`. The reth
        // consistency check at startup compares the SenderRecovery stage checkpoint against
        // the highest block recorded in the segment; an absent segment with a non-zero stage
        // checkpoint triggers an "unwind to block 0" panic. Upstream avoids this by clearing
        // the senders MDBX table, resetting the stage to 0, and relying on the pipeline to
        // rebuild the segment as it iterates blocks. We skip that rebuild for Celo (dummy
        // pre-migration blocks block the pipeline), so we must instead seed the segment so
        // its highest_block matches the stage checkpoint at `tip` (= migration block for a
        // Celo-imported datadir). The dummy blocks have no transactions, so the segment is
        // logically empty up to `tip` — we just need the boundary to advance.
        Self::seed_transaction_senders(&provider_factory, tip)?;

        // === Phase 3: Flip metadata to v2 ===
        info!(target: "reth::cli", "Writing StorageSettings v2 metadata");
        {
            let provider_rw = provider_factory.database_provider_rw()?;
            provider_rw.write_storage_settings(StorageSettings::v2())?;
            provider_rw.commit()?;
        }
        info!(target: "reth::cli", "Storage settings updated to v2");

        // === Phase 3b: Copy MDBX history indices → RocksDB ===
        //
        // Phase 4 below clears the MDBX `AccountsHistory` / `StoragesHistory` tables. In v2
        // those indices are served from RocksDB, but nothing has ever copied the imported
        // pre-migration history into RocksDB — the v2 write path only indexes blocks the node
        // executes, and the upstream IndexHistory pipeline stages we skip (dummy blocks have no
        // bodies) never run. We must move them here, after the v2 flip so RocksDB is the active
        // store and before the clear removes the only copy. Without this the history index is
        // empty, so every historical-state read past the in-memory canonical buffer misses and
        // reads as an empty account — including the FeeCurrencyDirectory, which surfaces as
        // "fee currency not registered" and stalls the chain (#192).
        Self::migrate_account_history(&provider_factory)?;
        Self::migrate_storage_history(&provider_factory)?;

        // === Phase 4: Clear MDBX tables superseded by the v2 layout ===
        //
        // After the flip these MDBX tables are never read in v2 (changesets come from static
        // files; TransactionSenders from its static-file segment; TransactionHashNumbers /
        // AccountsHistory / StoragesHistory from RocksDB), so leaving them only bloats the
        // compacted DB. Clear them to reclaim space.
        //
        // We deliberately do NOT clear PlainAccountState / PlainStorageState / HashedAccounts /
        // HashedStorages / AccountsTrie / StoragesTrie (still read from MDBX in v2) and do NOT
        // reset stage checkpoints. Upstream clears those too and rebuilds via the pipeline; for a
        // Celo import that rebuild is impossible (blocks 1..migration are header-only dummies with
        // no bodies) and they are already correct at the migration block.
        info!(target: "reth::cli", "Clearing MDBX tables superseded by the v2 layout");
        {
            let provider_rw = provider_factory.database_provider_rw()?;
            let tx = provider_rw.tx_ref();
            tx.clear::<tables::AccountChangeSets>()?;
            tx.clear::<tables::StorageChangeSets>()?;
            tx.clear::<tables::TransactionSenders>()?;
            tx.clear::<tables::TransactionHashNumbers>()?;
            tx.clear::<tables::AccountsHistory>()?;
            tx.clear::<tables::StoragesHistory>()?;
            provider_rw.commit()?;
        }

        // === Phase 5: Compact MDBX ===
        // Owned path is required: `provider_factory` is dropped below before `db_path` is used
        // in the swap. `redundant_clone` (nursery) false-positives across the `drop`.
        #[allow(clippy::redundant_clone)]
        let db_path = provider_factory.db_ref().path().to_path_buf();
        Self::compact_mdbx(provider_factory.db_ref())?;

        // Drop to release the DB handle so we can swap the directory in.
        drop(provider_factory);

        let compact_path = db_path.with_file_name("db_compact");
        Self::swap_compacted_db(&db_path, &compact_path)?;

        info!(
            target: "reth::cli",
            "Celo migration complete. You can now start celo-reth normally; no pipeline \
             rebuild is required.",
        );
        Ok(())
    }

    /// Whether a v2 datadir's MDBX history tables have already been cleared (i.e. Phase 4 ran).
    ///
    /// A completed migration empties `AccountsHistory` / `StoragesHistory` in MDBX, and a
    /// natively-synced v2 node never writes history to MDBX, so for both the tables read empty.
    /// If either is still populated, a previous `celo-migrate-v2` run crashed after the Phase 3
    /// v2 flip but before the Phase 4 clear, leaving the RocksDB v2 history index incomplete; the
    /// Phase 0 guard uses this to fail loudly instead of silently reporting success.
    fn v2_history_tables_cleared<N: ProviderNodeTypes>(
        factory: &ProviderFactory<N>,
    ) -> eyre::Result<bool> {
        let provider = factory.provider()?;
        let mut accounts = provider.tx_ref().cursor_read::<tables::AccountsHistory>()?;
        let mut storages = provider.tx_ref().cursor_read::<tables::StoragesHistory>()?;
        Ok(accounts.first()?.is_none() && storages.first()?.is_none())
    }

    // The following helpers are adapted from the upstream private associated functions on
    // `reth_cli_commands::db::migrate_v2::Command`. They are duplicated here because upstream
    // marks them as `fn` (private), and we need to call them from the celo-reth crate. Keep
    // them in sync with upstream when bumping the reth pin — EXCEPT for the Celo-specific
    // chunked commits in `migrate_account_changesets` / `migrate_storage_changesets` (see the
    // note inside each), which must be re-applied after any upstream sync.

    fn migrate_account_changesets<N: ProviderNodeTypes>(
        factory: &ProviderFactory<N>,
        tip: u64,
    ) -> eyre::Result<()> {
        info!(target: "reth::cli", "Migrating AccountChangeSets → static files");
        let provider = factory.provider()?.disable_long_read_transaction_safety();
        let sf_provider = factory.static_file_provider();

        let mut cursor = provider.tx_ref().cursor_read::<tables::AccountChangeSets>()?;

        let first_block = provider
            .get_prune_checkpoint(PruneSegment::AccountHistory)?
            .and_then(|cp| cp.block_number)
            .map_or(0, |b| b + 1);

        let mut writer = sf_provider.latest_writer(StaticFileSegment::AccountChangeSets)?;
        if first_block > 0 {
            writer.ensure_at_block(first_block - 1)?;
        }

        // Celo deviation from the upstream copy: commit every `COMMIT_INTERVAL_BLOCKS`. A single
        // commit over Celo's ~31M dummy pre-migration blocks accumulates per-block segment
        // metadata in the writer until flush — the same unbounded growth that OOM'd
        // `seed_transaction_senders` at 18 GB (see its docs). Chunked commits bound memory.
        // 500k aligns with the static-file segment boundary.
        const COMMIT_INTERVAL_BLOCKS: u64 = 500_000;

        let mut count = 0u64;
        let mut blocks_since_commit = 0u64;
        let mut walker = cursor.walk(Some(first_block))?.peekable();

        for block in first_block..=tip {
            let mut entries = Vec::new();

            while let Some(Ok((block_number, _))) = walker.peek() {
                if *block_number != block {
                    break;
                }
                let (_, entry) = walker.next().expect("peeked")?;
                entries.push(entry);
            }

            count += entries.len() as u64;
            writer.append_account_changeset(entries, block)?;

            blocks_since_commit += 1;
            if blocks_since_commit >= COMMIT_INTERVAL_BLOCKS {
                writer.commit()?;
                blocks_since_commit = 0;
            }
        }

        writer.commit()?;

        info!(target: "reth::cli", count, "AccountChangeSets migrated");
        Ok(())
    }

    fn migrate_storage_changesets<N: ProviderNodeTypes>(
        factory: &ProviderFactory<N>,
        tip: u64,
    ) -> eyre::Result<()> {
        info!(target: "reth::cli", "Migrating StorageChangeSets → static files");
        let provider = factory.provider()?.disable_long_read_transaction_safety();
        let sf_provider = factory.static_file_provider();

        let mut cursor = provider.tx_ref().cursor_read::<tables::StorageChangeSets>()?;

        let first_block = provider
            .get_prune_checkpoint(PruneSegment::StorageHistory)?
            .and_then(|cp| cp.block_number)
            .map_or(0, |b| b + 1);

        let mut writer = sf_provider.latest_writer(StaticFileSegment::StorageChangeSets)?;
        if first_block > 0 {
            writer.ensure_at_block(first_block - 1)?;
        }

        // Celo deviation from the upstream copy: chunked commits to bound writer memory over
        // Celo's ~31M dummy pre-migration blocks. See the note in `migrate_account_changesets`.
        const COMMIT_INTERVAL_BLOCKS: u64 = 500_000;

        let mut count = 0u64;
        let mut blocks_since_commit = 0u64;
        let mut walker =
            cursor.walk(Some((first_block, alloy_primitives::Address::ZERO).into()))?.peekable();

        for block in first_block..=tip {
            let mut entries = Vec::new();

            while let Some(Ok((key, _))) = walker.peek() {
                if key.block_number() != block {
                    break;
                }
                let (key, entry) = walker.next().expect("peeked")?;
                entries.push(StorageBeforeTx {
                    address: key.address(),
                    key: entry.key,
                    value: entry.value,
                });
            }

            count += entries.len() as u64;
            writer.append_storage_changeset(entries, block)?;

            blocks_since_commit += 1;
            if blocks_since_commit >= COMMIT_INTERVAL_BLOCKS {
                writer.commit()?;
                blocks_since_commit = 0;
            }
        }

        writer.commit()?;

        info!(target: "reth::cli", count, "StorageChangeSets migrated");
        Ok(())
    }

    /// Copy the MDBX `AccountsHistory` index into the v2 RocksDB store.
    ///
    /// MDBX and RocksDB use identical `ShardedKey` / `BlockNumberList` encoding and the same
    /// per-shard chunking, so every `(ShardedKey, BlockNumberList)` pair is copied verbatim —
    /// no regrouping by address or re-sharding. (`append_account_history_shard` is deliberately
    /// avoided: it reads existing shards from committed state and must be called at most once per
    /// address per batch, which a verbatim shard-by-shard walk would violate for any
    /// multi-shard address.) The batch auto-commits on a size threshold so a full-mainnet
    /// history table does not accumulate in memory.
    fn migrate_account_history<N: ProviderNodeTypes>(
        factory: &ProviderFactory<N>,
    ) -> eyre::Result<()> {
        info!(target: "reth::cli", "Migrating AccountsHistory index → RocksDB");
        let provider_rw = factory.database_provider_rw()?;

        let count = provider_rw.with_rocksdb_batch_auto_commit(|mut batch| {
            let mut cursor = provider_rw.tx_ref().cursor_read::<tables::AccountsHistory>()?;
            let mut count = 0u64;
            for entry in cursor.walk(None)? {
                let (key, value) = entry?;
                batch.put::<tables::AccountsHistory>(key, &value)?;
                count += 1;
            }
            Ok((count, Some(batch.into_inner())))
        })?;

        provider_rw.commit()?;

        info!(target: "reth::cli", count, "AccountsHistory shards migrated");
        Ok(())
    }

    /// Copy the MDBX `StoragesHistory` index into the v2 RocksDB store.
    ///
    /// Storage-slot counterpart of [`Self::migrate_account_history`]; see its docs for why the
    /// copy is verbatim (identical `StorageShardedKey` / `BlockNumberList` encoding and sharding)
    /// and why the batch auto-commits.
    fn migrate_storage_history<N: ProviderNodeTypes>(
        factory: &ProviderFactory<N>,
    ) -> eyre::Result<()> {
        info!(target: "reth::cli", "Migrating StoragesHistory index → RocksDB");
        let provider_rw = factory.database_provider_rw()?;

        let count = provider_rw.with_rocksdb_batch_auto_commit(|mut batch| {
            let mut cursor = provider_rw.tx_ref().cursor_read::<tables::StoragesHistory>()?;
            let mut count = 0u64;
            for entry in cursor.walk(None)? {
                let (key, value) = entry?;
                batch.put::<tables::StoragesHistory>(key, &value)?;
                count += 1;
            }
            Ok((count, Some(batch.into_inner())))
        })?;

        provider_rw.commit()?;

        info!(target: "reth::cli", count, "StoragesHistory shards migrated");
        Ok(())
    }

    /// Seed the `TransactionSenders` static-file segment up to `tip` with no entries.
    ///
    /// On a Celo-imported datadir all blocks below the migration block are header-only
    /// dummies (no tx bodies, no senders), and the migration block itself imports state
    /// without running transactions — so the segment is logically empty. The reth v2
    /// startup consistency check still requires the segment's `highest_static_file_block`
    /// to be >= the SenderRecovery stage checkpoint; we advance the segment boundary so
    /// that invariant holds. This mirrors the `writer.ensure_at_block(...)` call the
    /// upstream SenderRecovery stage uses when it processes a trailing range of empty
    /// blocks.
    ///
    /// Implementation note: `ensure_at_block(tip)` internally iterates one block at a
    /// time via `increment_block` and the writer accumulates per-block segment metadata
    /// in memory until the next commit. A single `ensure_at_block(31_056_500)` call from
    /// an empty segment grew the writer's in-memory state past 18 GB and deadlocked the
    /// final commit on a Celo Mainnet datadir. We avoid this by committing every
    /// `SEED_COMMIT_INTERVAL_BLOCKS` blocks, which bounds memory and gives the operator
    /// a usable progress signal.
    fn seed_transaction_senders<N: ProviderNodeTypes>(
        factory: &ProviderFactory<N>,
        tip: u64,
    ) -> eyre::Result<()> {
        /// Block interval between intermediate commits while seeding the segment.
        ///
        /// Picked to align with the natural 500 000-block segment-file boundary that the
        /// static-file writer already uses, so each commit flushes exactly one segment
        /// file's worth of in-memory metadata.
        const SEED_COMMIT_INTERVAL_BLOCKS: u64 = 500_000;

        info!(
            target: "reth::cli",
            tip,
            commit_interval_blocks = SEED_COMMIT_INTERVAL_BLOCKS,
            "Seeding TransactionSenders static-file segment to tip",
        );

        let sf_provider = factory.static_file_provider();
        let mut writer = sf_provider.latest_writer(StaticFileSegment::TransactionSenders)?;
        let mut next_log = SEED_COMMIT_INTERVAL_BLOCKS;
        let progress_start = std::time::Instant::now();

        // Advance the segment in commit-bounded chunks. Each iteration walks up to
        // `SEED_COMMIT_INTERVAL_BLOCKS` empty blocks and then commits, freeing the
        // accumulated metadata before continuing.
        let mut chunk_end = SEED_COMMIT_INTERVAL_BLOCKS.min(tip);
        loop {
            writer.ensure_at_block(chunk_end)?;
            writer.commit()?;

            if chunk_end >= next_log {
                let elapsed = progress_start.elapsed();
                let pct = (chunk_end as f64 / tip as f64) * 100.0;
                let eta_secs = if chunk_end > 0 {
                    (elapsed.as_secs_f64() * (tip - chunk_end) as f64 / chunk_end as f64) as u64
                } else {
                    0
                };
                info!(
                    target: "reth::cli",
                    block = chunk_end,
                    tip,
                    progress_pct = format!("{:.1}", pct),
                    elapsed_secs = elapsed.as_secs(),
                    eta_secs,
                    "Seeding TransactionSenders progress",
                );
                next_log = chunk_end + SEED_COMMIT_INTERVAL_BLOCKS;
            }

            if chunk_end == tip {
                break;
            }
            chunk_end = (chunk_end + SEED_COMMIT_INTERVAL_BLOCKS).min(tip);
        }

        let total_secs = progress_start.elapsed().as_secs();
        info!(
            target: "reth::cli",
            tip,
            total_secs,
            "TransactionSenders segment boundary advanced",
        );
        Ok(())
    }

    fn migrate_receipts<N: ProviderNodeTypes>(
        factory: &ProviderFactory<N>,
        tip: u64,
    ) -> eyre::Result<()>
    where
        N::Primitives: reth_primitives_traits::NodePrimitives<
                Receipt: reth_db_api::table::Value + reth_codecs::Compact,
            >,
    {
        let provider = factory.provider()?;
        if !provider.prune_modes_ref().receipts_log_filter.is_empty() {
            info!(
                target: "reth::cli",
                "Receipt log filter pruning is enabled, keeping receipts in MDBX",
            );
            return Ok(());
        }
        drop(provider);

        let sf_provider = factory.static_file_provider();
        let existing = sf_provider.get_highest_static_file_block(StaticFileSegment::Receipts);

        if existing.is_some_and(|b| b >= tip) {
            info!(target: "reth::cli", "Receipts already in static files, skipping");
            return Ok(());
        }

        info!(target: "reth::cli", "Migrating Receipts → static files");

        let provider = factory.provider()?.disable_long_read_transaction_safety();
        let prune_start = provider
            .get_prune_checkpoint(PruneSegment::Receipts)?
            .and_then(|cp| cp.block_number)
            .map_or(0, |b| b + 1);
        let first_block = prune_start.max(existing.map_or(0, |b| b + 1));

        if first_block > 0 {
            let mut writer = sf_provider.latest_writer(StaticFileSegment::Receipts)?;
            writer.ensure_at_block(first_block - 1)?;
            writer.commit()?;
        }

        let before = sf_provider
            .get_highest_static_file_tx(StaticFileSegment::Receipts)
            .map_or(0, |tx| tx + 1);

        let block_range = first_block..=tip;

        let segment = reth_static_file::segments::Receipts;
        reth_static_file::segments::Segment::copy_to_static_files(&segment, provider, block_range)?;

        sf_provider.commit()?;

        let after = sf_provider
            .get_highest_static_file_tx(StaticFileSegment::Receipts)
            .map_or(0, |tx| tx + 1);
        let count = after - before;
        info!(target: "reth::cli", count, "Receipts migrated");
        Ok(())
    }

    fn compact_mdbx(db: &mdbx::DatabaseEnv) -> eyre::Result<()> {
        let db_path = db.path();
        let compact_path = db_path.with_file_name("db_compact");

        reth_fs_util::create_dir_all(&compact_path)?;

        info!(target: "reth::cli", ?db_path, ?compact_path, "Compacting MDBX database");

        let compact_dest = compact_path.join("mdbx.dat");
        let dest_cstr = std::ffi::CString::new(
            compact_dest.to_str().ok_or_else(|| eyre::eyre!("compact path must be valid UTF-8"))?,
        )?;

        let flags = ffi::MDBX_CP_COMPACT | ffi::MDBX_CP_FORCE_DYNAMIC_SIZE;

        let rc = db.with_raw_env_ptr(|env_ptr| unsafe {
            ffi::mdbx_env_copy(env_ptr, dest_cstr.as_ptr(), flags)
        });

        if rc != 0 {
            eyre::bail!("mdbx_env_copy failed with error code {rc}: {}", unsafe {
                std::ffi::CStr::from_ptr(ffi::mdbx_strerror(rc)).to_string_lossy()
            });
        }

        info!(target: "reth::cli", "MDBX compaction complete");
        Ok(())
    }

    fn swap_compacted_db(
        db_path: &std::path::Path,
        compact_path: &std::path::Path,
    ) -> eyre::Result<()> {
        let backup_path = db_path.with_file_name("db_pre_compact");

        info!(target: "reth::cli", ?db_path, ?compact_path, "Swapping compacted database");

        std::fs::rename(db_path, &backup_path)?;

        if let Err(e) = std::fs::rename(compact_path, db_path) {
            let _ = std::fs::rename(&backup_path, db_path);
            return Err(e.into());
        }

        std::fs::remove_dir_all(&backup_path)?;

        info!(target: "reth::cli", "Database compaction swap complete");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;
    use reth_db_api::models::ShardedKey;
    use reth_provider::{RocksDBProviderFactory, test_utils::create_test_provider_factory};

    #[test]
    fn parse_minimal_cli() {
        // Defaults should parse with no required positional args.
        let cmd =
            CeloMigrateV2Command::parse_from(["celo-migrate-v2", "--datadir", "/tmp/celo-data"]);
        assert!(cmd.config.is_none());
    }

    /// `migrate_account_history` must copy each MDBX `AccountsHistory` shard verbatim into
    /// RocksDB. This is the #192 fix: without it the v2 datadir's history index is empty and
    /// every historical state read past the in-memory buffer misses.
    #[test]
    fn migrate_account_history_copies_mdbx_shard_to_rocksdb() {
        let factory = create_test_provider_factory();
        let address = Address::with_last_byte(0x42);
        let blocks = [10u64, 20, 30];
        let key = ShardedKey::new(address, u64::MAX);

        // Seed one MDBX AccountsHistory shard for `address`.
        {
            let provider_rw = factory.database_provider_rw().unwrap();
            provider_rw
                .tx_ref()
                .put::<tables::AccountsHistory>(
                    key.clone(),
                    tables::BlockNumberList::new(blocks).unwrap(),
                )
                .unwrap();
            provider_rw.commit().unwrap();
        }

        // Precondition: RocksDB has nothing for this address yet, so a passing postcondition
        // can only come from the migration actually writing the shard.
        assert!(
            factory.rocksdb_provider().account_history_shards(address).unwrap().is_empty(),
            "precondition: RocksDB account history index must start empty",
        );

        CeloMigrateV2Command::migrate_account_history(&factory).unwrap();

        // The shard now exists in RocksDB with the same key and block list as the MDBX source.
        let shards = factory.rocksdb_provider().account_history_shards(address).unwrap();
        assert_eq!(shards.len(), 1, "expected exactly one migrated shard");
        assert_eq!(shards[0].0, key);
        assert_eq!(shards[0].1.iter().collect::<Vec<_>>(), blocks.to_vec());
    }

    /// `migrate_storage_history` must copy each MDBX `StoragesHistory` shard verbatim into
    /// RocksDB, the storage-slot counterpart of the account-history copy above (#192).
    #[test]
    fn migrate_storage_history_copies_mdbx_shard_to_rocksdb() {
        use alloy_primitives::B256;
        use reth_db_api::models::storage_sharded_key::StorageShardedKey;

        let factory = create_test_provider_factory();
        let address = Address::with_last_byte(0x42);
        let slot = B256::with_last_byte(0x07);
        let blocks = [11u64, 22, 33];
        let key = StorageShardedKey::new(address, slot, u64::MAX);

        // Seed one MDBX StoragesHistory shard for `(address, slot)`.
        {
            let provider_rw = factory.database_provider_rw().unwrap();
            provider_rw
                .tx_ref()
                .put::<tables::StoragesHistory>(
                    key.clone(),
                    tables::BlockNumberList::new(blocks).unwrap(),
                )
                .unwrap();
            provider_rw.commit().unwrap();
        }

        // Precondition: RocksDB storage history is empty for this key.
        assert!(
            factory.rocksdb_provider().storage_history_shards(address, slot).unwrap().is_empty(),
            "precondition: RocksDB storage history index must start empty",
        );

        CeloMigrateV2Command::migrate_storage_history(&factory).unwrap();

        // The shard now exists in RocksDB with the same key and block list as the MDBX source.
        let shards = factory.rocksdb_provider().storage_history_shards(address, slot).unwrap();
        assert_eq!(shards.len(), 1, "expected exactly one migrated shard");
        assert_eq!(shards[0].0, key);
        assert_eq!(shards[0].1.iter().collect::<Vec<_>>(), blocks.to_vec());
    }

    /// A v2 datadir whose MDBX history tables are still populated is a partially-migrated datadir
    /// — a previous run crashed between the Phase 3 v2 flip and the Phase 4 history clear. The
    /// Phase 0 guard relies on this check to fail loudly rather than silently report "nothing to
    /// do" on an incomplete datadir that would later wedge with #192.
    #[test]
    fn v2_history_tables_cleared_detects_partial_migration() {
        let factory = create_test_provider_factory();

        // A fresh datadir has empty history tables → reads as cleared (a legitimate no-op).
        assert!(
            CeloMigrateV2Command::v2_history_tables_cleared(&factory).unwrap(),
            "empty AccountsHistory/StoragesHistory must read as cleared",
        );

        // Seed one AccountsHistory shard, mimicking a crash after the v2 flip but before Phase 4.
        {
            let provider_rw = factory.database_provider_rw().unwrap();
            provider_rw
                .tx_ref()
                .put::<tables::AccountsHistory>(
                    ShardedKey::new(Address::with_last_byte(0x42), u64::MAX),
                    tables::BlockNumberList::new([1u64]).unwrap(),
                )
                .unwrap();
            provider_rw.commit().unwrap();
        }

        // A populated history table must read as NOT cleared so the guard bails instead of
        // silently exiting.
        assert!(
            !CeloMigrateV2Command::v2_history_tables_cleared(&factory).unwrap(),
            "a populated AccountsHistory must read as not cleared",
        );
    }
}
