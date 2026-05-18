//! Celo reth node binary.

use alloy_celo_evm::blocklist::FeeCurrencyBlocklist;
use celo_reth::{
    chainspec::CeloChainSpecParser,
    node::{CeloNode, RollupArgs},
    payload::{DEFAULT_FEE_CURRENCY_LIMIT_FRACTION, FeeCurrencyLimits},
    rpc::{
        celo_admin_module, celo_fee_history_module, celo_gas_price_module, celo_tx_module,
        make_celo_fee_api,
    },
    state_import::ImportCeloStateCommand,
};
use clap::Parser;
use futures_util::FutureExt;
use reth_cli_runner::CliRunner;
use reth_db_api::database_metrics::DatabaseMetrics;
use reth_node_core::args::LogArgs;
use reth_optimism_cli::Cli;
use reth_optimism_exex::OpProofsExEx;
use reth_optimism_rpc::eth::proofs::{EthApiExt, EthApiOverrideServer};
use reth_optimism_trie::{OpProofsStorage, db::MdbxProofsStorage};
use reth_tasks::TaskExecutor;
use reth_tracing::Layers;
use std::{ffi::OsString, sync::Arc, time::Duration};
use tokio::time::sleep;
use tracing::info;

/// Subcommand name for the Celo state import.
const IMPORT_CELO_STATE: &str = "import-celo-state";

/// Top-level Celo-only subcommand wrapper.
///
/// Used only when intercepting `import-celo-state` from the binary's argv before handing the
/// rest of the CLI off to the upstream op-reth `Cli`.
#[derive(Debug, Parser)]
#[command(name = "celo-reth")]
struct CeloCli {
    #[command(subcommand)]
    command: CeloCommand,

    #[command(flatten)]
    logs: LogArgs,
}

#[derive(Debug, clap::Subcommand)]
enum CeloCommand {
    /// Initialize a Celo Mainnet datadir from an L1 state dump.
    #[command(name = IMPORT_CELO_STATE)]
    ImportCeloState(Box<ImportCeloStateCommand>),
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

    // Intercept `import-celo-state` before handing argv off to the upstream op-reth `Cli`,
    // whose `Commands` enum we cannot extend. The subcommand must be `argv[1]` — global flags
    // before it (e.g. `celo-reth -v import-celo-state ...`) are not supported and the
    // subcommand is hidden from `celo-reth --help`. To lift either limitation we'd have to
    // mirror op-reth's full `Commands` enum in this crate.
    let argv: Vec<OsString> = std::env::args_os().collect();
    if argv.get(1).is_some_and(|a| a == IMPORT_CELO_STATE) {
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

            // Snapshot the historical-proofs fields before we move rollup_args
            // into CeloNode::new. Mirrors the OP launcher pattern in
            // ethereum-optimism/optimism @ kona-client/v1.2.13:
            //   rust/op-reth/crates/node/src/proof_history.rs
            let RollupArgs {
                proofs_history,
                proofs_history_window,
                proofs_history_prune_interval,
                proofs_history_verification_interval,
                ..
            } = rollup_args.clone();
            let proofs_history_storage_path = rollup_args.proofs_history_storage_path.clone();

            let blocklist = FeeCurrencyBlocklist::default();
            let mut node_builder = builder.node(
                CeloNode::new(rollup_args)
                    .with_blocklist(blocklist.clone())
                    .with_fee_currency_limits(fee_currency_limits),
            );

            // Optional: historical-proofs ExEx. When --proofs-history is set,
            // op-reth's bounded-history sidecar maintains a separate MDBX DB
            // with pre-computed trie data for fast eth_getProof at depth.
            // The CLI flags are inherited from reth_optimism_node::RollupArgs
            // but the wiring is per-binary: each downstream (OpNode, CeloNode)
            // must install the ExEx itself. We do that here.
            //
            // NOTE: on_node_started + install_exex are installed here, but the
            // RPC override is deferred to the single consolidated extend_rpc_modules
            // closure below. reth's builder API treats extend_rpc_modules as a
            // single-slot set/replace (Box<dyn ExtendRpcModules>), so calling it
            // twice silently discards the first hook — which is exactly the bug
            // that shipped in the initial PR #175 (the proofs-history override
            // was overwritten by the celo modules override, and eth_getProof
            // always fell back to the slow historical-state path).
            let proofs_storage_rpc: Option<OpProofsStorage<Arc<MdbxProofsStorage>>> =
                if proofs_history {
                    let path = proofs_history_storage_path.ok_or_else(|| {
                        eyre::eyre!(
                            "--proofs-history.storage-path is required when --proofs-history is set"
                        )
                    })?;
                    info!(target: "reth::cli", "Using on-disk storage for proofs history");

                    let mdbx = Arc::new(
                        MdbxProofsStorage::new(&path)
                            .map_err(|e| eyre::eyre!("Failed to create MdbxProofsStorage: {e}"))?,
                    );
                    let storage: OpProofsStorage<Arc<MdbxProofsStorage>> = mdbx.clone().into();
                    let storage_exec = storage.clone();

                    node_builder = node_builder
                        .on_node_started(move |node| {
                            spawn_proofs_db_metrics(
                                node.task_executor,
                                mdbx,
                                node.config.metrics.push_gateway_interval,
                            );
                            Ok(())
                        })
                        .install_exex("proofs-history", async move |exex_context| {
                            Ok(OpProofsExEx::builder(exex_context, storage_exec)
                                .with_proofs_history_window(proofs_history_window)
                                .with_proofs_history_prune_interval(proofs_history_prune_interval)
                                .with_verification_interval(proofs_history_verification_interval)
                                .build()
                                .run()
                                .boxed())
                        });

                    Some(storage)
                } else {
                    None
                };

            // Single consolidated extend_rpc_modules. Installs:
            //   1. proofs-history EthApiExt (overrides eth_getProof) — only when enabled
            //   2. Celo gas / fee-history / tx / admin modules — always
            //
            // TODO: also install DebugApiExt so debug_executePayload is served from
            // the sidecar (mirrors the OP launcher in ethereum-optimism/optimism @
            // kona-client/v1.2.13: rust/op-reth/crates/node/src/proof_history.rs).
            // First attempt at porting hit a generic-bounds mismatch on
            // DebugApiExt::into_rpc when instantiated with CeloNode's component
            // types (5 generic params on this side vs 4 in OP). Left for follow-up;
            // the eth_getProof override is the load-bearing one for archive-RPC use.
            let handle = node_builder
                .extend_rpc_modules(move |ctx| {
                    // 1. proofs-history eth_getProof override (if enabled).
                    if let Some(storage_rpc) = proofs_storage_rpc {
                        info!(
                            target: "reth::cli",
                            "Installing proofs-history RPC override (eth_getProof)"
                        );
                        let api_ext = EthApiExt::new(ctx.registry.eth_api().clone(), storage_rpc);
                        let eth_replaced = ctx.modules.replace_configured(api_ext.into_rpc())?;
                        info!(
                            target: "reth::cli",
                            eth_replaced,
                            "Proofs-history eth_getProof override installed"
                        );
                    }

                    // 2. Celo modules.
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
/// Initializes tracing standalone (without the OTLP layer the upstream `CliApp` adds — OTLP is
/// of no use for an offline migration), then runs the parsed command on a tokio runtime.
fn run_celo_subcommand(argv: Vec<OsString>) -> eyre::Result<()> {
    let cli = CeloCli::parse_from(argv);
    let _guard = cli.logs.init_tracing_with_layers(Layers::new())?;

    let runner = CliRunner::try_default_runtime()?;
    runner.run_blocking_until_ctrl_c(async move {
        match cli.command {
            CeloCommand::ImportCeloState(cmd) => cmd.execute().await,
        }
    })
}

/// Spawns a task that periodically reports metrics for the proofs DB.
///
/// Ported verbatim from ethereum-optimism/optimism @ kona-client/v1.2.13:
///   rust/op-reth/crates/node/src/proof_history.rs
fn spawn_proofs_db_metrics(
    executor: TaskExecutor,
    storage: Arc<MdbxProofsStorage>,
    metrics_report_interval: Duration,
) {
    executor.spawn_critical_task("op-proofs-storage-metrics", async move {
        info!(
            target: "reth::cli",
            ?metrics_report_interval,
            "Starting op-proofs-storage metrics task"
        );

        loop {
            sleep(metrics_report_interval).await;
            storage.report_metrics();
        }
    });
}
