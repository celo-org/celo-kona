//! Celo chain spec parsing and Celo-specific hardfork variants.
//!
//! Wraps [`reth_optimism_chainspec::OpChainSpec`] so the embedded `--chain celo`
//! and `--chain celo-sepolia` paths produce a hardfork list that includes
//! `Gingerbread` and `Cel2` — both required for the `eth/68` ForkID to match
//! op-geth peers.
//!
//! The parser is std-only; the hardfork variants and the Upgrade 18 extractors are
//! no-std so `CeloEvmConfig`'s constructors can derive the Upgrade 18 wiring from
//! the chain spec directly.

mod hardfork;
#[cfg(feature = "std")]
mod parser;

pub use hardfork::{CeloHardfork, upgrade18_overrides, upgrade18_time};
#[cfg(feature = "std")]
pub use parser::CeloChainSpecParser;
