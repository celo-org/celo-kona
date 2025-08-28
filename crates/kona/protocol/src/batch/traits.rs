//! Traits for working with protocol types.

use alloc::boxed::Box;
use async_trait::async_trait;
use celo_alloy_consensus::CeloBlock;
use core::fmt::Display;
use kona_protocol::L2BlockInfo;

/// A trait for a data source that fetches safe blocks from Celo
///
/// This trait is cloned from Kona's [`BatchValidationProvider`] and modified to return
/// CeloBlock types instead of the original block types
#[async_trait]
pub trait CeloBatchValidationProvider {
    /// The error type for the [`CeloBatchValidationProvider`].
    type Error: Display;

    /// Returns the [`CeloL2BlockInfo`] given a block number.
    ///
    /// Errors if the block does not exist.
    async fn l2_block_info_by_number(&mut self, number: u64) -> Result<L2BlockInfo, Self::Error>;

    /// Returns the [`CeloBlock`] for a given number.
    ///
    /// Errors if no block is available for the given block number.
    async fn block_by_number(&mut self, number: u64) -> Result<CeloBlock, Self::Error>;
}
