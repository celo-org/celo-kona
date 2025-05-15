//! Celo-specific constants, types, and helpers.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc as std;

pub mod api;
pub mod chain_info;
pub mod evm;
pub mod handler;
pub mod precompiles;
pub mod receipt;
pub mod transaction;
pub mod tx;

pub use api::{
    builder::CeloBuilder,
    celo_block_env::CeloBlockEnv,
    default_ctx::{CeloContext, DefaultCelo},
};
pub use evm::CeloEvm;
pub use precompiles::CeloPrecompiles;
pub use receipt::CeloReceiptEnvelope;
pub use transaction::{
    CIP64_TRANSACTION_TYPE, CeloTransaction, CeloTxEnvelope, CeloTxType, CeloTypedTransaction,
    cip64::TxCip64,
};

/// Bincode-compatible serde implementations for consensus types.
///
/// `bincode` crate doesn't work well with optionally serializable serde fields, but some of the
/// consensus types require optional serialization for RPC compatibility. This module makes so that
/// all fields are serialized.
///
/// Read more: <https://github.com/bincode-org/bincode/issues/326>
#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub mod serde_bincode_compat {
    pub use crate::transaction::{
        cip64::serde_bincode_compat::TxCip64, serde_bincode_compat as transaction,
    };
}
