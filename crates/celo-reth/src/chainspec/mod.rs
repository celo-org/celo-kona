//! Celo chain spec parsing and Celo-specific hardfork variants.
//!
//! Wraps [`reth_optimism_chainspec::OpChainSpec`] so the embedded `--chain celo`
//! and `--chain celo-sepolia` paths produce a hardfork list that includes
//! `Gingerbread` and `Cel2` — both required for the `eth/68` ForkID to match
//! op-geth peers.

mod hardfork;
mod parser;

pub use hardfork::{CeloHardfork, upgrade18_time};
pub use parser::CeloChainSpecParser;
