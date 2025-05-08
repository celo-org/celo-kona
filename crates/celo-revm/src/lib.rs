//! Celo-specific constants, types, and helpers.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc as std;

pub mod chain_info;
pub mod evm;
pub mod precompiles;
pub mod tx;

pub use evm::CeloEvm;
pub use precompiles::CeloPrecompiles;
pub use tx::CeloTxEnv;
