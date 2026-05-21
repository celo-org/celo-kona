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
use reth_cli_commands::common::{AccessRights, CliNodeTypes, Environment, EnvironmentArgs};
use reth_db::{
    DatabaseEnv,
    mdbx::{self, ffi},
    models::StorageBeforeTx,
};
use reth_db_api::{cursor::DbCursorRO, database::Database, tables, transaction::DbTx};
use reth_node_builder::NodeTypesWithDBAdapter;
use reth_node_core::args::{DatabaseArgs, DatadirArgs, StaticFilesArgs};
use reth_optimism_node::OpNode;
use reth_provider::{
    DBProvider, DatabaseProviderFactory, MetadataProvider, MetadataWriter, ProviderFactory,
    PruneCheckpointReader, StaticFileProviderFactory, StaticFileWriter, StorageSettings,
    providers::ProviderNodeTypes,
};
use reth_prune_types::PruneSegment;
use reth_stages_types::StageId;
use reth_static_file_types::StaticFileSegment;
use reth_storage_api::StageCheckpointReader;
use std::path::PathBuf;
use tracing::info;

use crate::{chainspec::CeloChainSpecParser, state_import::CELO_MAINNET_CHAIN_ID};

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

        let Environment { provider_factory, .. } =
            env_args.init::<OpNode>(AccessRights::RW, runtime)?;

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
            info!(target: "reth::cli", "Storage is already v2, nothing to do");
            return Ok(());
        }

        let tip = provider
            .get_stage_checkpoint(StageId::Execution)?
            .map(|c| c.block_number)
            .unwrap_or(0);

        info!(target: "reth::cli", tip, "Chain tip block number");

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

        // === Phase 4 (skipped for Celo): Clear recomputable tables + reset stage checkpoints ===
        //
        // Upstream clears AccountChangeSets/StorageChangeSets (now in static files),
        // TransactionSenders/TransactionHashNumbers/AccountsHistory/StoragesHistory/
        // PlainAccountState/PlainStorageState/AccountsTrie/StoragesTrie, and resets
        // six stage checkpoints to 0 so the pipeline rebuilds them. For a Celo-imported
        // datadir those tables are already in the correct post-migration state and
        // the stages are at the migration block; rebuilding from block 0 fails because
        // blocks 1..migration_block-1 are header-only dummies (no tx bodies for
        // SenderRecovery to process). Leaving them as-is is correct.
        info!(
            target: "reth::cli",
            "Skipping recomputable-table clear and stage-checkpoint reset (Celo dummy-block range \
             cannot be re-walked by the pipeline)",
        );

        // === Phase 5: Compact MDBX ===
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

    // The following four helpers are direct copies of the upstream private associated
    // functions on `reth_cli_commands::db::migrate_v2::Command`. They are duplicated
    // here because upstream marks them as `fn` (private), and we need to call them
    // from the celo-reth crate. Keep them byte-for-byte in sync with upstream when
    // bumping the reth pin.

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

        let mut count = 0u64;
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

        let mut count = 0u64;
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
        }

        writer.commit()?;

        info!(target: "reth::cli", count, "StorageChangeSets migrated");
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
        reth_static_file::segments::Segment::copy_to_static_files(
            &segment,
            provider,
            block_range,
        )?;

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
            compact_dest
                .to_str()
                .ok_or_else(|| eyre::eyre!("compact path must be valid UTF-8"))?,
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

    #[test]
    fn parse_minimal_cli() {
        // Defaults should parse with no required positional args.
        let cmd =
            CeloMigrateV2Command::parse_from(["celo-migrate-v2", "--datadir", "/tmp/celo-data"]);
        assert!(cmd.config.is_none());
    }
}
