//! Celo reth node binary.

use alloy_celo_evm::blocklist::FeeCurrencyBlocklist;
use celo_reth::{
    CeloEvmConfig,
    celo_migrate_v2::CeloMigrateV2Command,
    chainspec::CeloChainSpecParser,
    node::{CeloConsensus, CeloNode, ProofsStorageVersion, RollupArgs},
    payload::{DEFAULT_FEE_CURRENCY_LIMIT_FRACTION, FeeCurrencyLimits},
    rpc::{
        celo_admin_module, celo_fee_history_module, celo_gas_price_module, celo_tx_module,
        make_celo_fee_api,
    },
    state_import::ImportCeloStateCommand,
};
use clap::{CommandFactory, FromArgMatches, Parser};
use futures_util::FutureExt;
use reth_chainspec::EthChainSpec;
use reth_cli_commands::{
    db,
    download::{DownloadCommand, manifest_cmd::SnapshotManifestCommand},
    p2p, prune, re_execute, stage,
};
use reth_cli_runner::CliRunner;
use reth_db::DatabaseEnv;
use reth_db_api::database_metrics::DatabaseMetrics;
use reth_node_builder::{NodeBuilder, WithLaunchContext};
use reth_node_core::{
    args::{LogArgs, OtlpInitStatus, OtlpLogsStatus, TraceArgs},
    version::{default_reth_version_metadata, try_init_version_metadata},
};
use reth_node_metrics::recorder::install_prometheus_recorder;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_cli::Cli;
use reth_optimism_exex::OpProofsExEx;
use reth_optimism_rpc::eth::proofs::{EthApiExt, EthApiOverrideServer};
use reth_optimism_trie::{
    OpProofsStorage, OpProofsStore,
    db::{MdbxProofsStorage, MdbxProofsStorageV2},
};
use reth_tasks::TaskExecutor;
use reth_tracing::{FileWorkerGuard, Layers};
use std::{ffi::OsString, sync::Arc, time::Duration};
use tokio::time::sleep;
use tracing::{info, warn};

/// Subcommand name for the Celo state import.
const IMPORT_CELO_STATE: &str = "import-celo-state";
const STAGE: &str = "stage";
const DB: &str = "db";
const P2P: &str = "p2p";
const PRUNE: &str = "prune";
const RE_EXECUTE: &str = "re-execute";
const DOWNLOAD: &str = "download";
const SNAPSHOT_MANIFEST: &str = "snapshot-manifest";
const CELO_MIGRATE_V2: &str = "celo-migrate-v2";

/// All Celo-intercepted subcommand names. Each one is dispatched in `run_celo_subcommand`
/// against `CeloNode` instead of letting op-reth's `Cli` route it to `OpNode`.
const CELO_SUBCOMMANDS: &[&str] = &[
    IMPORT_CELO_STATE,
    STAGE,
    DB,
    P2P,
    PRUNE,
    RE_EXECUTE,
    DOWNLOAD,
    SNAPSHOT_MANIFEST,
    CELO_MIGRATE_V2,
];

// TODO: `proofs unwind` is intentionally NOT intercepted: its upstream `execute<N>` binds
// `N::Primitives = OpPrimitives`, which `CeloNode` can't satisfy. It will panic on the
// first CIP-64 transaction it touches. See https://github.com/celo-org/celo-kona/issues/189
// for the port plan. `proofs init` and `proofs prune` are safe on the op-reth dispatch
// (no tx decoding).

/// Default snapshot manifest URL for `celo-reth download`. Overrides the upstream
/// `--manifest-url` default (which is empty, triggering interactive selection against
/// `snapshots.reth.rs` — an Ethereum-mainnet-only host).
const DEFAULT_MANIFEST_URL: &str =
    "https://fsn1.your-objectstorage.com/celo/snapshots/manifest.json";

/// Default chain ID embedded in `celo-reth snapshot-manifest` output. Overrides the upstream
/// `--chain-id` default of `1` (Ethereum mainnet).
const DEFAULT_CHAIN_ID: &str = "42220";

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
    /// Database debugging utilities.
    #[command(name = DB)]
    Db(db::Command<CeloChainSpecParser>),
    /// P2P debugging utilities.
    #[command(name = P2P)]
    P2P(Box<p2p::Command<CeloChainSpecParser>>),
    /// Prune according to the configuration without any limits.
    #[command(name = PRUNE)]
    Prune(prune::PruneCommand<CeloChainSpecParser>),
    /// Re-execute blocks in parallel to verify historical sync correctness.
    #[command(name = RE_EXECUTE)]
    ReExecute(re_execute::Command<CeloChainSpecParser>),
    /// Download a Celo reth snapshot. Defaults `--manifest-url` to the cLabs-operated
    /// publication endpoint; pass `--url`, `--manifest-url`, or `--manifest-path` to override.
    #[command(name = DOWNLOAD)]
    Download(Box<DownloadCommand<CeloChainSpecParser>>),
    /// Generate a chunked snapshot manifest from a local datadir (publisher tool).
    /// Defaults `--chain-id` to Celo Mainnet (42220).
    #[command(name = SNAPSHOT_MANIFEST)]
    SnapshotManifest(Box<SnapshotManifestCommand>),
    /// Celo-aware v1 → v2 storage migration (skips the upstream stage-reset that would
    /// force a pipeline rebuild over the dummy pre-migration blocks).
    #[command(name = CELO_MIGRATE_V2)]
    CeloMigrateV2(Box<CeloMigrateV2Command>),
}

impl CeloCommand {
    fn chain_spec(&self) -> Option<&Arc<OpChainSpec>> {
        match self {
            Self::ImportCeloState(_) => None,
            Self::Stage(command) => command.chain_spec(),
            Self::Db(command) => command.chain_spec(),
            Self::P2P(command) => command.chain_spec(),
            Self::Prune(command) => command.chain_spec(),
            Self::ReExecute(command) => command.chain_spec(),
            Self::Download(command) => command.chain_spec(),
            Self::SnapshotManifest(_) => None,
            Self::CeloMigrateV2(_) => None,
        }
    }
}

/// Build the Celo clap [`Command`] with Celo-specific argument defaults applied. The defaults
/// override upstream values that are Ethereum-mainnet-centric (chain id 1, snapshots.reth.rs).
fn celo_cli_command() -> clap::Command {
    CeloCli::command()
        .mut_subcommand(SNAPSHOT_MANIFEST, |sub| {
            sub.mut_arg("chain_id", |a| a.default_value(DEFAULT_CHAIN_ID))
        })
        .mut_subcommand(DOWNLOAD, |sub| {
            sub.mut_arg("manifest_url", |a| a.default_value(DEFAULT_MANIFEST_URL))
        })
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

/// Augment reth's version metadata with the celo-kona git SHA so `celo-reth --version` shows
/// both the upstream reth commit and the celo-kona commit that built the binary.
///
/// Must run before any clap command building that surfaces `--version`. The underlying global
/// is a [`OnceLock`], so the first call wins; we ignore the `Err` (already-set) case for safety
/// in tests or other entry points.
///
/// `CELO_KONA_GIT_SHA` / `CELO_KONA_GIT_SHA_LONG` are injected at compile time by
/// `crates/celo-reth/build.rs` from either the `CELO_KONA_GIT_SHA` build env (Docker build-arg
/// path) or `git rev-parse HEAD` (local cargo build path).
fn init_celo_version_metadata() {
    let mut meta = default_reth_version_metadata();
    let celo_sha = env!("CELO_KONA_GIT_SHA");
    let celo_sha_long = env!("CELO_KONA_GIT_SHA_LONG");
    // `short_version` is what reth's clap renders for `--version`. Keep reth's existing string
    // (which already includes reth's short SHA + build profile) and append the celo-kona SHA.
    meta.short_version = format!("{} / celo-kona {}", meta.short_version, celo_sha).into();
    // `long_version` is the multi-line output for `--version --verbose` style consumers.
    meta.long_version =
        format!("{}\nCelo-Kona Commit SHA: {}", meta.long_version, celo_sha_long).into();
    let _ = try_init_version_metadata(meta);
}

fn main() {
    init_celo_version_metadata();
    reth_cli_util::sigsegv_handler::install();

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe {
            std::env::set_var("RUST_BACKTRACE", "1");
        }
    }

    // Intercept Celo-specific command paths before handing argv off to the upstream op-reth `Cli`.
    // We try parsing with `CeloCli` first; if it succeeds we own the dispatch. Position-independent
    // global flags (`-v`, `--chain`, OTLP flags …) make plain `argv[1]` matching unsafe — e.g.
    // `celo-reth -v stage unwind --chain celo` would otherwise fall through to op-reth and run
    // `stage` against `OpNode` primitives, reintroducing the CIP-64 decode panic this path exists
    // to avoid. On parse failure we fall through to op-reth, except when a Celo subcommand sits in
    // the subcommand slot (where we surface clap's own help/version/error output instead of a
    // confusing "unrecognized subcommand" from op-reth).
    let argv: Vec<OsString> = std::env::args_os().collect();
    let parsed =
        celo_cli_command().try_get_matches_from(&argv).and_then(|m| CeloCli::from_arg_matches(&m));
    match parsed {
        Ok(cli) => {
            if let Err(err) = run_celo_subcommand(cli) {
                eprintln!("Error: {err:?}");
                std::process::exit(1);
            }
            return;
        }
        Err(e) if is_celo_subcommand_invocation(&argv) => e.exit(),
        Err(_) => { /* fall through to upstream `Cli` */ }
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
            // ethereum-optimism/optimism @ kona-node/v1.5.1:
            //   rust/op-reth/crates/node/src/proof_history.rs
            let RollupArgs {
                proofs_history,
                proofs_history_storage_path,
                proofs_history_window,
                proofs_history_verification_interval,
                proofs_history_storage_version,
                ..
            } = rollup_args.clone();

            let blocklist = FeeCurrencyBlocklist::default();
            let node = CeloNode::new(rollup_args)
                .with_blocklist(blocklist.clone())
                .with_fee_currency_limits(fee_currency_limits);

            // Historical-proofs ExEx (Bounded History Sidecar). When --proofs-history is
            // set, dispatch on the on-disk schema version (--proofs-history.storage-version)
            // and hand the matching MDBX store to `launch_celo_node`. When it is unset we
            // still go through `launch_celo_node` (with `None`), so the Celo RPC modules
            // are wired identically on every path — the `::<MdbxProofsStorage>` turbofish
            // there only pins the otherwise-unused store type parameter.
            if proofs_history {
                let path = proofs_history_storage_path.ok_or_else(|| {
                    eyre::eyre!(
                        "--proofs-history.storage-path is required when --proofs-history is set"
                    )
                })?;
                match proofs_history_storage_version {
                    ProofsStorageVersion::V1 => {
                        info!(target: "reth::cli", "Using on-disk storage for proofs history (v1)");
                        let mdbx =
                            Arc::new(MdbxProofsStorage::new(&path).map_err(|e| {
                                eyre::eyre!("Failed to create MdbxProofsStorage: {e}")
                            })?);
                        let proofs = ProofsHistoryConfig {
                            mdbx,
                            window: proofs_history_window,
                            verification_interval: proofs_history_verification_interval,
                        };
                        launch_celo_node(builder, node, blocklist, Some(proofs)).await
                    }
                    ProofsStorageVersion::V2 => {
                        info!(target: "reth::cli", "Using on-disk storage for proofs history (v2)");
                        let mdbx = Arc::new(MdbxProofsStorageV2::new(&path).map_err(|e| {
                            eyre::eyre!("Failed to create MdbxProofsStorageV2: {e}")
                        })?);
                        let proofs = ProofsHistoryConfig {
                            mdbx,
                            window: proofs_history_window,
                            verification_interval: proofs_history_verification_interval,
                        };
                        launch_celo_node(builder, node, blocklist, Some(proofs)).await
                    }
                }
            } else {
                launch_celo_node::<MdbxProofsStorage>(builder, node, blocklist, None).await
            }
        })
    {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}

/// Whether `argv` invokes one of the Celo-owned subcommands (routes `main` to `CeloCli`).
///
/// We can't just scan argv for a subcommand name: that would also match the name appearing as an
/// option *value* (e.g. `node --datadir db`), wrongly routing upstream commands to `CeloCli`.
/// Instead we do a lenient re-parse (`ignore_errors`) that lets clap skip unknown/invalid tokens
/// but still resolve the real subcommand position past any global flags, then check that slot.
///
/// Help/version are disabled on this routing-only parse so clap treats `--help`/`--version` as
/// ordinary (now-unknown) flags that `ignore_errors` skips, rather than short-circuiting with no
/// subcommand resolved. That keeps routing correct for `<celo-subcommand> --help` (→ `CeloCli`, so
/// it prints that command's help) while top-level `--help` still resolves no subcommand (→ op-reth,
/// which owns the full command surface). The real dispatch below uses the unmodified args, so the
/// chosen CLI still renders help/version normally.
///
/// Additionally accepts `help <celo-subcommand>` (clap's auto-generated form). clap's `help`
/// subcommand short-circuits parsing before `subcommand_matches` populates, so we can't detect it
/// via the sniff above — fall back to a direct positional check at argv[1..=2]. Kept narrow
/// (not a full walk skipping global flags) so that a global flag value happening to equal `help`
/// can't mis-route an upstream invocation.
fn is_celo_subcommand_invocation(argv: &[OsString]) -> bool {
    if CeloCli::command()
        .ignore_errors(true)
        .disable_help_flag(true)
        .disable_version_flag(true)
        .try_get_matches_from(argv)
        .ok()
        .and_then(|matches| matches.subcommand_name().map(str::to_owned))
        .is_some_and(|name| CELO_SUBCOMMANDS.contains(&name.as_str()))
    {
        return true;
    }
    matches!(
        (argv.get(1), argv.get(2)),
        (Some(first), Some(second))
            if first == "help" && CELO_SUBCOMMANDS.iter().any(|s| second == *s)
    )
}

/// Dispatch the Celo-specific subcommand path.
///
/// Mirrors upstream `CliApp::run`: scope log dir to the chain name, init tracing (OTLP layers
/// then file/stdout), install the global Prometheus recorder so `--metrics` exporters in
/// subcommands have something to record, then dispatch the command.
fn run_celo_subcommand(mut cli: CeloCli) -> eyre::Result<()> {
    if let Some(chain_spec) = cli.command.chain_spec() {
        cli.logs.log_file_directory =
            cli.logs.log_file_directory.join(chain_spec.chain().to_string());
    }

    let runner = CliRunner::try_default_runtime()?;
    let _guard = init_tracing(&runner, &mut cli.logs, &mut cli.traces)?;
    install_prometheus_recorder();

    let components = |spec: Arc<OpChainSpec>| {
        (CeloEvmConfig::celo(spec.clone()), Arc::new(CeloConsensus::new(spec)))
    };

    match cli.command {
        CeloCommand::ImportCeloState(cmd) => {
            let runtime = runner.runtime();
            runner.run_blocking_until_ctrl_c(cmd.execute(runtime))
        }
        CeloCommand::Stage(cmd) => {
            runner.run_command_until_exit(|ctx| cmd.execute::<CeloNode, _>(ctx, components))
        }
        CeloCommand::Db(cmd) => {
            runner.run_blocking_command_until_exit(|ctx| cmd.execute::<CeloNode>(ctx))
        }
        CeloCommand::P2P(cmd) => runner.run_until_ctrl_c(cmd.execute::<CeloNode>()),
        CeloCommand::Prune(cmd) => {
            runner.run_command_until_exit(|ctx| cmd.execute::<CeloNode>(ctx))
        }
        CeloCommand::ReExecute(cmd) => {
            let runtime = runner.runtime();
            runner.run_until_ctrl_c(cmd.execute::<CeloNode>(components, runtime))
        }
        // Download is chain-agnostic file shuttling; CeloNode satisfies the `N` type bound
        // without requiring Celo-aware decoding (archives are tarballs over MDBX + static files).
        CeloCommand::Download(cmd) => runner.run_until_ctrl_c(cmd.execute::<CeloNode>()),
        // Snapshot manifest generation is synchronous: it tars static files and reads the
        // `Finish` stage checkpoint from MDBX read-only. No runtime needed.
        CeloCommand::SnapshotManifest(cmd) => cmd.execute(),
        CeloCommand::CeloMigrateV2(cmd) => {
            let runtime = runner.runtime();
            runner.run_blocking_until_ctrl_c(cmd.execute(runtime))
        }
    }
}

/// Wire OTLP layers, init file/stdout tracing, then surface the OTLP init status as
/// `info!`/`warn!` so users can tell whether their `--otlp.*` flags actually took effect.
/// Mirrors upstream `CliApp::init_tracing` (rust/op-reth/crates/cli/src/app.rs).
fn init_tracing(
    runner: &CliRunner,
    logs: &mut LogArgs,
    traces: &mut TraceArgs,
) -> eyre::Result<Option<FileWorkerGuard>> {
    let mut layers = Layers::new();
    let otlp_status = runner.block_on(traces.init_otlp_tracing(&mut layers))?;
    let otlp_logs_status = runner.block_on(traces.init_otlp_logs(&mut layers))?;

    let guard = logs.init_tracing_with_layers(layers, false)?;
    info!(target: "reth::cli", "Initialized tracing, debug log directory: {}", logs.log_file_directory);

    match otlp_status {
        OtlpInitStatus::Started(endpoint) => {
            info!(target: "reth::cli", "Started OTLP {:?} tracing export to {endpoint}", traces.protocol);
        }
        OtlpInitStatus::NoFeature => {
            warn!(target: "reth::cli", "Provided OTLP tracing arguments do not have effect, compile with the `otlp` feature");
        }
        OtlpInitStatus::Disabled => {}
    }

    match otlp_logs_status {
        OtlpLogsStatus::Started(endpoint) => {
            info!(target: "reth::cli", "Started OTLP {:?} logs export to {endpoint}", traces.protocol);
        }
        OtlpLogsStatus::NoFeature => {
            warn!(target: "reth::cli", "Provided OTLP logs arguments do not have effect, compile with the `otlp-logs` feature");
        }
        OtlpLogsStatus::Disabled => {}
    }

    Ok(guard)
}

/// Configuration for the historical-proofs ExEx, generic over the on-disk store schema
/// (`MdbxProofsStorage` for v1, `MdbxProofsStorageV2` for v2).
struct ProofsHistoryConfig<S> {
    /// The MDBX-backed proofs store.
    mdbx: Arc<S>,
    /// Number of blocks of proof history to retain.
    window: u64,
    /// Trie re-verification cadence in blocks (`0` disables verification).
    verification_interval: u64,
}

/// Launch the Celo node, optionally installing the historical-proofs sidecar.
///
/// This is the single launch path for the binary. It always wires the Celo RPC modules
/// (gas price / fee history / tx / admin), and — when `proofs` is `Some` — additionally
/// installs the proofs-history ExEx, its metrics task, and the `eth_getProof` RPC
/// override, all bound to the given MDBX store `S` (v1 or v2).
///
/// reth's builder treats `extend_rpc_modules` as a single-slot set/replace, so calling it
/// twice silently discards the first hook — the bug that shipped in PR #175 (the
/// proofs-history override was overwritten by the Celo modules override, and `eth_getProof`
/// always fell back to the slow historical-state path). The proofs override and the Celo
/// modules therefore share one closure. Routing the no-proofs case through here too keeps
/// that closure — and the Celo module wiring — defined exactly once.
///
/// `on_node_started` and `install_exex` return `Self`, so installing the sidecar leaves the
/// builder type unchanged and the store type `S` stays erased behind the boxed hooks — that
/// is what lets the v1/v2 dispatch in `main` funnel into this one generic function.
///
/// TODO: also install `DebugApiExt` so `debug_executePayload` is served from the sidecar
/// (mirrors the OP launcher in ethereum-optimism/optimism @ kona-node/v1.5.1:
/// rust/op-reth/crates/node/src/proof_history.rs). First attempt at porting hit a
/// generic-bounds mismatch on `DebugApiExt::into_rpc` when instantiated with CeloNode's
/// component types (5 generic params on this side vs 4 in OP). Left for follow-up; the
/// `eth_getProof` override is the load-bearing one for archive-RPC use.
async fn launch_celo_node<S>(
    builder: WithLaunchContext<NodeBuilder<DatabaseEnv, OpChainSpec>>,
    node: CeloNode,
    blocklist: FeeCurrencyBlocklist,
    proofs: Option<ProofsHistoryConfig<S>>,
) -> eyre::Result<()>
where
    S: OpProofsStore + DatabaseMetrics + Send + Sync + 'static,
{
    let mut node_builder = builder.node(node);

    // When enabled, install the ExEx + metrics task and keep the storage handle for the
    // RPC override below.
    let proofs_storage_rpc: Option<OpProofsStorage<Arc<S>>> =
        if let Some(ProofsHistoryConfig { mdbx, window, verification_interval }) = proofs {
            let storage: OpProofsStorage<Arc<S>> = mdbx.clone().into();
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
                        .with_proofs_history_window(window)
                        .with_verification_interval(verification_interval)
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
            let fee_api = make_celo_fee_api(ctx.registry.eth_api().clone(), fee_currency_directory);
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
}

/// Spawns a task that periodically reports metrics for the proofs DB.
///
/// Ported from ethereum-optimism/optimism @ kona-node/v1.5.1:
///   rust/op-reth/crates/node/src/proof_history.rs
fn spawn_proofs_db_metrics<S>(
    executor: TaskExecutor,
    storage: Arc<S>,
    metrics_report_interval: Duration,
) where
    S: DatabaseMetrics + Send + Sync + 'static,
{
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
