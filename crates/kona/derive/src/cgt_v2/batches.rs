//! Apply the no-user rule after singular/span batches converge to SingleBatch.

use super::CgtV2Origin;
use alloc::{boxed::Box, sync::Arc};
use alloy_eips::BlockNumHash;
use async_trait::async_trait;
use kona_derive::{
    AttributesProvider, ChainProvider, OriginAdvancer, OriginProvider, PipelineError,
    PipelineResult, Stage,
};
use kona_genesis::{RollupConfig, SystemConfig};
use kona_protocol::{BlockInfo, L2BlockInfo, SingleBatch};

/// Prevents batch transactions from entering the activation block.
#[derive(Debug)]
pub struct CgtV2BatchProvider<B, P> {
    /// The standard provider, including its span/channel buffers.
    pub inner: B,
    pub(super) origin: CgtV2Origin<P>,
    pub(super) pending: Option<SingleBatch>,
    last_in_span: bool,
}
impl<B, P> CgtV2BatchProvider<B, P> {
    /// Wrap the shared post-decomposition batch stage.
    pub const fn new(cfg: Arc<RollupConfig>, inner: B, provider: P) -> Self {
        Self { inner, origin: CgtV2Origin::new(cfg, provider), pending: None, last_in_span: false }
    }
}
#[async_trait]
impl<B: AttributesProvider + Stage + Send, P: ChainProvider + Send> AttributesProvider
    for CgtV2BatchProvider<B, P>
{
    async fn next_batch(&mut self, parent: L2BlockInfo) -> PipelineResult<SingleBatch> {
        if self.pending.is_none() {
            self.pending = Some(self.inner.next_batch(parent).await?);
            self.last_in_span = self.inner.is_last_in_span();
        }
        let batch = self.pending.as_ref().expect("batch just loaded");
        let user = batch.transactions.iter().any(|tx| {
            !(self.origin.cfg.is_lagoon_active(batch.timestamp) && tx.first() == Some(&0x7f))
        });
        if user && self.origin.amount(parent, batch.epoch()).await?.is_some() {
            // Drop the invalid block and the rest of its channel/span. Already derived
            // prefix blocks remain accepted, matching Holocene's sequential validation.
            self.inner.flush_channel().await?;
            self.pending = None;
            self.last_in_span = false;
            return Err(PipelineError::NotEnoughData.temp());
        }
        Ok(self.pending.take().expect("pending batch retained across provider errors"))
    }
    fn is_last_in_span(&self) -> bool {
        self.last_in_span
    }
}
impl<B: OriginProvider, P> OriginProvider for CgtV2BatchProvider<B, P> {
    fn origin(&self) -> Option<BlockInfo> {
        self.inner.origin()
    }
}
#[async_trait]
impl<B: OriginAdvancer + Send, P: Send> OriginAdvancer for CgtV2BatchProvider<B, P> {
    async fn advance_origin(&mut self) -> PipelineResult<()> {
        self.inner.advance_origin().await
    }
}
#[async_trait]
impl<B: Stage + Send, P: Send> Stage for CgtV2BatchProvider<B, P> {
    async fn reset(&mut self, origin: BlockNumHash, config: SystemConfig) -> PipelineResult<()> {
        self.pending = None;
        self.last_in_span = false;
        self.inner.reset(origin, config).await
    }
    async fn activate(&mut self) -> PipelineResult<()> {
        self.pending = None;
        self.last_in_span = false;
        self.inner.activate().await
    }
    async fn flush_channel(&mut self) -> PipelineResult<()> {
        self.pending = None;
        self.last_in_span = false;
        self.inner.flush_channel().await
    }
}
