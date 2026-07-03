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

use eyre::Result;
use reth_chainspec::EthChainSpec;
use reth_cli_commands::common::EnvironmentArgs;
use reth_db::init_db;
use reth_db_api::{
    database::Database,
    tables,
    transaction::{DbTx, DbTxMut},
};
use reth_stages_types::{StageCheckpoint, StageId};
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
