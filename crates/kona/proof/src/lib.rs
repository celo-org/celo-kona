#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![no_std]

extern crate alloc;

#[macro_use]
extern crate tracing;

pub mod executor;

pub mod boot;
pub use boot::CeloBootInfo;

mod l2;
pub use l2::CeloOracleL2ChainProvider;

// mod pipeline;
// pub use pipeline::CeloOraclePipeline;
