//! Migration-boundary `TransactionLookup` checkpoint reconciliation for `celo-reth download`.
//!
//! # Why
//!
//! `celo-reth download` bootstraps a node from a chunked snapshot. When the selected components do
//! **not** include `rocksdb_indices` — every non-archive preset (`--full`, `--minimal`, and any
//! `--with-*` set without `--with-rocksdb`) — reth's download finalizer
//! (`reset_index_stage_checkpoints_tx`) resets three pipeline stage checkpoints to block `0`:
//! `TransactionLookup`, `IndexAccountHistory`, `IndexStorageHistory`. reth does this so the
//! pipeline rebuilds the RocksDB-backed index it did not download; on a genesis-contiguous chain
//! that rebuild-from-`0` is correct.
//!
//! It is **wrong for `TransactionLookup` on a migrated Celo chain**. Blocks
//! `1..CEL2_MIGRATION_BLOCK_NUMBER` are header-only dummy placeholders imported via
//! `import-celo-state` — they have no bodies and no `BlockBodyIndices`. `TransactionLookup` is
//! unpruned on a full node, so on node start it iterates from checkpoint `0`, reads block #1's body
//! indices, and dies with `ProviderError::BlockBodyIndicesNotFound(1)` ("block meta not found for
//! block #1").
//!
//! # What this does
//!
//! [`reconcile_migrated_index_checkpoints`] runs transparently right after a successful
//! `celo-reth download` (wired in `bin/celo_reth.rs`), so the normal reth workflow is unchanged.
//! For a migrated chain it advances the `TransactionLookup` checkpoint up to the migration block,
//! so the pipeline rebuilds it over real blocks (`migration_block + 1 ..= tip`) and skips the
//! header-only gap. The rebuilt index is complete: the pre-migration dummy blocks carry no
//! transactions.
//!
//! It touches **only `TransactionLookup`** and deliberately leaves reth's reset of
//! `IndexAccountHistory` / `IndexStorageHistory` at block `0`. Those two have a prune mode and
//! self-advance to `tip - distance`, writing their `AccountHistory` / `StorageHistory` prune
//! checkpoints — and it is exactly those prune checkpoints that let reth resolve an account with no
//! retained history shard via `PlainState` (`HistoryInfo::MaybeInPlainState`) instead of returning
//! an empty account (`NotYetWritten`). Advancing those stages here would risk skipping the
//! prune-checkpoint write and reintroduce the empty-historical-read failure that
//! [`crate::celo_migrate_v2`] fixes for the history-retaining path (#192). Pruned `--full` /
//! `--minimal` nodes never retain pre-migration history, so the un-downloaded pre-migration shards
//! are not needed. The publisher (`snapshot-manifest`) reconciles the full migrated-chain stage set
//! for the `--archive` path, where the RocksDB index *is* shipped.
//!
//! # Workflow
//!
//! The normal reth workflow is unchanged — no extra step:
//!
//! ```text
//! celo-reth download --datadir=/celo --chain=celo --full   # (no rocksdb_indices)
//! celo-reth node --datadir=/celo --chain=celo --full ...
//! ```
//!
//! It is a safe no-op when the index was downloaded (`--archive`/`--with-rocksdb`, checkpoint
//! already at the tip) and on non-migrated chains (celo-sepolia is a fresh L2).

use clap::ArgMatches;
use eyre::{Result, WrapErr};
use reth_chainspec::EthChainSpec;
use reth_cli_commands::common::EnvironmentArgs;
use reth_config::Config;
use reth_db::init_db;
use reth_db_api::{
    database::Database,
    tables,
    transaction::{DbTx, DbTxMut},
};
use reth_prune_types::PruneModes;
use reth_stages_types::{StageCheckpoint, StageId};
use std::path::Path;
use tracing::{info, warn};

use crate::{
    chainspec::CeloChainSpecParser,
    state_import::{CEL2_MIGRATION_BLOCK_NUMBER, CELO_MAINNET_CHAIN_ID},
};

/// Stage checkpoints advanced to the migration block after a `rocksdb_indices`-less download.
///
/// Only `TransactionLookup`: it is unpruned on a full node, so it has no prune-mode self-advance
/// and iterates from reth's reset value of `0` straight into the header-only pre-migration gap
/// (`BlockBodyIndicesNotFound(1)`). reth also resets `IndexAccountHistory` / `IndexStorageHistory`,
/// but those are deliberately **not** listed here — see the module docs: leaving them at `0` lets
/// their prune-mode self-advance write the prune-checkpoint boundary that keeps historical reads
/// falling back to `PlainState`.
const STAGES_TO_ADVANCE: [StageId; 1] = [StageId::TransactionLookup];

/// The migration-block height for a migrated Celo chain, or `None` for a genesis-contiguous chain
/// that needs no reconciliation.
///
/// Keyed on the chain id, **not** `chain.genesis().number`: on Celo mainnet the chain-spec genesis
/// is the op-geth block-`0` L1 genesis (`genesis().number == 0`), while the CEL2 migration — the
/// first block with real bodies — is at [`CEL2_MIGRATION_BLOCK_NUMBER`]. celo-sepolia is a fresh L2
/// (genesis-contiguous), so it needs no reconciliation.
fn migration_block_for(chain_id: u64) -> Option<u64> {
    (chain_id == CELO_MAINNET_CHAIN_ID).then_some(CEL2_MIGRATION_BLOCK_NUMBER)
}

/// What to run after a successful `celo-reth download`, decided from its parsed arguments by
/// [`post_download_action`].
#[derive(Debug, PartialEq, Eq)]
pub enum PostDownloadAction {
    /// Run [`reconcile_migrated_index_checkpoints`].
    Reconcile,
    /// Do nothing: the invocation downloaded nothing (a read-only `--list`), so the datadir must
    /// not be opened or created.
    Skip,
}

/// Decide the post-`download` action from the download's parsed arguments.
///
/// `--list` / `--list-snapshots` lists snapshots and downloads nothing, so return
/// [`PostDownloadAction::Skip`]: even the read-only-looking reconciliation opens the datadir with
/// `init_db`, which would give a listing command write side effects (and fail on an unwritable
/// datadir). Every other invocation returns [`PostDownloadAction::Reconcile`]; whether the
/// resulting datadir is actually bootable is decided authoritatively from the resolved reth.toml
/// prune shape inside [`reconcile_migrated_index_checkpoints`], not from the raw download flags
/// here.
pub fn post_download_action(matches: &ArgMatches) -> PostDownloadAction {
    if matches.get_flag("list") { PostDownloadAction::Skip } else { PostDownloadAction::Reconcile }
}

/// Reconcile the `TransactionLookup` checkpoint a `rocksdb_indices`-less `celo-reth download` reset
/// to block `0`, advancing it to the migration block for a migrated Celo chain so the node rebuilds
/// it over real blocks instead of crashing on the header-only pre-migration gap.
///
/// `env` is the same [`EnvironmentArgs`] the download ran with. Synchronous (MDBX only), so it runs
/// directly after the async download completes.
pub fn reconcile_migrated_index_checkpoints(
    env: EnvironmentArgs<CeloChainSpecParser>,
) -> Result<()> {
    let chain_id = env.chain.chain_id();
    let Some(migration_block) = migration_block_for(chain_id) else {
        info!(
            target: "reth::cli",
            chain_id,
            "Not a migrated Celo chain; no TransactionLookup checkpoint reconciliation needed",
        );
        return Ok(());
    };

    // Resolve the datadir exactly as the download did (chain-aware), then open the MDBX directly.
    // `init_db` is a raw open (no `EnvironmentArgs::init` consistency check), so it cannot trip the
    // destructive "unwind to 0" the reset checkpoints could otherwise provoke.
    let data_dir = env.datadir.clone().resolve_datadir(env.chain.chain());
    let db_path = data_dir.db();
    info!(
        target: "reth::cli",
        ?db_path,
        migration_block,
        "Reconciling downloaded TransactionLookup checkpoint for migrated chain",
    );

    let db = init_db(db_path, env.db.database_args())?;
    let tx = db.tx_mut()?;

    // Authoritative bootability guard: refuse a download whose resolved reth.toml prune shape
    // leaves a reset history index stage unable to self-advance past the header-only
    // pre-migration gap.
    if let Some(prune) = read_prune_segments(&data_dir.config())? {
        assert_bootable_migrated_shape(&tx, &prune, migration_block)?;
    }

    let reconciled = advance_stage_checkpoints_tx(&tx, migration_block)?;
    tx.commit()?;

    info!(
        target: "reth::cli",
        migration_block,
        reconciled,
        "TransactionLookup checkpoint reconciliation complete",
    );
    Ok(())
}

/// Reads the prune segments reth resolved and wrote to `<datadir>/reth.toml` during the download.
///
/// Returns `None` when reth.toml is absent so the caller skips the bootability guard rather than
/// guess a shape — a missing config is not evidence that history is unpruned, and reth's own
/// download always writes one, so this only happens on an unexpected/partial datadir.
fn read_prune_segments(config_path: &Path) -> Result<Option<PruneModes>> {
    if !config_path.exists() {
        warn!(
            target: "reth::cli",
            ?config_path,
            "reth.toml not found after download; skipping prune-shape bootability check",
        );
        return Ok(None);
    }
    let config = Config::from_path(config_path)
        .wrap_err("failed to read reth.toml for the prune-shape bootability check")?;
    Ok(Some(config.prune.segments))
}

/// Refuse a downloaded datadir whose resolved prune shape cannot boot on migrated Celo mainnet.
///
/// A `rocksdb_indices`-less download resets `IndexAccountHistory` / `IndexStorageHistory` to block
/// `0`. Those stages can only skip the header-only pre-migration gap by self-advancing to their
/// prune boundary, which requires history pruning to be configured. An archive-shaped selection
/// (`--archive`, the TUI archive preset, or `--with-state-history` without `--with-rocksdb`) leaves
/// history unpruned in reth.toml, so a reset history stage would rebuild from block `1` with no
/// prune boundary to fall back to `PlainState` — and the pre-migration history was never
/// downloaded. That datadir cannot boot, so it is refused here from the resolved reth.toml shape
/// rather than the raw download flags (which also miss the TUI-preset and `--with-state-history`
/// paths).
///
/// Only fires for a completed post-migration snapshot (`Finish` at or above the migration block). A
/// history stage already at or above the migration block had its RocksDB index downloaded and is
/// fine (archive-with-rocksdb), so it is left alone.
fn assert_bootable_migrated_shape<Tx>(
    tx: &Tx,
    prune: &PruneModes,
    migration_block: u64,
) -> Result<()>
where
    Tx: DbTx,
{
    let tip =
        tx.get::<tables::StageCheckpoints>(StageId::Finish.to_string())?.map(|c| c.block_number);
    if !matches!(tip, Some(tip) if tip >= migration_block) {
        return Ok(());
    }

    for (stage, prune_mode, segment) in [
        (StageId::IndexAccountHistory, prune.account_history, "account_history"),
        (StageId::IndexStorageHistory, prune.storage_history, "storage_history"),
    ] {
        let current = tx
            .get::<tables::StageCheckpoints>(stage.to_string())?
            .map(|c| c.block_number)
            .unwrap_or(0);
        if prune_mode.is_none() && current < migration_block {
            eyre::bail!(
                "downloaded datadir is not bootable on migrated Celo mainnet: {stage} was reset to \
                 block {current} (RocksDB history index not downloaded) but reth.toml leaves \
                 `{segment}` unpruned (archive shape), so the stage cannot self-advance past the \
                 header-only pre-migration gap at block {migration_block} and the pre-migration \
                 history was never downloaded. Re-run with RocksDB indices (`--archive` without \
                 `--without-rocksdb`, or `--with-rocksdb`) for an archive node, or a \
                 `--full`/`--minimal` preset for a pruned node."
            );
        }
    }
    Ok(())
}

/// Advance any [`STAGES_TO_ADVANCE`] checkpoint that sits below `migration_block` up to it, inside
/// an existing write transaction. Returns the number of stages advanced.
///
/// Guards against acting on anything but a completed post-migration snapshot download: if the
/// `Finish` checkpoint is missing or below the migration block, the datadir is not a valid
/// migrated-chain snapshot and is left untouched. A stage already at or beyond the migration block
/// (e.g. the index *was* downloaded, so the checkpoint is at the tip) is also left untouched,
/// making this idempotent.
fn advance_stage_checkpoints_tx<Tx>(tx: &Tx, migration_block: u64) -> Result<usize>
where
    Tx: DbTx + DbTxMut,
{
    let tip =
        tx.get::<tables::StageCheckpoints>(StageId::Finish.to_string())?.map(|c| c.block_number);
    match tip {
        None => {
            warn!(
                target: "reth::cli",
                "No `Finish` stage checkpoint; datadir is not a completed snapshot download — skipping reconciliation",
            );
            return Ok(0);
        }
        Some(tip) if tip < migration_block => {
            warn!(
                target: "reth::cli",
                tip,
                migration_block,
                "Datadir tip is below the migration block; not a valid post-migration snapshot — skipping reconciliation",
            );
            return Ok(0);
        }
        Some(_) => {}
    }

    let mut advanced = 0usize;
    for stage in STAGES_TO_ADVANCE {
        let key = stage.to_string();
        let current =
            tx.get::<tables::StageCheckpoints>(key.clone())?.map(|c| c.block_number).unwrap_or(0);
        if current < migration_block {
            tx.put::<tables::StageCheckpoints>(key.clone(), StageCheckpoint::new(migration_block))?;
            // Clear any stale per-stage progress so the stage restarts cleanly from the migration
            // block. reth's own reset clears `StageCheckpointProgresses` for the same reason; on a
            // freshly reset datadir this is already empty, so the delete is a harmless no-op.
            tx.delete::<tables::StageCheckpointProgresses>(key, None)?;
            info!(
                target: "reth::cli",
                stage = %stage,
                from = current,
                to = migration_block,
                "Advanced stage checkpoint to migration block so the pipeline rebuilds over real blocks",
            );
            advanced += 1;
        }
    }
    Ok(advanced)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_cli::chainspec::ChainSpecParser;
    use reth_provider::{DatabaseProviderFactory, test_utils::create_test_provider_factory};
    use reth_prune_types::PruneMode;

    /// Only `TransactionLookup` is advanced. The two history stages reth also resets must be
    /// absent: advancing them would skip the prune-checkpoint write that keeps historical reads
    /// falling back to `PlainState` (module docs / #192). `SenderRecovery` is not reset by the
    /// download at all.
    #[test]
    fn advances_only_transaction_lookup() {
        assert_eq!(STAGES_TO_ADVANCE, [StageId::TransactionLookup]);
        for excluded in
            [StageId::IndexAccountHistory, StageId::IndexStorageHistory, StageId::SenderRecovery]
        {
            assert!(!STAGES_TO_ADVANCE.contains(&excluded), "{excluded} must not be advanced");
        }
    }

    /// Regression for the migration-height derivation: it must come from the chain id +
    /// `CEL2_MIGRATION_BLOCK_NUMBER`, never `chain.genesis().number` (which is the op-geth block-0
    /// genesis on Celo mainnet, so using it would make the reconciliation a silent no-op).
    #[test]
    fn migration_block_derives_from_chain_id_not_genesis() {
        let mainnet = CeloChainSpecParser::parse("celo").unwrap();
        assert_eq!(mainnet.chain_id(), CELO_MAINNET_CHAIN_ID);
        assert_eq!(
            mainnet.genesis().number.unwrap_or_default(),
            0,
            "Celo mainnet genesis is op-geth block 0, NOT the migration block",
        );
        assert_eq!(migration_block_for(mainnet.chain_id()), Some(CEL2_MIGRATION_BLOCK_NUMBER));

        // celo-sepolia is a fresh L2 (genesis-contiguous) and needs no reconciliation.
        let sepolia = CeloChainSpecParser::parse("celo-sepolia").unwrap();
        assert_eq!(migration_block_for(sepolia.chain_id()), None);
    }

    /// Parse real `celo-reth download` arguments so a wrong flag id would panic in `get_flag`.
    fn download_matches(args: &[&str]) -> clap::ArgMatches {
        use clap::CommandFactory;
        reth_cli_commands::download::DownloadCommand::<CeloChainSpecParser>::command()
            .no_binary_name(true)
            .try_get_matches_from(args)
            .unwrap()
    }

    /// `--list` downloads nothing, so the post-download step must skip (no datadir side effects).
    #[test]
    fn post_download_action_skips_list() {
        let matches = download_matches(&["--chain", "celo", "--list"]);
        assert_eq!(post_download_action(&matches), PostDownloadAction::Skip);
    }

    /// Any non-`--list` download reconciles; bootability is decided later from the reth.toml prune
    /// shape (`assert_bootable_migrated_shape`), not from these flags.
    #[test]
    fn post_download_action_reconciles_non_list() {
        for args in [
            ["--chain", "celo", "--full"].as_slice(),
            ["--chain", "celo", "--minimal"].as_slice(),
            ["--chain", "celo", "--archive", "--without-rocksdb"].as_slice(),
        ] {
            let matches = download_matches(args);
            assert_eq!(post_download_action(&matches), PostDownloadAction::Reconcile);
        }
    }

    /// Writes `Finish` at `tip` plus the two history-index stage checkpoints, then runs the
    /// bootability guard with the supplied reth.toml prune modes.
    fn check_shape(
        tip: u64,
        index_account: u64,
        index_storage: u64,
        account_history: Option<PruneMode>,
        storage_history: Option<PruneMode>,
    ) -> Result<()> {
        let factory = create_test_provider_factory();
        let provider_rw = factory.database_provider_rw().unwrap();
        let tx = provider_rw.tx_ref();
        tx.put::<tables::StageCheckpoints>(StageId::Finish.to_string(), StageCheckpoint::new(tip))
            .unwrap();
        tx.put::<tables::StageCheckpoints>(
            StageId::IndexAccountHistory.to_string(),
            StageCheckpoint::new(index_account),
        )
        .unwrap();
        tx.put::<tables::StageCheckpoints>(
            StageId::IndexStorageHistory.to_string(),
            StageCheckpoint::new(index_storage),
        )
        .unwrap();
        let prune = PruneModes { account_history, storage_history, ..Default::default() };
        assert_bootable_migrated_shape(tx, &prune, CEL2_MIGRATION_BLOCK_NUMBER)
    }

    /// Archive-shaped rocksdb-less download (history unpruned in reth.toml, history index reset to
    /// 0): not bootable on migrated mainnet — must be rejected.
    #[test]
    fn rejects_unpruned_history_index_reset_to_zero() {
        let tip = CEL2_MIGRATION_BLOCK_NUMBER + 1_000;
        assert!(check_shape(tip, 0, 0, None, None).is_err());
    }

    /// Pruned rocksdb-less download (`--full`/`--minimal`: history pruned, history index reset to
    /// 0): the reset stages self-advance via their prune boundary — bootable, must be accepted.
    #[test]
    fn accepts_pruned_history_index_reset_to_zero() {
        let tip = CEL2_MIGRATION_BLOCK_NUMBER + 1_000;
        let distance = Some(PruneMode::Distance(10_064));
        assert!(check_shape(tip, 0, 0, distance, distance).is_ok());
    }

    /// Archive download WITH rocksdb (history unpruned but the index was downloaded, so the history
    /// stages sit at the tip): bootable, must be accepted.
    #[test]
    fn accepts_unpruned_history_index_at_tip() {
        let tip = CEL2_MIGRATION_BLOCK_NUMBER + 1_000;
        assert!(check_shape(tip, tip, tip, None, None).is_ok());
    }

    /// One unpruned history stage reset to 0 is enough to reject, even if the other is pruned.
    #[test]
    fn rejects_when_only_storage_history_unpruned_and_reset() {
        let tip = CEL2_MIGRATION_BLOCK_NUMBER + 1_000;
        assert!(check_shape(tip, 0, 0, Some(PruneMode::Distance(10_064)), None).is_err());
    }

    /// Below the migration block the datadir is not a completed post-migration snapshot; the guard
    /// must not fire (the advance step already refuses to touch it).
    #[test]
    fn shape_check_skips_when_tip_below_migration() {
        let tip = CEL2_MIGRATION_BLOCK_NUMBER - 1;
        assert!(check_shape(tip, 0, 0, None, None).is_ok());
    }

    /// A `--full`-shaped datadir (index stages reset to 0, everything else at the tip): only
    /// `TransactionLookup` is advanced to the migration block; the two history stages reth reset
    /// are LEFT at 0 (so their prune self-advance runs), and unrelated stages are untouched.
    #[test]
    fn advances_transaction_lookup_leaving_history_stages_reset() {
        let migration = CEL2_MIGRATION_BLOCK_NUMBER;
        let tip = migration + 40_000_000;
        let factory = create_test_provider_factory();
        let provider_rw = factory.database_provider_rw().unwrap();
        let tx = provider_rw.tx_ref();

        tx.put::<tables::StageCheckpoints>(StageId::Finish.to_string(), StageCheckpoint::new(tip))
            .unwrap();
        tx.put::<tables::StageCheckpoints>(
            StageId::SenderRecovery.to_string(),
            StageCheckpoint::new(tip),
        )
        .unwrap();
        for reset in
            [StageId::TransactionLookup, StageId::IndexAccountHistory, StageId::IndexStorageHistory]
        {
            tx.put::<tables::StageCheckpoints>(reset.to_string(), StageCheckpoint::new(0)).unwrap();
        }

        let advanced = advance_stage_checkpoints_tx(tx, migration).unwrap();
        assert_eq!(advanced, 1);

        let tx_lookup = tx
            .get::<tables::StageCheckpoints>(StageId::TransactionLookup.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(
            tx_lookup.block_number, migration,
            "TransactionLookup must reach the migration block",
        );

        // The history stages must be LEFT at reth's reset value of 0 so their prune-mode
        // self-advance writes the prune boundary (module docs / #192).
        for history in [StageId::IndexAccountHistory, StageId::IndexStorageHistory] {
            let cp = tx.get::<tables::StageCheckpoints>(history.to_string()).unwrap().unwrap();
            assert_eq!(cp.block_number, 0, "{history} must be left at reth's reset-to-0");
        }
        let sender = tx
            .get::<tables::StageCheckpoints>(StageId::SenderRecovery.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(sender.block_number, tip, "SenderRecovery must not be touched");
    }

    /// When the index was downloaded (`--archive`/`--with-rocksdb`), `TransactionLookup` is already
    /// at the tip, so the reconciliation is a no-op — idempotency/safety on healthy datadirs.
    #[test]
    fn noop_when_transaction_lookup_already_at_tip() {
        let migration = CEL2_MIGRATION_BLOCK_NUMBER;
        let tip = migration + 1_000;
        let factory = create_test_provider_factory();
        let provider_rw = factory.database_provider_rw().unwrap();
        let tx = provider_rw.tx_ref();

        tx.put::<tables::StageCheckpoints>(StageId::Finish.to_string(), StageCheckpoint::new(tip))
            .unwrap();
        tx.put::<tables::StageCheckpoints>(
            StageId::TransactionLookup.to_string(),
            StageCheckpoint::new(tip),
        )
        .unwrap();

        let advanced = advance_stage_checkpoints_tx(tx, migration).unwrap();
        assert_eq!(advanced, 0);
        let cp = tx
            .get::<tables::StageCheckpoints>(StageId::TransactionLookup.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(cp.block_number, tip, "TransactionLookup must be left untouched");
    }

    /// A datadir whose tip is below the migration block is not a valid post-migration snapshot; the
    /// reconciliation must refuse to touch it rather than corrupt an unrelated datadir.
    #[test]
    fn skips_when_tip_below_migration_block() {
        let migration = CEL2_MIGRATION_BLOCK_NUMBER;
        let factory = create_test_provider_factory();
        let provider_rw = factory.database_provider_rw().unwrap();
        let tx = provider_rw.tx_ref();

        tx.put::<tables::StageCheckpoints>(
            StageId::Finish.to_string(),
            StageCheckpoint::new(migration - 1),
        )
        .unwrap();
        tx.put::<tables::StageCheckpoints>(
            StageId::TransactionLookup.to_string(),
            StageCheckpoint::new(0),
        )
        .unwrap();

        let advanced = advance_stage_checkpoints_tx(tx, migration).unwrap();
        assert_eq!(advanced, 0);
        let cp = tx
            .get::<tables::StageCheckpoints>(StageId::TransactionLookup.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(
            cp.block_number, 0,
            "TransactionLookup must be left untouched when tip < migration",
        );
    }

    /// Without a `Finish` checkpoint the datadir is not a completed download; the reconciliation
    /// no-ops.
    #[test]
    fn skips_when_no_finish_checkpoint() {
        let migration = CEL2_MIGRATION_BLOCK_NUMBER;
        let factory = create_test_provider_factory();
        let provider_rw = factory.database_provider_rw().unwrap();
        let tx = provider_rw.tx_ref();

        tx.put::<tables::StageCheckpoints>(
            StageId::TransactionLookup.to_string(),
            StageCheckpoint::new(0),
        )
        .unwrap();

        let advanced = advance_stage_checkpoints_tx(tx, migration).unwrap();
        assert_eq!(advanced, 0);
    }
}
