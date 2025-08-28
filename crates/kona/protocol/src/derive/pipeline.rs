//! Contains the core derivation pipeline.
use crate::CeloL2ChainProvider;
use alloc::{boxed::Box, collections::VecDeque, sync::Arc};
use async_trait::async_trait;
use core::fmt::Debug;
use kona_derive::{
    errors::{PipelineError, PipelineErrorKind},
    traits::{NextAttributes, OriginAdvancer, OriginProvider, Pipeline, SignalReceiver},
    types::{ActivationSignal, PipelineResult, ResetSignal, Signal, StepResult},
};
use kona_genesis::{RollupConfig, SystemConfig};
use kona_protocol::{BlockInfo, L2BlockInfo, OpAttributesWithParent};
use tracing::{error, trace, warn};

/// TODO: Not need?
/// The derivation pipeline is responsible for deriving L2 inputs from L1 data.
#[derive(Debug)]
pub struct CeloDerivationPipeline<S, P>
where
    S: NextAttributes + SignalReceiver + OriginProvider + OriginAdvancer + Debug + Send,
    P: CeloL2ChainProvider + Send + Sync + Debug,
{
    /// A handle to the next attributes.
    pub attributes: S,
    /// Reset provider for the pipeline.
    /// A list of prepared [OpAttributesWithParent] to be used by the derivation pipeline
    /// consumer.
    pub prepared: VecDeque<OpAttributesWithParent>,
    /// The rollup config.
    pub rollup_config: Arc<RollupConfig>,
    /// The L2 Chain Provider used to fetch the system config on reset.
    pub l2_chain_provider: P,
}

impl<S, P> CeloDerivationPipeline<S, P>
where
    S: NextAttributes + SignalReceiver + OriginProvider + OriginAdvancer + Debug + Send,
    P: CeloL2ChainProvider + Send + Sync + Debug,
{
    /// Creates a new instance of the [DerivationPipeline].
    pub const fn new(
        attributes: S,
        rollup_config: Arc<RollupConfig>,
        l2_chain_provider: P,
    ) -> Self {
        Self { attributes, prepared: VecDeque::new(), rollup_config, l2_chain_provider }
    }
}

impl<S, P> OriginProvider for CeloDerivationPipeline<S, P>
where
    S: NextAttributes + SignalReceiver + OriginProvider + OriginAdvancer + Debug + Send,
    P: CeloL2ChainProvider + Send + Sync + Debug,
{
    fn origin(&self) -> Option<BlockInfo> {
        self.attributes.origin()
    }
}

impl<S, P> Iterator for CeloDerivationPipeline<S, P>
where
    S: NextAttributes + SignalReceiver + OriginProvider + OriginAdvancer + Debug + Send + Sync,
    P: CeloL2ChainProvider + Send + Sync + Debug,
{
    type Item = OpAttributesWithParent;

    fn next(&mut self) -> Option<Self::Item> {
        self.prepared.pop_front()
    }
}

#[async_trait]
impl<S, P> SignalReceiver for CeloDerivationPipeline<S, P>
where
    S: NextAttributes + SignalReceiver + OriginProvider + OriginAdvancer + Debug + Send + Sync,
    P: CeloL2ChainProvider + Send + Sync + Debug,
{
    async fn signal(&mut self, signal: Signal) -> PipelineResult<()> {
        match signal {
            mut s @ Signal::Reset(ResetSignal { l2_safe_head, .. }) |
            mut s @ Signal::Activation(ActivationSignal { l2_safe_head, .. }) => {
                let system_config = self
                    .l2_chain_provider
                    .system_config_by_number(
                        l2_safe_head.block_info.number,
                        Arc::clone(&self.rollup_config),
                    )
                    .await
                    .map_err(Into::into)?;
                s = s.with_system_config(system_config);
                match self.attributes.signal(s).await {
                    Ok(()) => trace!(target: "pipeline", "Stages reset"),
                    Err(err) => {
                        if let PipelineErrorKind::Temporary(PipelineError::Eof) = err {
                            trace!(target: "pipeline", "Stages reset with EOF");
                        } else {
                            error!(target: "pipeline", "Stage reset errored: {:?}", err);
                            return Err(err);
                        }
                    }
                }
            }
            Signal::FlushChannel => {
                self.attributes.signal(signal).await?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl<S, P> Pipeline for CeloDerivationPipeline<S, P>
where
    S: NextAttributes + SignalReceiver + OriginProvider + OriginAdvancer + Debug + Send + Sync,
    P: CeloL2ChainProvider + Send + Sync + Debug,
{
    /// Peeks at the next prepared [OpAttributesWithParent] from the pipeline.
    fn peek(&self) -> Option<&OpAttributesWithParent> {
        self.prepared.front()
    }

    /// Returns the rollup config.
    fn rollup_config(&self) -> &RollupConfig {
        &self.rollup_config
    }

    /// Returns the [SystemConfig] by L2 number.
    async fn system_config_by_number(
        &mut self,
        number: u64,
    ) -> Result<SystemConfig, PipelineErrorKind> {
        self.l2_chain_provider
            .system_config_by_number(number, self.rollup_config.clone())
            .await
            .map_err(Into::into)
    }

    async fn step(&mut self, cursor: L2BlockInfo) -> StepResult {
        match self.attributes.next_attributes(cursor).await {
            Ok(a) => {
                trace!(target: "pipeline", "Prepared L2 attributes: {:?}", a);
                self.prepared.push_back(a);
                StepResult::PreparedAttributes
            }
            Err(err) => match err {
                PipelineErrorKind::Temporary(PipelineError::Eof) => {
                    trace!(target: "pipeline", "Pipeline advancing origin");
                    if let Err(e) = self.attributes.advance_origin().await {
                        return StepResult::OriginAdvanceErr(e);
                    }
                    StepResult::AdvancedOrigin
                }
                PipelineErrorKind::Temporary(_) => {
                    trace!(target: "pipeline", "Attributes queue step failed due to temporary error: {:?}", err);
                    StepResult::StepFailed(err)
                }
                _ => {
                    warn!(target: "pipeline", "Attributes queue step failed: {:?}", err);
                    StepResult::StepFailed(err)
                }
            },
        }
    }
}
