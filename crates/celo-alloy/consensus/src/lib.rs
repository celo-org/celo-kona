#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc as std;

mod receipt;
pub use receipt::{CeloCip64Receipt, CeloCip64ReceiptWithBloom, CeloReceiptEnvelope};

pub mod transaction;
pub use transaction::{
    CeloPooledTransaction, CeloTxEnvelope, CeloTxType, CeloTypedTransaction, cip64::TxCip64,
};

mod block;
pub use block::CeloBlock;

pub mod pre_gingerbread;
pub use pre_gingerbread::pre_gingerbread_header_hash;

#[cfg(feature = "reth")]
mod reth_compat;
