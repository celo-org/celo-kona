#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![no_std]

extern crate alloc;

#[macro_use]
extern crate tracing;

mod attributes;
pub use attributes::CeloStatefulAttributesBuilder;

pub mod executor;

pub mod boot;
pub use boot::CeloBootInfo;

pub mod l2;
pub use l2::CeloOracleL2ChainProvider;

pub mod pipeline;
pub use pipeline::CeloOraclePipeline;

pub mod sync;
pub use sync::new_oracle_pipeline_cursor;
