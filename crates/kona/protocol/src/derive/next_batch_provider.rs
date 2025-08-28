use crate::batch::CeloBatch;
use alloc::boxed::Box;
use async_trait::async_trait;
use kona_derive::types::PipelineResult;
use kona_protocol::{BlockInfo, L2BlockInfo};

#[async_trait]
pub trait CeloNextBatchProvider {
    async fn next_batch(
        &mut self,
        parent: L2BlockInfo,
        l1_origins: &[BlockInfo],
    ) -> PipelineResult<CeloBatch>;

    fn span_buffer_size(&self) -> usize;

    fn flush(&mut self);
}
