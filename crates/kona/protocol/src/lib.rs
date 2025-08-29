#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod batch;
pub use batch::{CeloBatchValidationProvider, CeloToOpProviderAdapter};

mod derive;
pub use derive::{CeloBatchValidationProviderDerive, CeloL2ChainProvider};

mod block;
pub use block::CeloL2BlockInfo;

mod celo_to_op;
pub use celo_to_op::{convert_celo_block_to_op_block, convert_celo_txs_to_op_txs};
