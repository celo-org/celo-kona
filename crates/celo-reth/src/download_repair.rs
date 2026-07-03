//! Post-download index-stage checkpoint repair for migrated Celo chains
//! (`celo-reth repair-download-checkpoints`).
//!
//! # Why
//!
//! `celo-reth download` bootstraps a node from a chunked snapshot. When the selected components
//! do **not** include `rocksdb_indices` — which is the case for every non-archive preset
//! (`--full`, `--minimal`, and any `--with-*` set without `--with-rocksdb`) — reth's download
//! finalizer (`reset_index_stage_checkpoints_tx`) resets three pipeline stage checkpoints to block
//! `0`: `TransactionLookup`, `IndexAccountHistory`, `IndexStorageHistory`. The snapshot MDBX ships
//! these at the tip, but since the RocksDB index they produce is not distributed, reth resets them
//! so the pipeline rebuilds the index from scratch; otherwise it would see "already done" and skip
//! rebuilding an index that isn't there.
//!
//! That reset-to-`0` is correct for a genesis-contiguous chain, but **wrong for a migrated Celo
//! chain**. Celo Mainnet's blocks `1..CEL2_MIGRATION_BLOCK_NUMBER` are header-only dummy
//! placeholders imported via `import-celo-state` — they have no bodies and no `BlockBodyIndices`.
//! On node start the pipeline runs `TransactionLookup` from block `1`, reads block #1's body
//! indices, and dies with `ProviderError::BlockBodyIndicesNotFound(1)` ("block meta not found for
//! block #1"), crash-looping the node.
//!
//! # Fix
//!
//! For a migrated chain, the correct rebuild start is the **migration block** (the L2 "genesis",
//! `chain.genesis().number`), not block `0`. This command advances any of the three reset stages
//! that sit below the migration block up to it, so the pipeline rebuilds the index over real
//! blocks (`migration_block + 1 ..= tip`) and skips the header-only gap entirely. The rebuilt
//! index is complete: the pre-migration dummy blocks carry no transactions, so there is nothing to
//! index there.
//!
//! This deliberately targets **only** the three stages reth resets, and sets them to the migration
//! block rather than the tip: unlike the snapshot *publisher*
//! (`snapshot-manifest`, which advances a lagging checkpoint to the tip because the index data is
//! complete in the source datadir), here the RocksDB index was never downloaded, so the pipeline
//! must actually rebuild it. Setting the checkpoint to the tip would skip that rebuild and leave
//! the node without a transaction-lookup index. This mirrors the migrated-chain reasoning in
//! [`crate::celo_migrate_v2`], which likewise refuses to reset these stages to `0`.
//!
//! # Usage
//!
//! Run once after `celo-reth download`, before starting the node:
//!
//! ```text
//! celo-reth download --datadir=/celo --chain=celo --full   # (no rocksdb_indices)
//! celo-reth repair-download-checkpoints --datadir=/celo --chain=celo
//! celo-reth node --datadir=/celo --chain=celo --full ...
//! ```
//!
//! It is a safe no-op when the index was downloaded (`--archive`/`--with-rocksdb`, checkpoints
//! already at the tip) and on non-migrated chains (`chain.genesis().number == 0`).

use clap::Parser;
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

use crate::chainspec::CeloChainSpecParser;

/// Pipeline stages whose output is stored in RocksDB and is therefore never distributed in a
/// snapshot's static files/MDBX. reth's download finalizer resets exactly these to block `0` when
/// `rocksdb_indices` is not part of the selection (its `INDEX_STAGE_IDS`). On a migrated Celo chain
/// that reset must instead target the migration block — see the module docs.
///
/// This is intentionally the three-stage download-reset set, **not** the six-stage
/// `MIGRATED_CHAIN_STAGES` set the snapshot publisher reconciles: the other three (`SenderRecovery`,
/// `MerkleExecute`, `MerkleUnwind`) are left at the tip by the download and must not be disturbed.
const RESET_INDEX_STAGES: [StageId; 3] =
    [StageId::TransactionLookup, StageId::IndexAccountHistory, StageId::IndexStorageHistory];

/// Repair the index-stage checkpoints of a datadir produced by a `rocksdb_indices`-less
/// `celo-reth download` so a migrated Celo chain can rebuild them without hitting the header-only
/// pre-migration gap.
#[derive(Debug, Parser)]
pub struct CeloRepairDownloadCommand {
    /// Datadir, chain, and database arguments (same flags as `celo-reth download`).
    #[command(flatten)]
    env: EnvironmentArgs<CeloChainSpecParser>,
}

impl CeloRepairDownloadCommand {
    /// Open the downloaded datadir's MDBX read-write and advance the reset index-stage checkpoints
    /// to the migration block for migrated chains. Synchronous: it only touches MDBX, so no async
    /// runtime is required.
    pub fn execute(self) -> Result<()> {
        // The migration block is the L2 datadir's genesis block number: `import-celo-state` writes
        // the migration header there and seeds every stage checkpoint to it (see
        // `state_import.rs`). A normal genesis-contiguous chain has genesis `0` and needs no repair.
        let migration_block = self.env.chain.genesis().number.unwrap_or_default();
        if migration_block == 0 {
            info!(
                target: "reth::cli",
                chain = %self.env.chain.chain(),
                "Chain genesis is block 0 (not a migrated chain); no index-stage checkpoint repair needed",
            );
            return Ok(());
        }

        // Resolve the datadir exactly as reth's download did (chain-aware), then open the MDBX
        // directly. `init_db` is a raw open (no `EnvironmentArgs::init` consistency check), so it
        // cannot trip the destructive "unwind to 0" the reset checkpoints could otherwise provoke.
        let data_dir = self.env.datadir.clone().resolve_datadir(self.env.chain.chain());
        let db_path = data_dir.db();
        info!(
            target: "reth::cli",
            ?db_path,
            migration_block,
            "Repairing downloaded datadir index-stage checkpoints",
        );

        let db = init_db(db_path, self.env.db.database_args())?;
        let tx = db.tx_mut()?;
        let repaired = repair_index_checkpoints_tx(&tx, migration_block)?;
        tx.commit()?;

        info!(
            target: "reth::cli",
            migration_block,
            repaired,
            "Index-stage checkpoint repair complete",
        );
        Ok(())
    }
}

/// Advance any [`RESET_INDEX_STAGES`] checkpoint that sits below `migration_block` up to it, inside
/// an existing write transaction. Returns the number of stages advanced.
///
/// Guards against acting on anything but a completed post-migration snapshot download: if the
/// `Finish` checkpoint is missing or below the migration block, the datadir is not a valid
/// migrated-chain snapshot and is left untouched. Stages already at or beyond the migration block
/// (e.g. the index *was* downloaded, so the checkpoints are at the tip) are also left untouched,
/// making this idempotent.
fn repair_index_checkpoints_tx<Tx>(tx: &Tx, migration_block: u64) -> Result<usize>
where
    Tx: DbTx + DbTxMut,
{
    let tip = tx.get::<tables::StageCheckpoints>(StageId::Finish.to_string())?.map(|c| c.block_number);
    match tip {
        None => {
            warn!(
                target: "reth::cli",
                "No `Finish` stage checkpoint; datadir is not a completed snapshot download — skipping repair",
            );
            return Ok(0);
        }
        Some(tip) if tip < migration_block => {
            warn!(
                target: "reth::cli",
                tip,
                migration_block,
                "Datadir tip is below the migration block; not a valid post-migration snapshot — skipping repair",
            );
            return Ok(0);
        }
        Some(_) => {}
    }

    let mut repaired = 0usize;
    for stage in RESET_INDEX_STAGES {
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
                "Reset index-stage checkpoint to migration block so the pipeline rebuilds over real blocks",
            );
            repaired += 1;
        }
    }
    Ok(repaired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_provider::{DatabaseProviderFactory, test_utils::create_test_provider_factory};

    /// The download-reset set must be exactly the three RocksDB-backed index stages reth resets
    /// (`INDEX_STAGE_IDS`), and must **not** include `SenderRecovery`, which the download leaves at
    /// the tip.
    #[test]
    fn reset_index_stages_are_the_three_rocksdb_index_stages() {
        assert_eq!(
            RESET_INDEX_STAGES,
            [StageId::TransactionLookup, StageId::IndexAccountHistory, StageId::IndexStorageHistory],
        );
        assert!(!RESET_INDEX_STAGES.contains(&StageId::SenderRecovery));
    }

    /// A `--full`-shaped datadir (index stages reset to 0, everything else at the tip) must have
    /// exactly the three index stages advanced to the migration block, and unrelated stages left
    /// untouched.
    #[test]
    fn advances_reset_index_stages_to_migration_block() {
        let migration = 31_056_500u64;
        let tip = migration + 40_000_000;
        let factory = create_test_provider_factory();
        let provider_rw = factory.database_provider_rw().unwrap();
        let tx = provider_rw.tx_ref();

        // Snapshot MDBX shape after a rocksdb-less download: Finish + SenderRecovery at the tip,
        // the three index stages reset to 0.
        tx.put::<tables::StageCheckpoints>(StageId::Finish.to_string(), StageCheckpoint::new(tip))
            .unwrap();
        tx.put::<tables::StageCheckpoints>(
            StageId::SenderRecovery.to_string(),
            StageCheckpoint::new(tip),
        )
        .unwrap();
        for stage in RESET_INDEX_STAGES {
            tx.put::<tables::StageCheckpoints>(stage.to_string(), StageCheckpoint::new(0)).unwrap();
        }

        let repaired = repair_index_checkpoints_tx(tx, migration).unwrap();
        assert_eq!(repaired, 3);

        for stage in RESET_INDEX_STAGES {
            let cp = tx.get::<tables::StageCheckpoints>(stage.to_string()).unwrap().unwrap();
            assert_eq!(cp.block_number, migration, "{stage} must be advanced to the migration block");
        }
        // SenderRecovery is not a download-reset stage and must be left at the tip.
        let sender = tx.get::<tables::StageCheckpoints>(StageId::SenderRecovery.to_string()).unwrap().unwrap();
        assert_eq!(sender.block_number, tip, "SenderRecovery must not be touched");
    }

    /// When the index was downloaded (`--archive`/`--with-rocksdb`), the checkpoints are already at
    /// the tip, so the repair is a no-op — proving idempotency and safety on healthy datadirs.
    #[test]
    fn noop_when_index_stages_already_at_tip() {
        let migration = 31_056_500u64;
        let tip = migration + 1_000;
        let factory = create_test_provider_factory();
        let provider_rw = factory.database_provider_rw().unwrap();
        let tx = provider_rw.tx_ref();

        tx.put::<tables::StageCheckpoints>(StageId::Finish.to_string(), StageCheckpoint::new(tip))
            .unwrap();
        for stage in RESET_INDEX_STAGES {
            tx.put::<tables::StageCheckpoints>(stage.to_string(), StageCheckpoint::new(tip)).unwrap();
        }

        let repaired = repair_index_checkpoints_tx(tx, migration).unwrap();
        assert_eq!(repaired, 0);
        for stage in RESET_INDEX_STAGES {
            let cp = tx.get::<tables::StageCheckpoints>(stage.to_string()).unwrap().unwrap();
            assert_eq!(cp.block_number, tip, "{stage} must be left untouched");
        }
    }

    /// A datadir whose tip is below the migration block is not a valid post-migration snapshot; the
    /// repair must refuse to touch it rather than corrupt an unrelated datadir.
    #[test]
    fn skips_when_tip_below_migration_block() {
        let migration = 31_056_500u64;
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

        let repaired = repair_index_checkpoints_tx(tx, migration).unwrap();
        assert_eq!(repaired, 0);
        let cp =
            tx.get::<tables::StageCheckpoints>(StageId::TransactionLookup.to_string()).unwrap().unwrap();
        assert_eq!(cp.block_number, 0, "TransactionLookup must be left untouched when tip < migration");
    }

    /// Without a `Finish` checkpoint the datadir is not a completed download; the repair no-ops.
    #[test]
    fn skips_when_no_finish_checkpoint() {
        let migration = 31_056_500u64;
        let factory = create_test_provider_factory();
        let provider_rw = factory.database_provider_rw().unwrap();
        let tx = provider_rw.tx_ref();

        tx.put::<tables::StageCheckpoints>(
            StageId::TransactionLookup.to_string(),
            StageCheckpoint::new(0),
        )
        .unwrap();

        let repaired = repair_index_checkpoints_tx(tx, migration).unwrap();
        assert_eq!(repaired, 0);
    }
}
