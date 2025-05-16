pub mod abstraction;
pub mod cip64;
pub mod envelope;
pub mod tx_type;
pub mod typed;

pub use abstraction::{CeloTransaction, CeloTxTr};
pub use envelope::CeloTxEnvelope;
pub use tx_type::{CIP64_TRANSACTION_TYPE, CeloTxType};
pub use typed::CeloTypedTransaction;

#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub use envelope::serde_bincode_compat as envelope_serde_bincode_compat;

/// Bincode-compatible serde implementations for transaction types.
#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub mod serde_bincode_compat {
    pub use super::{cip64::serde_bincode_compat::TxCip64, envelope::serde_bincode_compat::*};
}
