//! Transaction types for Celo.

pub mod cip64;
pub mod envelope;
pub mod tx_type;
pub mod typed;

pub use envelope::CeloTxEnvelope;
pub use tx_type::CeloTxType;
pub use typed::CeloTypedTransaction;

pub mod tx_type_alt;
pub use tx_type_alt::CeloTxTypeAlt;

#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub use envelope::serde_bincode_compat as envelope_serde_bincode_compat;

/// Bincode-compatible serde implementations for transaction types.
#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub mod serde_bincode_compat {
    pub use super::{cip64::serde_bincode_compat::TxCip64, envelope::serde_bincode_compat::*};
}
