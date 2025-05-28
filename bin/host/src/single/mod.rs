//! This module contains the single-chain mode for the host.

mod cfg;
pub use cfg::{CeloSingleChainHost, CeloSingleChainProviders};

mod handler;
pub use handler::CeloSingleChainHintHandler;
