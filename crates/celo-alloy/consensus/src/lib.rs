#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc as std;

mod receipt;
pub use receipt::CeloReceiptEnvelope;

pub mod transaction;
pub use transaction::{CeloTxEnvelope, CeloTxType, CeloTypedTransaction, cip64::TxCip64};

mod block;
pub use block::CeloBlock;

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
