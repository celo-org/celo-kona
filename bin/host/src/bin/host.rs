//! Main entrypoint for the host binary.

#![warn(missing_debug_implementations, missing_docs, unreachable_pub, rustdoc::all)]
#![deny(unused_must_use, rust_2018_idioms)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

use anyhow::Result;
use clap::{Parser, Subcommand};
use kona_cli::{LogArgs, LogConfig, cli_styles};
use serde::Serialize;
use tracing::info;
use tracing_subscriber::EnvFilter;

const ABOUT: &str = "
celo-host is a CLI application that runs the Celo-Kona pre-image server and client program. The host
can run in two modes: server mode and native mode. In server mode, the host runs the pre-image
server and waits for the client program in the parent process to request pre-images. In native
mode, the host runs the client program in a separate thread with the pre-image server in the
primary thread.
";

/// The host binary CLI application arguments.
#[derive(Parser, Serialize, Clone, Debug)]
#[command(about = ABOUT, version, styles = cli_styles())]
pub struct HostCli {
    /// Logging arguments.
    #[command(flatten)]
    pub log_args: LogArgs,
    /// Host mode
    #[command(subcommand)]
    pub mode: HostMode,
}

/// Operation modes for the host binary.
#[derive(Subcommand, Serialize, Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum HostMode {
    /// Run the host in single-chain mode.
    #[cfg(feature = "single")]
    Single(celo_host::single::CeloSingleChainHost),
}

#[allow(unreachable_code, unused_variables)]
fn main() -> Result<()> {
    let cfg = HostCli::parse();
    LogConfig::new(cfg.log_args).init_tracing_subscriber(None::<EnvFilter>)?;

    let use_limited_runtime = true;

    let runtime = if use_limited_runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .thread_stack_size(1024 * 1024)
            .enable_all()
            .build()?
    } else {
        // Default multi-threaded runtime
        tokio::runtime::Builder::new_multi_thread().enable_all().build()?
    };

    runtime.block_on(async {
        match cfg.mode {
            #[cfg(feature = "single")]
            HostMode::Single(cfg) => {
                cfg.start().await?;
            }
        }

        info!("Exiting host program.");
        Ok(())
    })
}
