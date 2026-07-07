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
    /// On Celo mainnet the reconciliation step opens this datadir's MDBX read-write to advance
    /// lagging stage checkpoints, so it must be a *static* datadir (a ZFS clone or copy of a
    /// stopped node), never a live/running node's. Other chains are never opened as MDBX — the
    /// snapshot block is inferred from the header static files — so a read-only/static source
    /// works there.
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
        // Resolve the snapshot block and, on Celo mainnet, advance any lagging migrated-chain stage
        // checkpoint up to it (the Celo fix). Opens the source MDBX read-write on mainnet; other
        // chains infer the block from header static files without opening MDBX. Returns the block.
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

/// Resolves the snapshot block and, on Celo mainnet, advances any [`MIGRATED_CHAIN_STAGES`]
/// checkpoint that is behind the block up to it.
///
/// Only migrated Celo mainnet has the header-only pre-migration gap that makes a lagging index/trie
/// stage checkpoint correct to advance, so only there is the source MDBX opened (read-write) at
/// all, with the block resolved from its `Finish` checkpoint. On every other chain a lagging
/// checkpoint means real work is genuinely missing — advancing it would publish a snapshot that
/// skips rebuilding required indexes/trie on restore — so the block is resolved from `--block` or
/// the highest header static-file range without opening MDBX, letting the publisher run against a
/// read-only/static source (matching upstream's header-tip fallback).
fn reconcile_stage_checkpoints(
    source_datadir: &Path,
    block_arg: Option<u64>,
    chain_id: u64,
) -> Result<u64> {
    if chain_id != CELO_MAINNET_CHAIN_ID {
        let block = resolve_block_against_tip(block_arg, max_header_block(source_datadir))?;
        info!(
            target: "reth::cli",
            chain_id,
            block,
            "Non-migrated chain; skipping migrated-chain stage-checkpoint reconciliation",
        );
        return Ok(block);
    }

    let db_dir = source_datadir.join("db");
    let db_dir = if db_dir.exists() { db_dir } else { source_datadir.to_path_buf() };
    let db = open_db(&db_dir, DatabaseArguments::default())
        .wrap_err("failed to open source db read-write for stage-checkpoint reconciliation")?;
    let tx = db.tx_mut()?;
    let finish_tip = tx
        .get::<tables::StageCheckpoints>(StageId::Finish.to_string())?
        .map(|checkpoint| checkpoint.block_number);
    let block = resolve_block_against_tip(block_arg, finish_tip)?;

    let mut advanced = 0usize;
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
    tx.commit()?;

    info!(
        target: "reth::cli",
        block,
        advanced,
        "Reconciled stage checkpoints for snapshot publication"
    );
    Ok(block)
}

/// Resolves the snapshot block: an explicit `--block` (validated to be at or below the source `tip`
/// when it is known) or, when omitted, the `tip` itself. Pure; `tip` is the source `Finish`
/// checkpoint on Celo mainnet, or the highest header static-file block on other chains.
fn resolve_block_against_tip(block_arg: Option<u64>, tip: Option<u64>) -> Result<u64> {
    match block_arg {
        // Reject an explicit --block above the source tip: stamping the migrated-chain stages
        // complete beyond the data the source datadir actually contains (a typo'd or future height)
        // would let a retry publish a snapshot that skips required index/trie work.
        Some(block) => match tip {
            Some(tip) if block > tip => eyre::bail!(
                "--block {block} is above the source tip {tip}; refusing to publish a snapshot \
                 beyond the data the source datadir contains"
            ),
            _ => Ok(block),
        },
        None => tip.ok_or_else(|| {
            eyre::eyre!(
                "Could not infer the snapshot block from the source datadir (no Finish checkpoint \
                 or header static-file range found); pass --block manually"
            )
        }),
    }
}

/// Highest header block available as a static file in the source datadir, if any.
///
/// Used as the snapshot-tip fallback for non-mainnet sources, which are never opened as MDBX, so a
/// read-only/static source (or one without a `Finish` checkpoint) still resolves a block.
fn max_header_block(source_datadir: &Path) -> Option<u64> {
    header_ranges(source_datadir).ok()?.into_iter().map(|(_, end)| end).max()
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

    /// Asserts what the shared block resolver returns for a given source tip and `--block` arg.
    fn assert_resolves(tip: Option<u64>, block_arg: Option<u64>, expected: Option<u64>) {
        match expected {
            Some(block) => assert_eq!(resolve_block_against_tip(block_arg, tip).unwrap(), block),
            None => assert!(resolve_block_against_tip(block_arg, tip).is_err()),
        }
    }

    /// No `--block`: the snapshot block is inferred from the source tip.
    #[test]
    fn resolve_block_infers_from_tip() {
        assert_resolves(Some(31_100_000), None, Some(31_100_000));
    }

    /// An explicit `--block` at or below the source tip is accepted verbatim.
    #[test]
    fn resolve_block_accepts_block_at_or_below_tip() {
        assert_resolves(Some(100), Some(100), Some(100));
        assert_resolves(Some(100), Some(50), Some(50));
    }

    /// An explicit `--block` above the source tip is rejected (would publish a snapshot claiming
    /// index/trie work beyond the data the source datadir actually contains).
    #[test]
    fn resolve_block_rejects_block_above_tip() {
        assert_resolves(Some(100), Some(101), None);
    }

    /// An explicit `--block` is honored even when the tip is unknown (no MDBX and no header files).
    #[test]
    fn resolve_block_honors_explicit_block_without_tip() {
        assert_resolves(None, Some(42), Some(42));
    }

    /// Without a tip and without `--block`, resolution fails with a clear ask.
    #[test]
    fn resolve_block_errors_without_tip_or_block() {
        assert_resolves(None, None, None);
    }

    /// Creates `static_files/<names>` under a fresh temp dir and returns the (kept-alive) handle.
    fn datadir_with_headers(names: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let static_files = dir.path().join("static_files");
        std::fs::create_dir_all(&static_files).unwrap();
        for name in names {
            std::fs::write(static_files.join(name), b"").unwrap();
        }
        dir
    }

    /// `max_header_block` returns the highest header static-file end, ignoring non-header files.
    #[test]
    fn max_header_block_reads_highest_header_range() {
        let dir = datadir_with_headers(&[
            "static_file_headers_0_499999",
            "static_file_headers_500000_999999.jar",
            "static_file_transactions_0_499999",
        ]);
        assert_eq!(max_header_block(dir.path()), Some(999_999));

        let empty = tempfile::tempdir().unwrap();
        assert_eq!(max_header_block(empty.path()), None);
    }

    /// A non-mainnet source with header static files but no MDBX still resolves its block from the
    /// header tip — never opening (let alone write-opening) the source ([8]).
    #[test]
    fn non_mainnet_infers_block_from_headers_without_mdbx() {
        let dir = datadir_with_headers(&["static_file_headers_0_499999"]);
        assert_eq!(reconcile_stage_checkpoints(dir.path(), None, 12_345).unwrap(), 499_999);
        assert_eq!(
            reconcile_stage_checkpoints(dir.path(), Some(400_000), 12_345).unwrap(),
            400_000
        );
        assert!(reconcile_stage_checkpoints(dir.path(), Some(500_000), 12_345).is_err());
    }
}
