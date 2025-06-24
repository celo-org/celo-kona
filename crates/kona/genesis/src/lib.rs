#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

/// An alias for the hardfork configuration.
#[deprecated(note = "Use `CeloHardForkConfig` instead")]
pub type CeloHardForkConfiguration = CeloHardForkConfig;

mod chain;
pub use chain::CeloHardForkConfig;

mod rollup;
pub use rollup::CeloRollupConfig;
