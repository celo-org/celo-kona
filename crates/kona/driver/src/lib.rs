#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod executor;
pub use executor::CeloExecutorTr;
