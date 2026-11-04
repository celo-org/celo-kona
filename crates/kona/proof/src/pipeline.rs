//! Celo pipeline composition over the pinned upstream stages.
//!
//! Mirrors kona-proof's l1/pipeline.rs and kona-derive's pipeline/builder.rs at
//! c9d5a6d69e1ec7efce7d453cc4cd9a424804bb9a. Only the attributes and batch guards differ.

use alloc::{boxed::Box, sync::Arc};
use alloy_primitives::B256;
use async_trait::async_trait;
use celo_derive::{CgtV2AttributesBuilder, CgtV2BatchProvider, ReceiptCacheProvider};
use core::fmt::Debug;
use kona_derive::{
    AttributesQueue, BatchProvider, BatchProviderStage, BatchStream, ChainProvider,
    ChannelProvider, ChannelReader, DataAvailabilityProvider, DerivationPipeline, FrameQueue,
    L1Retrieval, L2ChainProvider, OriginProvider, Pipeline, PipelineResult, PollingTraversal,
    ResetSignal, Signal, SignalReceiver, StatefulAttributesBuilder, StepResult,
};
use kona_driver::{DriverPipeline, PipelineCursor};
use kona_genesis::{DependencySet, L1ChainConfig, RollupConfig, SystemConfig};
use kona_preimage::CommsClient;
use kona_proof::FlushableCache;
use kona_protocol::{BlockInfo, L2BlockInfo, OpAttributesWithParent};
use spin::RwLock;

/// Standard polled stages with a guard after span decomposition.
pub type CeloAttributesStage<DA, L1, L2> = AttributesQueue<
    CgtV2BatchProvider<
        BatchProviderStage<DA, PollingTraversal<ReceiptCacheProvider<L1>>, L2>,
        ReceiptCacheProvider<L1>,
    >,
    CgtV2AttributesBuilder<
        StatefulAttributesBuilder<ReceiptCacheProvider<L1>, L2>,
        ReceiptCacheProvider<L1>,
    >,
>;
/// The concrete derivation pipeline used by the client.
pub type CeloDerivationPipeline<DA, L1, L2> =
    DerivationPipeline<CeloAttributesStage<DA, L1, L2>, L2>;

/// Oracle-backed pipeline with Celo's receipt-triggered upgrade construction.
#[derive(Debug)]
pub struct CeloOraclePipeline<O, P> {
    /// Composed upstream stages.
    pub pipeline: P,
    caching_oracle: Arc<O>,
}
impl<O, P> CeloOraclePipeline<O, P> {
    /// Wrap an initialized pipeline. The constructor below handles initial reset.
    pub const fn from_pipeline(pipeline: P, caching_oracle: Arc<O>) -> Self {
        Self { pipeline, caching_oracle }
    }
}

/// Compose the pinned upstream stages, inserting the Celo guard before AttributesQueue.
/// Creates the attributes builder and a shared authenticated receipt cache internally.
#[allow(clippy::too_many_arguments)]
pub async fn new_celo_oracle_pipeline<O, L1, L2, DA>(
    cfg: Arc<RollupConfig>,
    l1_cfg: Arc<L1ChainConfig>,
    sync_start: Arc<RwLock<PipelineCursor>>,
    oracle: Arc<O>,
    da: DA,
    provider: L1,
    l2: L2,
    dependency_set: Option<Arc<DependencySet>>,
) -> PipelineResult<CeloOraclePipeline<O, CeloDerivationPipeline<DA, L1, L2>>>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    L1: ChainProvider + Clone + Send + Sync + Debug,
    L2: L2ChainProvider + Clone + Send + Sync + Debug,
    DA: DataAvailabilityProvider + Send + Sync + Debug,
{
    let provider = ReceiptCacheProvider::new(provider);
    let builder = StatefulAttributesBuilder::new(
        cfg.clone(),
        l1_cfg,
        l2.clone(),
        provider.clone(),
        dependency_set,
    );
    let attrs = CgtV2AttributesBuilder::new(cfg.clone(), builder, provider.clone());
    let mut traversal = PollingTraversal::new(provider.clone(), cfg.clone());
    traversal.block = Some(sync_start.read().origin());
    let retrieval = L1Retrieval::new(traversal, da);
    let frames = FrameQueue::new(retrieval, cfg.clone());
    let channels = ChannelProvider::new(cfg.clone(), frames);
    let reader = ChannelReader::new(channels, cfg.clone());
    let stream = BatchStream::new(reader, cfg.clone(), l2.clone());
    let batches = BatchProvider::new(cfg.clone(), stream, l2.clone());
    let guarded = CgtV2BatchProvider::new(cfg.clone(), batches, provider);
    let queue = AttributesQueue::new(cfg.clone(), guarded, attrs);
    let mut pipeline = DerivationPipeline::new(queue, cfg, l2);
    let l2_safe_head = *sync_start.read().l2_safe_head();
    pipeline.signal(Signal::Reset(ResetSignal { l2_safe_head })).await?;
    Ok(CeloOraclePipeline::from_pipeline(pipeline, oracle))
}
impl<O: FlushableCache + Send + Sync + Debug, P: Pipeline + SignalReceiver + Send + Sync + Debug>
    DriverPipeline<P> for CeloOraclePipeline<O, P>
{
    fn flush(&mut self) {
        self.caching_oracle.flush();
    }
}
#[async_trait]
impl<O: Send + Sync, P: SignalReceiver + Send> SignalReceiver for CeloOraclePipeline<O, P> {
    async fn signal(&mut self, signal: Signal) -> PipelineResult<()> {
        self.pipeline.signal(signal).await
    }
}
impl<O, P: OriginProvider> OriginProvider for CeloOraclePipeline<O, P> {
    fn origin(&self) -> Option<BlockInfo> {
        self.pipeline.origin()
    }
}
impl<O, P: Iterator<Item = OpAttributesWithParent>> Iterator for CeloOraclePipeline<O, P> {
    type Item = OpAttributesWithParent;
    fn next(&mut self) -> Option<Self::Item> {
        self.pipeline.next()
    }
}
#[async_trait]
impl<O: Send + Sync + Debug, P: Pipeline + Send + Sync + Debug> Pipeline
    for CeloOraclePipeline<O, P>
{
    fn peek(&self) -> Option<&OpAttributesWithParent> {
        self.pipeline.peek()
    }
    async fn step(&mut self, cursor: L2BlockInfo) -> StepResult {
        self.pipeline.step(cursor).await
    }
    fn rollup_config(&self) -> &RollupConfig {
        self.pipeline.rollup_config()
    }
    async fn system_config_by_l2_hash(&mut self, hash: B256) -> PipelineResult<SystemConfig> {
        self.pipeline.system_config_by_l2_hash(hash).await
    }
}
