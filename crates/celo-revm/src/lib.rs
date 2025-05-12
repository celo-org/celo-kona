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

pub use api::{
    builder::CeloBuilder,
    default_ctx::{CeloContext, DefaultCelo},
};
pub use evm::CeloEvm;
pub use precompiles::CeloPrecompiles;
pub use transaction::{
    CeloPooledTransaction, CeloTransaction, CeloTxEnvelope, CeloTypedTransaction, cip64::TxCip64,
};

#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub mod serde_bincode_compat {
    pub use crate::transaction::{
        cip64::serde_bincode_compat::TxCip64, serde_bincode_compat as transaction,
    };
}
