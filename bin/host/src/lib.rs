#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod backend;

pub mod eigenda;
pub use eigenda::CeloCanoeVerifierAddressFetcher;

pub mod eth;

#[cfg(feature = "single")]
pub mod single;
