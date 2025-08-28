use core::ops::{Deref, DerefMut};

use crate::{CeloBatchValidationProvider, CeloL2ChainAdapter};
use alloc::vec::Vec;
use alloy_primitives::FixedBytes;
use kona_genesis::RollupConfig;
use kona_proof::errors::OracleProviderError;
use kona_protocol::{
    BatchValidity, BlockInfo, L2BlockInfo, RawSpanBatch, SingleBatch, SpanBatch, SpanBatchElement,
    SpanBatchError,
};

// TODO: Write comment for description of CeloSpanBatch
// TODO: Not need?
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CeloSpanBatch {
    pub inner: SpanBatch,
}

impl CeloSpanBatch {
    /// Returns the starting timestamp for the first batch in the span.
    pub fn starting_timestamp(&self) -> u64 {
        self.inner.starting_timestamp()
    }

    /// Returns the final timestamp for the last batch in the span.
    pub fn final_timestamp(&self) -> u64 {
        self.inner.final_timestamp()
    }

    /// Returns the L1 epoch number for the first batch in the span.
    pub fn starting_epoch_num(&self) -> u64 {
        self.inner.starting_epoch_num()
    }

    /// Validates that the L1 origin hash matches the span's L1 origin check.
    pub fn check_origin_hash(&self, hash: FixedBytes<32>) -> bool {
        self.inner.check_origin_hash(hash)
    }

    /// Validates that the parent hash matches the span's parent check.
    pub fn check_parent_hash(&self, hash: FixedBytes<32>) -> bool {
        self.inner.check_parent_hash(hash)
    }

    /// Accesses the nth element from the end of the batch list.
    fn peek(&self, n: usize) -> &SpanBatchElement {
        &self.inner.batches[self.inner.batches.len() - 1 - n]
    }

    /// Converts this span batch to its raw serializable format.
    pub fn to_raw_span_batch(&self) -> Result<RawSpanBatch, SpanBatchError> {
        self.inner.to_raw_span_batch()
    }

    /// Converts all [`SpanBatchElement`]s after the L2 safe head to [`SingleBatch`]es. The
    /// resulting [`SingleBatch`]es do not contain a parent hash, as it is populated by the
    /// Batch Queue stage.
    pub fn get_singular_batches(
        &self,
        l1_origins: &[BlockInfo],
        l2_safe_head: L2BlockInfo,
    ) -> Result<Vec<SingleBatch>, SpanBatchError> {
        self.inner.get_singular_batches(l1_origins, l2_safe_head)
    }

    /// Append a [`SingleBatch`] to the [`SpanBatch`]. Updates the L1 origin check if need be.
    pub fn append_singular_batch(
        &mut self,
        singular_batch: SingleBatch,
        seq_num: u64,
    ) -> Result<(), SpanBatchError> {
        self.inner.append_singular_batch(singular_batch, seq_num)
    }

    /// Checks if the span batch is valid.
    pub async fn check_batch<BV>(
        &self,
        cfg: &RollupConfig,
        l1_blocks: &[BlockInfo],
        l2_safe_head: L2BlockInfo,
        inclusion_block: &BlockInfo,
        fetcher: BV,
    ) -> BatchValidity
    where
        BV: CeloBatchValidationProvider + Send,
        OracleProviderError: From<<BV as CeloBatchValidationProvider>::Error>,
    {
        self.inner
            .check_batch(
                cfg,
                l1_blocks,
                l2_safe_head,
                inclusion_block,
                &mut CeloL2ChainAdapter(fetcher),
            )
            .await
    }

    /// Checks the validity of the batch's prefix.
    pub async fn check_batch_prefix<BF>(
        &self,
        cfg: &RollupConfig,
        l1_origins: &[BlockInfo],
        l2_safe_head: L2BlockInfo,
        inclusion_block: &BlockInfo,
        fetcher: BF,
    ) -> (BatchValidity, Option<L2BlockInfo>)
    where
        BF: CeloBatchValidationProvider + Send,
        OracleProviderError: From<<BF as CeloBatchValidationProvider>::Error>,
    {
        self.inner
            .check_batch_prefix(
                cfg,
                l1_origins,
                l2_safe_head,
                inclusion_block,
                &mut CeloL2ChainAdapter(fetcher),
            )
            .await
    }
}

impl Deref for CeloSpanBatch {
    type Target = SpanBatch;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for CeloSpanBatch {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
