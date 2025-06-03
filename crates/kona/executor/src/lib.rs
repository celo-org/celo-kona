#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "test-utils"), no_std)]

extern crate alloc;

#[macro_use]
extern crate tracing;

mod builder;
pub use builder::{CeloBlockBuildingOutcome, CeloStatelessL2Builder, compute_receipts_root};

pub(crate) mod util;

pub(crate) mod constants;

#[cfg(feature = "test-utils")]
pub mod test_utils;
