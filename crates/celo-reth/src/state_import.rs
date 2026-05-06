//! Celo L1 → L2 state-dump import for the `celo-reth import-celo-state` subcommand.

use alloy_consensus::{EMPTY_OMMER_ROOT_HASH, EMPTY_ROOT_HASH, Header};
use alloy_primitives::{B64, B256, U256, address, b256, bloom, bytes};
use clap::Parser;
use reth_chainspec::EthChainSpec;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::common::{AccessRights, Environment, EnvironmentArgs};
use reth_db_common::init::init_from_state_dump;
use reth_node_core::args::{DatabaseArgs, DatadirArgs, StaticFilesArgs, StorageArgs};
use reth_optimism_node::OpNode;
use reth_primitives_traits::{SealedHeader, header::HeaderMut};
use reth_provider::{
    BlockNumReader, DBProvider, DatabaseProviderFactory, StaticFileProviderFactory,
    StaticFileWriter,
};
use std::{io::BufReader, path::PathBuf};
use tracing::info;

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
    pub async fn execute(self) -> eyre::Result<()> {
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

        let Environment { config, provider_factory, .. } =
            env_args.init::<OpNode>(AccessRights::RW)?;

        let static_file_provider = provider_factory.static_file_provider();
        let provider_rw = provider_factory.database_provider_rw()?;

        let last_block_number = provider_rw.last_block_number()?;
        if last_block_number == 0 {
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
        } else if last_block_number < CEL2_MIGRATION_BLOCK_NUMBER {
            return Err(eyre::eyre!(
                "data directory must be empty when running import-celo-state \
                 (current tip block #{last_block_number})"
            ));
        }

        info!(target: "reth::cli", path = ?self.state, "Initiating state dump");

        let reader = BufReader::new(reth_fs_util::open(self.state)?);
        let hash = init_from_state_dump(reader, &provider_rw, config.stages.etl)?;

        provider_rw.commit()?;

        info!(
            target: "reth::cli",
            state_root = ?hash,
            migration_hash = ?CEL2_HEADER_HASH,
            "Migration block written",
        );
        Ok(())
    }
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
