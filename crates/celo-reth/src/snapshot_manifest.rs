//! Celo-aware `celo-reth snapshot-manifest` publisher.
//!
//! Wraps the upstream snapshot packaging
//! ([`reth_cli_commands::download::manifest::generate_manifest`]) with a Celo-specific
//! stage-checkpoint reconciliation step, then writes `manifest.json`.
//!
//! # Why
//!
//! Celo Mainnet is a migrated chain: blocks `1..CEL2_MIGRATION_BLOCK_NUMBER` are header-only
//! dummy placeholders imported via `import-celo-state` — they have no bodies. In v2 storage the
//! transaction-lookup index lives in RocksDB, so a running node never advances the MDBX
//! `TransactionLookup` stage checkpoint past its post-import value of `0`, even though the index
//! itself is complete. The upstream publisher opens the source DB read-only and packages the MDBX
//! verbatim, so the published snapshot ships `TransactionLookup = 0`.
//!
//! On restore, the pipeline sees that checkpoint, tries to *rebuild* tx-lookup from block 1, hits
//! the header-only pre-migration gap, and dies with `ProviderError::BlockBodyIndicesNotFound(1)`
//! ("block meta not found for block #1"). The snapshot is internally inconsistent: every other
//! stage is at head, but tx-lookup claims it has done nothing.
//!
//! Since the snapshot's data (and the shipped RocksDB tx-lookup index) is already complete through
//! the snapshot block, advancing any lagging `MIGRATED_CHAIN_STAGES` checkpoint to that block is
//! correct and makes the published snapshot bootable. This mirrors the reasoning in
//! [`crate::celo_migrate_v2`], which likewise refuses to reset these stages to `0` on a migrated
//! datadir.

use clap::Parser;
use eyre::{Result, WrapErr};
use reth_cli_commands::download::manifest::generate_manifest;
use reth_db::{Database, mdbx::DatabaseArguments, open_db, tables};
use reth_db_api::transaction::{DbTx, DbTxMut};
use reth_stages_types::{StageCheckpoint, StageId};
use std::path::{Path, PathBuf};
use tracing::info;

use crate::state_import::CELO_MAINNET_CHAIN_ID;

/// Pipeline stages that consume transaction bodies (or trie state derived from them) and therefore
/// cannot be rebuilt over Celo's header-only pre-migration placeholder blocks. If any is shipped
/// behind the snapshot block, a from-snapshot pipeline run rebuilds it from block 1 and fails with
/// `BlockBodyIndicesNotFound(1)`. Matches the set protected by [`crate::celo_migrate_v2`].
const MIGRATED_CHAIN_STAGES: [StageId; 6] = [
    StageId::SenderRecovery,
    StageId::TransactionLookup,
    StageId::IndexAccountHistory,
    StageId::IndexStorageHistory,
    StageId::MerkleExecute,
    StageId::MerkleUnwind,
];

/// Generate a chunked snapshot manifest from a local datadir (Celo publisher tool).
///
/// Same arguments as upstream `reth snapshot-manifest`, plus a Celo stage-checkpoint
/// reconciliation pass (see module docs) run before packaging.
#[derive(Debug, Parser)]
pub struct CeloSnapshotManifestCommand {
    /// Source datadir containing static files.
    ///
    /// Must be a *static* datadir (a ZFS clone or copy of a stopped node) — the reconciliation
    /// step opens the MDBX read-write, so it must not be a live/running node's datadir.
    #[arg(long, short = 'd')]
    source_datadir: PathBuf,

    /// Optional base URL where archives will be hosted.
    #[arg(long)]
    base_url: Option<String>,

    /// Output directory where chunk archives and manifest.json are written.
    #[arg(long, short = 'o')]
    output_dir: PathBuf,

    /// Block number this snapshot was taken at.
    ///
    /// If omitted, this is inferred from the source datadir's `Finish` stage checkpoint.
    #[arg(long)]
    block: Option<u64>,

    /// Chain ID.
    #[arg(long, default_value = "1")]
    chain_id: u64,

    /// Blocks per archive file for chunked components.
    ///
    /// If omitted, this is inferred from header static file ranges in the source datadir.
    #[arg(long)]
    blocks_per_file: Option<u64>,
}

impl CeloSnapshotManifestCommand {
    /// Reconciles lagging stage checkpoints, packages the archives, and writes the manifest.
    pub fn execute(self) -> Result<()> {
        // Open the source DB read-write, resolve the snapshot block, and advance any lagging
        // migrated-chain stage checkpoint up to it (the Celo fix). Returns the resolved block.
        let block = reconcile_stage_checkpoints(&self.source_datadir, self.block, self.chain_id)?;

        let blocks_per_file = match self.blocks_per_file {
            Some(blocks_per_file) => blocks_per_file,
            None => infer_blocks_per_file(&self.source_datadir)?,
        };

        info!(
            target: "reth::cli",
            dir = ?self.source_datadir,
            output = ?self.output_dir,
            block,
            blocks_per_file,
            "Packaging modular snapshot archives (Celo)"
        );
        let manifest = generate_manifest(
            &self.source_datadir,
            &self.output_dir,
            self.base_url.as_deref(),
            block,
            self.chain_id,
            blocks_per_file,
        )?;

        let num_components = manifest.components.len();
        let json = serde_json::to_string_pretty(&manifest)?;
        let output = self.output_dir.join("manifest.json");
        reth_fs_util::write(&output, &json)?;
        info!(
            target: "reth::cli",
            path = ?output,
            components = num_components,
            block = manifest.block,
            "Manifest written"
        );

        Ok(())
    }
}

/// Opens the source MDBX read-write, resolves the snapshot block (explicit `--block` or the
/// `Finish` stage checkpoint), and advances any [`MIGRATED_CHAIN_STAGES`] checkpoint that is behind
/// the block up to it. Stages already at or beyond the block are left untouched.
fn reconcile_stage_checkpoints(
    source_datadir: &Path,
    block_arg: Option<u64>,
    chain_id: u64,
) -> Result<u64> {
    let db_dir = source_datadir.join("db");
    let db_dir = if db_dir.exists() { db_dir } else { source_datadir.to_path_buf() };

    let db = open_db(&db_dir, DatabaseArguments::default())
        .wrap_err("failed to open source db read-write for stage-checkpoint reconciliation")?;
    let tx = db.tx_mut()?;

    let finish_tip = tx
        .get::<tables::StageCheckpoints>(StageId::Finish.to_string())?
        .map(|checkpoint| checkpoint.block_number);

    let block = match block_arg {
        // Reject an explicit --block above the source `Finish` tip: stamping the migrated-chain
        // stages complete beyond the data the source datadir actually contains (a typo'd or future
        // height) would let a retry publish a snapshot that skips required index/trie work.
        Some(block) => match finish_tip {
            Some(tip) if block > tip => eyre::bail!(
                "--block {block} is above the source Finish checkpoint {tip}; refusing to \
                 reconcile stage checkpoints beyond the data the source datadir contains"
            ),
            _ => block,
        },
        None => finish_tip.ok_or_else(|| {
            eyre::eyre!(
                "Could not infer --block from source DB (Finish checkpoint missing); \
                 pass --block manually"
            )
        })?,
    };

    let mut advanced = 0usize;
    // Only migrated Celo mainnet has the header-only pre-migration gap that makes a lagging
    // index/trie stage checkpoint correct to advance. On a genesis-contiguous chain a lagging
    // checkpoint means real work is genuinely missing, so advancing it would publish a snapshot
    // that skips rebuilding required indexes/trie on restore.
    if chain_id == CELO_MAINNET_CHAIN_ID {
        for stage in MIGRATED_CHAIN_STAGES {
            let key = stage.to_string();
            let current = tx
                .get::<tables::StageCheckpoints>(key.clone())?
                .map(|checkpoint| checkpoint.block_number)
                .unwrap_or(0);
            if current < block {
                tx.put::<tables::StageCheckpoints>(key, StageCheckpoint::new(block))?;
                info!(
                    target: "reth::cli",
                    stage = %stage,
                    from = current,
                    to = block,
                    "Advanced lagging stage checkpoint for snapshot publication"
                );
                advanced += 1;
            }
        }
    } else {
        info!(
            target: "reth::cli",
            chain_id,
            "Non-migrated chain; skipping migrated-chain stage-checkpoint reconciliation",
        );
    }
    tx.commit()?;

    info!(
        target: "reth::cli",
        block,
        advanced,
        "Reconciled stage checkpoints for snapshot publication"
    );
    Ok(block)
}

/// Infers the static-file block span from header static-file ranges in the source datadir.
fn infer_blocks_per_file(source_datadir: &Path) -> Result<u64> {
    let mut inferred = None;
    for (start, end) in header_ranges(source_datadir)? {
        let span = end.saturating_sub(start).saturating_add(1);
        if span == 0 {
            continue;
        }
        match inferred {
            Some(existing) if existing != span => {
                eyre::bail!(
                    "Inconsistent header static file ranges; pass --blocks-per-file manually"
                )
            }
            None => inferred = Some(span),
            _ => {}
        }
    }
    inferred.ok_or_else(|| {
        eyre::eyre!("Could not infer --blocks-per-file from header static files; pass it manually")
    })
}

/// Collects header static-file `(start, end)` ranges from the source datadir.
fn header_ranges(source_datadir: &Path) -> Result<Vec<(u64, u64)>> {
    let static_files_dir = source_datadir.join("static_files");
    let static_files_dir =
        if static_files_dir.exists() { static_files_dir } else { source_datadir.to_path_buf() };

    let entries = std::fs::read_dir(&static_files_dir).wrap_err_with(|| {
        format!("Failed to read static files directory: {}", static_files_dir.display())
    })?;

    let mut ranges = Vec::new();
    for entry in entries {
        let file_name = entry?.file_name();
        if let Some(range) = parse_headers_range(&file_name.to_string_lossy()) {
            ranges.push(range);
        }
    }

    Ok(ranges)
}

/// Parses `(start, end)` from a `static_file_headers_{start}_{end}[suffix]` file name.
fn parse_headers_range(file_name: &str) -> Option<(u64, u64)> {
    let remainder = file_name.strip_prefix("static_file_headers_")?;
    let (start, end_with_suffix) = remainder.split_once('_')?;

    let start = start.parse::<u64>().ok()?;
    let end_digits: String = end_with_suffix.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    let end = end_digits.parse::<u64>().ok()?;

    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_headers_range_works_with_suffixes() {
        assert_eq!(parse_headers_range("static_file_headers_0_499999"), Some((0, 499_999)));
        assert_eq!(
            parse_headers_range("static_file_headers_500000_999999.jar"),
            Some((500_000, 999_999))
        );
        assert_eq!(parse_headers_range("static_file_transactions_0_499999"), None);
    }

    #[test]
    fn migrated_chain_stages_include_transaction_lookup() {
        assert!(MIGRATED_CHAIN_STAGES.contains(&StageId::TransactionLookup));
    }
}
