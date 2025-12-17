#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![no_std]

extern crate alloc;

pub mod executor;

mod l2;
pub use l2::CeloOracleL2ChainProvider;
