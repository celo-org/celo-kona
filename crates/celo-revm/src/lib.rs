#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc as std;

// Silence unused dependency warning - spin is only used in no_std environments
use spin as _;

pub mod api;
pub mod common;
pub mod constants;
pub mod contracts;
pub mod evm;
pub mod handler;
pub mod precompiles;
pub mod transaction;
pub mod tx;

pub use api::{
    CeloSystemCallEvmExt,
    builder::CeloBuilder,
    celo_block_env::CeloBlockEnv,
    default_ctx::{CeloContext, DefaultCelo},
};
pub use evm::CeloEvm;
pub use precompiles::CeloPrecompiles;
pub use transaction::CeloTransaction;
