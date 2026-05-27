//! Celo L1 → L2 state-dump import for the `celo-reth import-celo-state` subcommand.

use alloy_consensus::{BlockHeader, EMPTY_OMMER_ROOT_HASH, EMPTY_ROOT_HASH, Header};
use alloy_genesis::GenesisAccount;
use alloy_primitives::{B64, B256, U256, address, b256, bloom, bytes, keccak256, map::B256Set};
use clap::Parser;
use reth_chainspec::EthChainSpec;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::common::{AccessRights, Environment, EnvironmentArgs};
use reth_codecs::Compact;
use reth_config::config::EtlConfig;
use reth_db_api::{
    cursor::{DbCursorRW, DbDupCursorRW},
    models::{
        AccountBeforeTx, BlockNumberAddress, IntegerList, ShardedKey,
        storage_sharded_key::StorageShardedKey,
    },
    tables,
    transaction::DbTxMut,
};
use reth_etl::Collector;
use reth_node_core::args::{DatabaseArgs, DatadirArgs, StaticFilesArgs, StorageArgs};
use reth_optimism_node::OpNode;
use reth_primitives_traits::{Account, Bytecode, SealedHeader, StorageEntry, header::HeaderMut};
use reth_provider::{
    BlockHashReader, BlockNumReader, ChainSpecProvider, DBProvider, DatabaseProviderFactory,
    HeaderProvider, ProviderError, StageCheckpointWriter, StaticFileProviderFactory,
    StaticFileSegment, StaticFileWriter, TrieWriter,
};
use reth_stages_types::{StageCheckpoint, StageId};
use reth_storage_api::StorageSettingsCache;
use reth_trie::{IntermediateStateRootState, StateRootProgress};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
};
use tracing::{debug, error, info, trace};

use crate::chainspec::CeloChainSpecParser;

/// Celo Mainnet chain ID.
pub const CELO_MAINNET_CHAIN_ID: u64 = 42220;

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
    /// The `runtime` is forwarded to [`EnvironmentArgs::init`] for parallel storage I/O.
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

        let env_args = EnvironmentArgs::<CeloChainSpecParser> {
            datadir: self.datadir,
            config: self.config,
            chain,
            db: self.db,
            static_files: self.static_files,
            storage: StorageArgs::default(),
        };

        let Environment { config, provider_factory, .. } =
            env_args.init::<OpNode>(AccessRights::RW, runtime)?;

        if !provider_factory.cached_storage_settings().storage_v2 {
            return Err(eyre::eyre!(
                "import-celo-state requires storage v2 but this datadir has v1 settings"
            ));
        }

        let static_file_provider = provider_factory.static_file_provider();

        // Write the Cel2 migration header. `init_from_state_dump` below opens its own
        // provider and commits in chunks, so the header must be committed up front.
        {
            let provider_rw = provider_factory.database_provider_rw()?;

            let last_block_number = provider_rw.last_block_number()?;
            if last_block_number != 0 {
                return Err(eyre::eyre!(
                    "data directory must be empty when running import-celo-state \
                     (current tip block #{last_block_number})"
                ));
            }

            reth_cli_commands::init_state::without_evm::setup_without_evm(
                &provider_rw,
                SealedHeader::new(CEL2_HEADER, CEL2_HEADER_HASH),
                |number| {
                    let mut header = Header::default();
                    header.set_number(number);
                    header
                },
            )?;

            // Changeset segments must cover the same block range as headers/transactions.
            // `setup_without_evm` advances headers, transactions, receipts, and senders
            // through the dummy blocks but does not touch changeset segments. Advance
            // them here so the static file provider considers all segments consistent at
            // the migration block.
            for segment in
                [StaticFileSegment::AccountChangeSets, StaticFileSegment::StorageChangeSets]
            {
                static_file_provider
                    .latest_writer(segment)?
                    .ensure_at_block(CEL2_MIGRATION_BLOCK_NUMBER)?;
            }

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
        let hash = celo_init_from_state_dump(reader, &provider_factory, config.stages.etl)?;

        info!(
            target: "reth::cli",
            state_root = ?hash,
            migration_hash = ?stored,
            "Migration block written",
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Local copy of `reth_db_common::init::init_from_state_dump` and its private
// helpers. Identical to upstream except `compute_state_root_chunked` is replaced
// with the non-chunked `compute_state_root` to work around an upstream bug
// where the chunked variant replays the boundary storage key after a commit,
// causing a panic in `HashBuilder::add_leaf` for accounts with large storage.
// ---------------------------------------------------------------------------

/// Max storage "units" (1 per account + 1 per slot) per MDBX commit batch.
const STORAGE_COMMIT_THRESHOLD: usize = 500_000;

/// Soft limit for flushed trie updates before logging progress.
const SOFT_LIMIT_COUNT_FLUSHED_UPDATES: usize = 1_000_000;

#[derive(Debug, Serialize, Deserialize)]
struct DumpStateRoot {
    root: B256,
}

#[derive(Debug, Deserialize)]
struct GenesisAccountWithAddress {
    #[serde(flatten)]
    genesis_account: GenesisAccount,
    address: alloy_primitives::Address,
}

/// Replacement for `reth_db_common::init::init_from_state_dump` that uses a
/// single-transaction state root computation instead of the chunked variant.
fn celo_init_from_state_dump<PF>(
    mut reader: impl BufRead,
    provider_factory: &PF,
    etl_config: EtlConfig,
) -> eyre::Result<B256>
where
    PF: DatabaseProviderFactory<
        ProviderRW: DBProvider<Tx: DbTxMut>
                        + BlockNumReader
                        + BlockHashReader
                        + ChainSpecProvider
                        + StageCheckpointWriter
                        + HeaderProvider
                        + TrieWriter
                        + StorageSettingsCache,
    >,
{
    if etl_config.file_size == 0 {
        return Err(eyre::eyre!("ETL file size cannot be zero"));
    }

    // Read block metadata from the provider.
    let (block, hash, expected_state_root) = {
        let provider_rw = provider_factory.database_provider_rw()?;
        let block = provider_rw.last_block_number()?;
        let hash = provider_rw
            .block_hash(block)?
            .ok_or_else(|| eyre::eyre!("Block hash not found for block {block}"))?;
        let header = provider_rw
            .header_by_number(block)?
            .map(|h| SealedHeader::new(h, hash))
            .ok_or_else(|| ProviderError::HeaderNotFound(block.into()))?;
        let state_root = header.header().state_root();

        debug!(target: "reth::cli", block, chain=%provider_rw.chain_spec().chain(), "Initializing state at block");

        (block, hash, state_root)
    };

    // Verify dump state root matches the migration header.
    let dump_state_root = {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        serde_json::from_str::<DumpStateRoot>(&line)?.root
    };
    if expected_state_root != dump_state_root {
        error!(target: "reth::cli", ?dump_state_root, ?expected_state_root,
            "State root from state dump does not match state root in current header.");
        return Err(eyre::eyre!(
            "state root mismatch: dump={dump_state_root:?} expected={expected_state_root:?}"
        ));
    }

    // Parse accounts into an ETL collector (sorted by address).
    let collector = {
        let mut line = String::new();
        let mut collector = Collector::new(etl_config.file_size, etl_config.dir);
        loop {
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                break;
            }
            let entry: GenesisAccountWithAddress = serde_json::from_str(&line)?;
            collector.insert(entry.address, entry.genesis_account)?;
            line.clear();
        }
        collector
    };

    // Write accounts to MDBX in batches.
    dump_state(collector, provider_factory, block)?;

    info!(target: "reth::cli",
        "All accounts written to database, starting state root computation (may take some time)");

    // Clear trie tables so state root is computed from scratch.
    {
        let provider_rw = provider_factory.database_provider_rw()?;
        provider_rw.tx_ref().clear::<tables::AccountsTrie>()?;
        provider_rw.tx_ref().clear::<tables::StoragesTrie>()?;
        provider_rw.commit()?;
    }

    // Compute state root in a single transaction (avoids the upstream chunked
    // computation bug that replays boundary keys across transaction commits).
    let computed_state_root = {
        let provider_rw = provider_factory.database_provider_rw()?;

        let root = reth_trie_db::with_adapter!(&provider_rw, |A| {
            compute_state_root::<_, A>(&provider_rw)
        })?;
        provider_rw.commit()?;
        root
    };

    if computed_state_root == expected_state_root {
        info!(target: "reth::cli", ?computed_state_root,
            "Computed state root matches state root in state dump");
    } else {
        error!(target: "reth::cli", ?computed_state_root, ?expected_state_root,
            "Computed state root does not match state root in state dump");
        return Err(eyre::eyre!(
            "state root mismatch: computed={computed_state_root:?} expected={expected_state_root:?}"
        ));
    }

    // Set stage checkpoints for stages that require state.
    {
        let provider_rw = provider_factory.database_provider_rw()?;
        for stage in StageId::STATE_REQUIRED {
            provider_rw.save_stage_checkpoint(stage, StageCheckpoint::new(block))?;
        }
        provider_rw.commit()?;
    }

    Ok(hash)
}

/// Single-transaction state root computation. Unlike the upstream chunked
/// version, this keeps one MDBX transaction open for the entire computation,
/// avoiding the boundary-key replay bug.
fn compute_state_root<Provider, A>(provider: &Provider) -> Result<B256, eyre::Error>
where
    Provider: DBProvider<Tx: DbTxMut> + TrieWriter + StorageSettingsCache,
    A: reth_trie_db::TrieTableAdapter,
{
    let tx = provider.tx_ref();
    let mut intermediate_state: Option<IntermediateStateRootState> = None;
    let mut total_flushed_updates: usize = 0;

    loop {
        let cursor_factory = reth_trie_db::DatabaseTrieCursorFactory::<_, A>::new(tx);
        let hashed_factory = reth_trie_db::DatabaseHashedCursorFactory::new(tx);
        let state_root = reth_trie::StateRoot::new(cursor_factory, hashed_factory)
            .with_intermediate_state(intermediate_state);

        match state_root.root_with_progress()? {
            StateRootProgress::Progress(state, _, updates) => {
                let updated_len = provider.write_trie_updates(updates)?;
                total_flushed_updates += updated_len;

                trace!(target: "reth::cli",
                    last_account_key = %state.account_root_state.last_hashed_key,
                    updated_len, total_flushed_updates,
                    "Flushing trie updates");

                intermediate_state = Some(*state);

                if total_flushed_updates % SOFT_LIMIT_COUNT_FLUSHED_UPDATES == 0 {
                    info!(target: "reth::cli", total_flushed_updates, "Flushing trie updates");
                }
            }
            StateRootProgress::Complete(root, _, updates) => {
                let updated_len = provider.write_trie_updates(updates)?;
                total_flushed_updates += updated_len;

                info!(target: "reth::cli", %root, total_flushed_updates,
                    "State root computation complete");

                return Ok(root);
            }
        }
    }
}

/// Write all accounts from the ETL collector to MDBX tables directly.
fn dump_state<PF>(
    mut collector: Collector<alloy_primitives::Address, GenesisAccount>,
    provider_factory: &PF,
    block: u64,
) -> Result<(), eyre::Error>
where
    PF: DatabaseProviderFactory<ProviderRW: DBProvider<Tx: DbTxMut>>,
{
    let accounts_len = collector.len();
    let mut total_accounts: usize = 0;
    let mut storage_units: usize = 0;

    let history_list = IntegerList::new([block])?;
    let mut seen_bytecodes: B256Set = B256Set::default();
    let mut provider_rw = provider_factory.database_provider_rw()?;

    for entry in collector.iter()? {
        let (address_raw, account_raw) = entry?;
        let (address, _) =
            alloy_primitives::Address::from_compact(address_raw.as_slice(), address_raw.len());
        let (account, _) = GenesisAccount::from_compact(account_raw.as_slice(), account_raw.len());

        let account_storage_len = account.storage.as_ref().map_or(0, |s| s.len());
        let account_units = 1 + account_storage_len;

        if storage_units > 0 && storage_units + account_units > STORAGE_COMMIT_THRESHOLD {
            provider_rw.commit()?;
            provider_rw = provider_factory.database_provider_rw()?;
            info!(target: "reth::cli", total_accounts, accounts_len, storage_units, "Committed chunk");
            storage_units = 0;
        }

        write_account_to_db(
            provider_rw.tx_ref(),
            &address,
            &account,
            block,
            &history_list,
            &mut seen_bytecodes,
        )?;

        total_accounts += 1;
        storage_units += account_units;

        if total_accounts % 100_000 == 0 {
            info!(target: "reth::cli", total_accounts, accounts_len, "Writing accounts...");
        }
    }

    provider_rw.commit()?;
    info!(target: "reth::cli", total_accounts, "All accounts written to database");
    Ok(())
}

/// Write a single account and its storage to all required MDBX tables.
fn write_account_to_db<TX: DbTxMut>(
    tx: &TX,
    address: &alloy_primitives::Address,
    genesis_account: &GenesisAccount,
    block: u64,
    history_list: &IntegerList,
    seen_bytecodes: &mut B256Set,
) -> Result<(), eyre::Error> {
    let bytecode_hash = if let Some(code) = &genesis_account.code {
        let bytecode = Bytecode::new_raw_checked(code.clone())
            .map_err(|e| eyre::eyre!("Invalid bytecode for {address}: {e}"))?;
        let hash = bytecode.hash_slow();
        if seen_bytecodes.insert(hash) {
            tx.put::<tables::Bytecodes>(hash, bytecode)?;
        }
        Some(hash)
    } else {
        None
    };

    let account = Account {
        nonce: genesis_account.nonce.unwrap_or_default(),
        balance: genesis_account.balance,
        bytecode_hash,
    };

    let hashed_address = keccak256(address);

    tx.put::<tables::PlainAccountState>(*address, account)?;
    tx.put::<tables::HashedAccounts>(hashed_address, account)?;

    let mut acct_cs_cursor = tx.cursor_dup_write::<tables::AccountChangeSets>()?;
    acct_cs_cursor.append_dup(block, AccountBeforeTx { address: *address, info: None })?;

    tx.put::<tables::AccountsHistory>(ShardedKey::new(*address, u64::MAX), history_list.clone())?;

    if let Some(storage) = &genesis_account.storage {
        let mut hashed_storage_cursor = tx.cursor_dup_write::<tables::HashedStorages>()?;
        let mut plain_storage_cursor = tx.cursor_dup_write::<tables::PlainStorageState>()?;
        let mut storage_cs_cursor = tx.cursor_dup_write::<tables::StorageChangeSets>()?;

        let mut sorted_slots: Vec<_> = storage.iter().collect();
        sorted_slots.sort_unstable_by_key(|(k, _)| *k);

        for &(&key, &value) in &sorted_slots {
            let value_u256 = U256::from_be_bytes(value.0);

            plain_storage_cursor.append_dup(*address, StorageEntry { key, value: value_u256 })?;

            let hashed_key = keccak256(key);
            hashed_storage_cursor
                .upsert(hashed_address, &StorageEntry { key: hashed_key, value: value_u256 })?;

            storage_cs_cursor.append_dup(
                BlockNumberAddress((block, *address)),
                StorageEntry { key, value: U256::ZERO },
            )?;

            tx.put::<tables::StoragesHistory>(
                StorageShardedKey::new(*address, key, u64::MAX),
                history_list.clone(),
            )?;
        }
    }

    Ok(())
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
