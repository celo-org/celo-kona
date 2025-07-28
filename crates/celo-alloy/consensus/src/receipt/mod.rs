//! Receipt types for Celo.

use alloy_consensus::TxReceipt;
use alloy_primitives::U256;

mod envelope;
pub use envelope::CeloReceiptEnvelope;

pub(crate) mod receipts;
pub use receipts::{CeloCip64Receipt, CeloCip64ReceiptWithBloom};

/// Receipt is the result of a transaction execution.
pub trait CeloTxReceipt: TxReceipt {
    /// Returns the deposit nonce of the transaction.
    fn base_fee(&self) -> Option<u128>;
}
