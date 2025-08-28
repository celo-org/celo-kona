#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod batch;
pub use batch::{
    CeloBatch, CeloBatchValidationProvider, CeloBatchValidationProviderAdapter,
    CeloBatchWithInclusionBlock, CeloSpanBatch,
};

mod derive;
pub use derive::{
    CeloBatchProvider, CeloBatchQueue, CeloBatchStream, CeloBatchValidationProviderDerive,
    CeloBatchValidator, CeloL2ChainProvider, CeloNextBatchProvider, CeloStatefulAttributesBuilder,
};

mod block;
pub use block::CeloL2BlockInfo;

mod convert;
pub use convert::{convert_celo_block_to_op_block, convert_celo_txs_to_op_txs};
