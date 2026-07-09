//! Example for creating a static test fixture for `celo-executor` from a live chain
//!
//! ## Usage
//!
//! ```sh
//! cargo run --release -p execution-fixture
//! ```
//!
//! ## Inputs
//!
//! The test fixture creator takes the following inputs:
//!
//! - `-v` or `--verbosity`: Verbosity level (0-2)
//! - `-r` or `--l2-rpc`: The L2 execution layer RPC URL to use. Must be archival.
//! - `-b` or `--block-number`: L2 block number to execute for the fixture.
//! - `-o` or `--output-dir`: (Optional) The output directory for the fixture. If not provided,
//!   defaults to `celo-executor`'s `testdata` directory.
//! - `-c` or `--rollup-config`: (Optional) Path to the chain's `rollup.json`. If not provided, the
//!   config is looked up in `celo-registry` by the RPC's chain ID — which only knows the production
//!   Celo chains, so a dev chain must pass one.
//! - `--preimage-source`: (Optional) How to source trie/bytecode/header preimages. Defaults to
//!   probing `debug_executionWitness` (served by reth) and falling back to `debug_dbGet` (served by
//!   archival op-geth).

use anyhow::{Context, Result, anyhow};
use celo_executor::test_utils::{PreimageSource, create_static_fixture};
use celo_genesis::CeloRollupConfig;
use clap::{Parser, ValueEnum};
use kona_cli::{LogArgs, LogConfig};
use std::path::PathBuf;
use tracing::info;
use tracing_subscriber::EnvFilter;
use url::Url;

/// The execution fixture creation command.
#[derive(Parser, Debug, Clone)]
#[command(about = "Creates a static test fixture for `celo-executor` from a live chain")]
pub struct ExecutionFixtureCommand {
    /// Logging arguments.
    #[command(flatten)]
    pub log_args: LogArgs,
    /// The L2 archive EL to use.
    #[arg(long, short = 'r')]
    pub l2_rpc: Url,
    /// L2 block number to execute.
    #[arg(long, short = 'b')]
    pub block_number: u64,
    /// The output directory for the fixture.
    #[arg(long, short = 'o')]
    pub output_dir: Option<PathBuf>,
    /// Path to the chain's `rollup.json`. Required for chains `celo-registry` does not know.
    #[arg(long, short = 'c')]
    pub rollup_config: Option<PathBuf>,
    /// Where to source the execution preimages from.
    #[arg(long, value_enum, default_value_t = PreimageSourceArg::Auto)]
    pub preimage_source: PreimageSourceArg,
}

/// CLI mirror of [`PreimageSource`].
#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum PreimageSourceArg {
    /// Probe `debug_executionWitness`, falling back to `debug_dbGet`.
    Auto,
    /// Fetch preimages on demand via `debug_dbGet` (archival op-geth).
    DbGet,
    /// Collect preimages up front from `debug_executionWitness` (reth).
    Witness,
}

impl From<PreimageSourceArg> for PreimageSource {
    fn from(arg: PreimageSourceArg) -> Self {
        match arg {
            PreimageSourceArg::Auto => Self::Auto,
            PreimageSourceArg::DbGet => Self::DbGet,
            PreimageSourceArg::Witness => Self::Witness,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = ExecutionFixtureCommand::parse();
    LogConfig::new(cli.log_args).init_tracing_subscriber(None::<EnvFilter>)?;

    let output_dir = if let Some(output_dir) = cli.output_dir {
        output_dir
    } else {
        // Default to `crates/kona/executor/testdata`
        let output = std::process::Command::new(env!("CARGO"))
            .arg("locate-project")
            .arg("--workspace")
            .arg("--message-format=plain")
            .output()?
            .stdout;
        let workspace_root: PathBuf = String::from_utf8(output)?.trim().into();

        workspace_root
            .parent()
            .ok_or(anyhow!("Failed to locate workspace root"))?
            .join("crates/kona/executor/testdata")
    };

    let rollup_config = cli
        .rollup_config
        .map(|path| -> Result<CeloRollupConfig> {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to read rollup config at {}", path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("Failed to parse rollup config at {}", path.display()))
        })
        .transpose()?;

    create_static_fixture(
        cli.l2_rpc.as_str(),
        cli.block_number,
        output_dir,
        rollup_config,
        cli.preimage_source.into(),
    )
    .await;

    info!(block_number = cli.block_number, "Successfully created static test fixture");
    Ok(())
}
