//! Celo reth node binary.

use alloy_celo_evm::blocklist::FeeCurrencyBlocklist;
use celo_reth::{
    CeloEvmConfig,
    chainspec::CeloChainSpecParser,
    node::{CeloConsensus, CeloNode, RollupArgs},
    payload::{DEFAULT_FEE_CURRENCY_LIMIT_FRACTION, FeeCurrencyLimits},
    rpc::{
        celo_admin_module, celo_fee_history_module, celo_gas_price_module, celo_tx_module,
        make_celo_fee_api,
    },
    state_import::ImportCeloStateCommand,
};
use clap::Parser;
use reth_chainspec::EthChainSpec;
use reth_cli_commands::stage;
use reth_cli_runner::CliRunner;
use reth_node_core::args::{LogArgs, TraceArgs};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_cli::Cli;
use reth_tracing::Layers;
use std::{ffi::OsString, sync::Arc};

/// Subcommand name for the Celo state import.
const IMPORT_CELO_STATE: &str = "import-celo-state";
const STAGE: &str = "stage";

/// Top-level Celo-only subcommand wrapper.
///
/// Used only when intercepting Celo-owned paths from the binary's argv before handing the rest of
/// the CLI off to the upstream op-reth `Cli`.
#[derive(Debug, Parser)]
#[command(name = "celo-reth")]
struct CeloCli {
    #[command(subcommand)]
    command: CeloCommand,

    #[command(flatten)]
    logs: LogArgs,

    #[command(flatten)]
    traces: TraceArgs,
}

#[derive(Debug, clap::Subcommand)]
enum CeloCommand {
    /// Initialize a Celo Mainnet datadir from an L1 state dump.
    #[command(name = IMPORT_CELO_STATE)]
    ImportCeloState(Box<ImportCeloStateCommand>),
    /// Manipulate individual stages using Celo primitives.
    #[command(name = STAGE)]
    Stage(Box<stage::Command<CeloChainSpecParser>>),
}

impl CeloCommand {
    fn chain_spec(&self) -> Option<&Arc<OpChainSpec>> {
        match self {
            Self::ImportCeloState(_) => None,
            Self::Stage(command) => command.chain_spec(),
        }
    }
}

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

/// Celo-specific CLI extension arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct CeloArgs {
    /// OP Stack rollup arguments.
    #[command(flatten)]
    pub rollup: RollupArgs,

    /// Per-fee-currency block space limits as fraction of block gas.
    ///
    /// Format: `address=fraction,address=fraction,...`
    /// Example: `0x765DE816845861e75A25fCA122bb6898B8B1282a=0.9,
    /// 0xD8763CBa276a3738E6DE85b4b3bF5FDed6D6cA73=0.5`
    #[arg(long = "celo.feecurrency.limits", value_name = "LIMITS")]
    pub fee_currency_limits: Option<String>,

    /// Default block space fraction for fee currencies not listed in `--celo.feecurrency.limits`.
    ///
    /// Native CELO transactions are always unrestricted regardless of this setting.
    #[arg(
        long = "celo.feecurrency.default",
        value_name = "FRACTION",
        default_value_t = DEFAULT_FEE_CURRENCY_LIMIT_FRACTION,
    )]
    pub fee_currency_default: f64,
}

fn main() {
    reth_cli_util::sigsegv_handler::install();

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe {
            std::env::set_var("RUST_BACKTRACE", "1");
        }
    }

    // Intercept Celo-specific command paths before handing argv off to the upstream op-reth `Cli`.
    // The subcommand must be `argv[1]` — global flags before it (e.g.
    // `celo-reth -v import-celo-state ...`) are not supported and these intercepted paths are
    // hidden from `celo-reth --help`. To lift either limitation we'd have to mirror op-reth's full
    // `Commands` enum in this crate.
    let argv: Vec<OsString> = std::env::args_os().collect();
    if argv.get(1).is_some_and(|a| a == IMPORT_CELO_STATE || a == STAGE) {
        if let Err(err) = run_celo_subcommand(argv) {
            eprintln!("Error: {err:?}");
            std::process::exit(1);
        }
        return;
    }

    if let Err(err) =
        Cli::<CeloChainSpecParser, CeloArgs>::parse().run(async move |builder, celo_args| {
            let rollup_args = celo_args.rollup;

            // Parse fee currency limits from CLI args.
            // If no explicit limits are provided, fall back to the chain's
            // built-in defaults (op-geth-matching for mainnet; empty elsewhere).
            let chain_id = builder.config().chain.chain().id();
            let limits = celo_args.fee_currency_limits.as_deref().map_or_else(
                || FeeCurrencyLimits::defaults_for_chain(chain_id),
                FeeCurrencyLimits::parse_limits,
            );
            let fee_currency_limits =
                FeeCurrencyLimits { limits, default_limit: celo_args.fee_currency_default };

            let blocklist = FeeCurrencyBlocklist::default();
            let handle = builder
                .node(
                    CeloNode::new(rollup_args)
                        .with_blocklist(blocklist.clone())
                        .with_fee_currency_limits(fee_currency_limits),
                )
                .extend_rpc_modules(move |ctx| {
                    let chain_id = ctx.config().chain.chain().id();
                    let fee_currency_directory =
                        celo_revm::constants::get_addresses(chain_id).fee_currency_directory;
                    let fee_api =
                        make_celo_fee_api(ctx.registry.eth_api().clone(), fee_currency_directory);
                    let fee_api = std::sync::Arc::new(fee_api);
                    let gas_module = celo_gas_price_module(fee_api.clone());
                    ctx.modules.replace_configured(gas_module)?;

                    let tx_module = celo_tx_module(fee_api.clone());
                    ctx.modules.replace_configured(tx_module)?;

                    let fee_history_module = celo_fee_history_module(fee_api);
                    ctx.modules.replace_configured(fee_history_module)?;

                    let admin_module = celo_admin_module(blocklist);
                    ctx.modules.merge_configured(admin_module)?;
                    Ok(())
                })
                .launch_with_debug_capabilities()
                .await?;
            handle.node_exit_future.await
        })
    {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}

/// Dispatch the Celo-specific subcommand path.
///
/// Mirrors upstream `CliApp`'s init order: build the runtime, wire OTLP layers (no-op if the
/// `otlp`/`otlp-logs` features aren't compiled in), then initialize file/stdout tracing, then
/// dispatch the command.
fn run_celo_subcommand(argv: Vec<OsString>) -> eyre::Result<()> {
    let mut cli = CeloCli::parse_from(argv);
    if let Some(chain_spec) = cli.command.chain_spec() {
        cli.logs.log_file_directory =
            cli.logs.log_file_directory.join(chain_spec.chain().to_string());
    }

    let runner = CliRunner::try_default_runtime()?;
    let mut layers = Layers::new();
    runner.block_on(cli.traces.init_otlp_tracing(&mut layers))?;
    runner.block_on(cli.traces.init_otlp_logs(&mut layers))?;
    let _guard = cli.logs.init_tracing_with_layers(layers)?;

    match cli.command {
        CeloCommand::ImportCeloState(cmd) => runner.run_blocking_until_ctrl_c(cmd.execute()),
        CeloCommand::Stage(cmd) => {
            let components = |spec: Arc<OpChainSpec>| {
                (CeloEvmConfig::celo(spec.clone()), Arc::new(CeloConsensus::new(spec)))
            };
            runner.run_command_until_exit(|ctx| cmd.execute::<CeloNode, _>(ctx, components))
        }
    }
}
