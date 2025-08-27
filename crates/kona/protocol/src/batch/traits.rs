//! Traits for working with protocol types.

use alloc::boxed::Box;
use async_trait::async_trait;
use celo_alloy_consensus::CeloBlock;
use kona_proof::errors::OracleProviderError;
use kona_protocol::L2BlockInfo;

/// Describes the functionality of a data source that fetches safe blocks.
#[async_trait]
pub trait CeloBatchValidationProvider {
    /// Returns the [L2BlockInfo] given a block number.
    ///
    /// Errors if the block does not exist.
    async fn l2_block_info_by_number(
        &mut self,
        number: u64,
    ) -> Result<L2BlockInfo, OracleProviderError>;

    /// Returns the [CeloBlock] for a given number.
    ///
    /// Errors if no block is available for the given block number.
    async fn block_by_number(&mut self, number: u64) -> Result<CeloBlock, OracleProviderError>;
}
