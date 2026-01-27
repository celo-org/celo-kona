//! Transaction types for Celo.

pub mod cip64;
pub mod envelope;
mod tx_type;
mod typed;

pub use cip64::TxCip64;
pub use envelope::{CeloTxEnvelope, CeloTxType, CeloTypedTransaction};

#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub use envelope::serde_bincode_compat as envelope_serde_bincode_compat;

/// Bincode-compatible serde implementations for transaction types.
#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub mod serde_bincode_compat {
    pub use super::{cip64::serde_bincode_compat::TxCip64, envelope::serde_bincode_compat::*};
}
