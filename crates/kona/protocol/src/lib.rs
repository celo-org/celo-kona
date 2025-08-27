#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod block;
pub use block::CeloL2BlockInfo;

mod batch;
pub use batch::CeloBatchValidationProvider;

mod protocol;
pub use protocol::to_system_config;
