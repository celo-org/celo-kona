//! Receipt types for Celo.
mod envelope;
pub use envelope::CeloReceiptEnvelope;

pub(crate) mod receipts;
pub use receipts::{CeloCip64Receipt, CeloCip64ReceiptWithBloom};
