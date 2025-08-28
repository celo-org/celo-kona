//! Module containing the [CeloBatchWithInclusionBlock] struct.
use crate::{CeloBatchValidationProvider, batch::CeloBatch};
use kona_genesis::RollupConfig;
use kona_proof::errors::OracleProviderError;
use kona_protocol::{BatchValidity, BlockInfo, L2BlockInfo};

/// A batch with its inclusion block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CeloBatchWithInclusionBlock {
    /// The inclusion block
    pub inclusion_block: BlockInfo,
    /// The batch
    pub batch: CeloBatch,
}

impl CeloBatchWithInclusionBlock {
    /// Creates a new batch with inclusion block.
    pub const fn new(inclusion_block: BlockInfo, batch: CeloBatch) -> Self {
        Self { inclusion_block, batch }
    }

    pub async fn check_batch<BF>(
        &self,
        cfg: &RollupConfig,
        l1_blocks: &[BlockInfo],
        l2_safe_head: L2BlockInfo,
        fetcher: BF,
    ) -> BatchValidity
    where
        BF: CeloBatchValidationProvider + Send,
        OracleProviderError: From<<BF as CeloBatchValidationProvider>::Error>,
    {
        match &self.batch {
            CeloBatch::Single(single_batch) => {
                single_batch.check_batch(cfg, l1_blocks, l2_safe_head, &self.inclusion_block)
            }
            CeloBatch::Span(span_batch) => {
                span_batch
                    .check_batch(cfg, l1_blocks, l2_safe_head, &self.inclusion_block, fetcher)
                    .await
            }
        }
    }
}
