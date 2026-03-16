//! Celo reth node binary.

use celo_reth::{
    node::{CeloNode, RollupArgs},
    rpc::{celo_gas_price_module, make_celo_fee_api},
};
use clap::Parser;
use reth_optimism_cli::{Cli, chainspec::OpChainSpecParser};

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

fn main() {
    reth_cli_util::sigsegv_handler::install();

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe {
            std::env::set_var("RUST_BACKTRACE", "1");
        }
    }

    if let Err(err) =
        Cli::<OpChainSpecParser, RollupArgs>::parse().run(async move |builder, rollup_args| {
            let handle = builder
                .node(CeloNode::new(rollup_args))
                .extend_rpc_modules(|ctx| {
                    let fee_api = make_celo_fee_api(ctx.registry.eth_api().clone());
                    let module = celo_gas_price_module(fee_api);
                    ctx.modules.replace_configured(module)?;
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
