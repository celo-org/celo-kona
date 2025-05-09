//! Celo-specific constants, types, and helpers.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc as std;

pub mod api;
pub mod chain_info;
pub mod constants;
pub mod evm;
pub mod precompiles;
pub mod transaction;
pub mod tx;

pub use evm::CeloEvm;
pub use precompiles::CeloPrecompiles;
pub use transaction::cip64::TxCip64;
pub use tx::CeloTxEnv;

#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub mod serde_bincode_compat {
    pub use crate::transaction::cip64::serde_bincode_compat::*;
}
