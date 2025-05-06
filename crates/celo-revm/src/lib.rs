//! Celo-specific constants, types, and helpers.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc as std;

pub mod chain_info;
pub mod context;
pub mod evm;
pub mod precompiles;
pub mod transfer;

pub use context::CeloTxEnv;
pub use evm::CeloEvm;
