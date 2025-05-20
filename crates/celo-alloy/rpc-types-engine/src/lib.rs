#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(test)]
extern crate alloc;

pub use alloy_rpc_types_engine::ForkchoiceUpdateVersion;

mod attributes;
pub use attributes::CeloPayloadAttributes;
