#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

#[allow(unused_extern_crates)]
extern crate alloc;

mod rollup;
pub use rollup::CeloRollupConfig;
