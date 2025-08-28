//! Contains an oracle-backed pipeline.
use alloc::{boxed::Box, sync::Arc};
use async_trait::async_trait;
use celo_protocol::{
    CeloAttributesQueueStage, CeloDerivationPipeline, CeloL2ChainProvider, CeloPipelineBuilder,
    CeloStatefulAttributesBuilder,
};
use core::fmt::Debug;
use kona_derive::{
    errors::PipelineErrorKind,
    traits::{ChainProvider, DataAvailabilityProvider, OriginProvider, Pipeline, SignalReceiver},
    types::{PipelineResult, ResetSignal, Signal, StepResult},
};
use kona_driver::{DriverPipeline, PipelineCursor};
use kona_genesis::{RollupConfig, SystemConfig};
use kona_preimage::CommsClient;
use kona_proof::FlushableCache;
use kona_protocol::{BlockInfo, L2BlockInfo, OpAttributesWithParent};
use spin::RwLock;

/// An oracle-backed derivation pipeline.
pub type CeloProviderDerivationPipeline<L1, L2, DA> = CeloDerivationPipeline<
    CeloAttributesQueueStage<DA, L1, L2, ProviderAttributesBuilder<L1, L2>>,
    L2,
>;

/// An oracle-backed payload attributes builder for the `AttributesQueue` stage of the derivation
/// pipeline.
pub type ProviderAttributesBuilder<L1, L2> = CeloStatefulAttributesBuilder<L1, L2>;

/// TODO: Not need?
/// An oracle-backed derivation pipeline.
#[derive(Debug)]
pub struct CeloOraclePipeline<O, L1, L2, DA>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    L1: ChainProvider + Send + Sync + Debug + Clone,
    L2: CeloL2ChainProvider + Send + Sync + Debug + Clone,
    DA: DataAvailabilityProvider + Send + Sync + Debug + Clone,
{
    pub pipeline: CeloProviderDerivationPipeline<L1, L2, DA>,
    pub caching_oracle: Arc<O>,
}

impl<O, L1, L2, DA> CeloOraclePipeline<O, L1, L2, DA>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    L1: ChainProvider + Send + Sync + Debug + Clone,
    L2: CeloL2ChainProvider + Send + Sync + Debug + Clone,
    DA: DataAvailabilityProvider + Send + Sync + Debug + Clone,
{
    /// Constructs a new oracle-backed derivation pipeline.
    pub async fn new(
        cfg: Arc<RollupConfig>,
        sync_start: Arc<RwLock<PipelineCursor>>,
        caching_oracle: Arc<O>,
        da_provider: DA,
        chain_provider: L1,
        mut l2_chain_provider: L2,
    ) -> PipelineResult<Self> {
        let attributes = CeloStatefulAttributesBuilder::new(
            cfg.clone(),
            l2_chain_provider.clone(),
            chain_provider.clone(),
        );

        let mut pipeline = CeloPipelineBuilder::new()
            .rollup_config(cfg.clone())
            .dap_source(da_provider)
            .l2_chain_provider(l2_chain_provider.clone())
            .chain_provider(chain_provider)
            .builder(attributes)
            .origin(sync_start.read().origin())
            .build();

        // Reset the pipeline to populate the initial system configuration in L1 Traversal.
        let l2_safe_head = *sync_start.read().l2_safe_head();
        pipeline
            .signal(
                ResetSignal {
                    l2_safe_head,
                    l1_origin: sync_start.read().origin(),
                    system_config: l2_chain_provider
                        .system_config_by_number(l2_safe_head.block_info.number, cfg.clone())
                        .await
                        .ok(),
                }
                .signal(),
            )
            .await?;

        Ok(Self { pipeline, caching_oracle })
    }
}

impl<O, L1, L2, DA> DriverPipeline<CeloProviderDerivationPipeline<L1, L2, DA>>
    for CeloOraclePipeline<O, L1, L2, DA>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    L1: ChainProvider + Send + Sync + Debug + Clone,
    L2: CeloL2ChainProvider + Send + Sync + Debug + Clone,
    DA: DataAvailabilityProvider + Send + Sync + Debug + Clone,
{
    /// Flushes the cache on re-org.
    fn flush(&mut self) {
        self.caching_oracle.flush();
    }
}

#[async_trait]
impl<O, L1, L2, DA> SignalReceiver for CeloOraclePipeline<O, L1, L2, DA>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    L1: ChainProvider + Send + Sync + Debug + Clone,
    L2: CeloL2ChainProvider + Send + Sync + Debug + Clone,
    DA: DataAvailabilityProvider + Send + Sync + Debug + Clone,
{
    /// Receives a signal from the driver.
    async fn signal(&mut self, signal: Signal) -> PipelineResult<()> {
        self.pipeline.signal(signal).await
    }
}

impl<O, L1, L2, DA> OriginProvider for CeloOraclePipeline<O, L1, L2, DA>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    L1: ChainProvider + Send + Sync + Debug + Clone,
    L2: CeloL2ChainProvider + Send + Sync + Debug + Clone,
    DA: DataAvailabilityProvider + Send + Sync + Debug + Clone,
{
    /// Returns the optional L1 [BlockInfo] origin.
    fn origin(&self) -> Option<BlockInfo> {
        self.pipeline.origin()
    }
}

impl<O, L1, L2, DA> Iterator for CeloOraclePipeline<O, L1, L2, DA>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    L1: ChainProvider + Send + Sync + Debug + Clone,
    L2: CeloL2ChainProvider + Send + Sync + Debug + Clone,
    DA: DataAvailabilityProvider + Send + Sync + Debug + Clone,
{
    type Item = OpAttributesWithParent;

    fn next(&mut self) -> Option<Self::Item> {
        self.pipeline.next()
    }
}

#[async_trait]
impl<O, L1, L2, DA> Pipeline for CeloOraclePipeline<O, L1, L2, DA>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    L1: ChainProvider + Send + Sync + Debug + Clone,
    L2: CeloL2ChainProvider + Send + Sync + Debug + Clone,
    DA: DataAvailabilityProvider + Send + Sync + Debug + Clone,
{
    /// Peeks at the next [OpAttributesWithParent] from the pipeline.
    fn peek(&self) -> Option<&OpAttributesWithParent> {
        self.pipeline.peek()
    }

    /// Attempts to progress the pipeline.
    async fn step(&mut self, cursor: L2BlockInfo) -> StepResult {
        self.pipeline.step(cursor).await
    }

    /// Returns the rollup config.
    fn rollup_config(&self) -> &RollupConfig {
        self.pipeline.rollup_config()
    }

    /// Returns the [SystemConfig] by L2 number.
    async fn system_config_by_number(
        &mut self,
        number: u64,
    ) -> Result<SystemConfig, PipelineErrorKind> {
        self.pipeline.system_config_by_number(number).await
    }
}
