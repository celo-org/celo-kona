//! Adapter for using CeloBatchValidationProvider with BatchValidationProvider interface.
//!
//! This adapter allows Celo-specific batch validation providers to be used with
//! existing Kona code that expects the original BatchValidationProvider interface.
//! It handles the necessary type conversions between Celo and Kona types.

use crate::{CeloBatchValidationProvider, convert_celo_block_to_op_block};
use alloc::boxed::Box;
use async_trait::async_trait;
use kona_protocol::{BatchValidationProvider, L2BlockInfo};
use op_alloy_consensus::OpBlock;

/// Newtype wrapper that adapts CeloBatchValidationProvider to BatchValidationProvider.
///
/// NOTE: When converting blocks, the adapter automatically filters out Celo-specific transaction
/// types that are not supported in the Optimism protocol
#[derive(Debug, Clone)]
pub struct CeloBatchValidationProviderAdapter<T: CeloBatchValidationProvider>(pub T);

#[async_trait]
impl<T> BatchValidationProvider for CeloBatchValidationProviderAdapter<T>
where
    T: CeloBatchValidationProvider + Send,
{
    /// The error type is the same as the underlying provider
    type Error = T::Error;

    /// Fetches L2 block info by converting CeloL2BlockInfo to L2BlockInfo.
    async fn l2_block_info_by_number(&mut self, number: u64) -> Result<L2BlockInfo, Self::Error> {
        self.0.l2_block_info_by_number(number).await
    }

    /// Fetches block by converting CeloBlock to OpBlock.
    /// The returned block may contain fewer transactions than the original CeloBlock
    /// due to TxCip64 filtering
    async fn block_by_number(&mut self, number: u64) -> Result<OpBlock, Self::Error> {
        self.0.block_by_number(number).await.map(convert_celo_block_to_op_block)
    }
}
