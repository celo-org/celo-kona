//! Celo reth node binary.

use alloy_celo_evm::blocklist::FeeCurrencyBlocklist;
use celo_reth::{
    node::{CeloNode, RollupArgs},
    payload::FeeCurrencyLimits,
    rpc::{celo_admin_module, celo_fee_history_module, celo_gas_price_module, make_celo_fee_api},
};
use clap::Parser;
use reth_optimism_cli::{Cli, chainspec::OpChainSpecParser};

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
    #[arg(long = "celo.feecurrency.default", value_name = "FRACTION", default_value = "0.5")]
    pub fee_currency_default: f64,
}

fn main() {
    reth_cli_util::sigsegv_handler::install();

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe {
            std::env::set_var("RUST_BACKTRACE", "1");
        }
    }

    if let Err(err) =
        Cli::<OpChainSpecParser, CeloArgs>::parse().run(async move |builder, celo_args| {
            let rollup_args = celo_args.rollup;

            // Parse fee currency limits from CLI args.
            // If no explicit limits are provided and the chain is Celo Mainnet,
            // use the op-geth-matching defaults (cUSD/USDT/USDC=0.9, cEUR/cREAL=0.5).
            let chain_id = builder.config().chain.chain().id();
            let limits = match celo_args.fee_currency_limits.as_deref() {
                Some(s) => FeeCurrencyLimits::parse_limits(s),
                None if chain_id == celo_revm::constants::CELO_MAINNET_CHAIN_ID => {
                    FeeCurrencyLimits::mainnet_defaults()
                }
                None => Default::default(),
            };
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
