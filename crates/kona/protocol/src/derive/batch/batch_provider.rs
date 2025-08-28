//! This module contains the [CeloBatchProvider] stage.
use alloc::{boxed::Box, sync::Arc};
use async_trait::async_trait;
use core::fmt::Debug;
use kona_derive::{
    errors::PipelineError,
    traits::{AttributesProvider, OriginAdvancer, OriginProvider, SignalReceiver},
    types::{PipelineResult, Signal},
};
use kona_genesis::RollupConfig;
use kona_protocol::{BlockInfo, L2BlockInfo, SingleBatch};

use crate::{
    CeloBatchValidator, CeloL2ChainProvider, CeloNextBatchProvider, derive::CeloBatchQueue,
};

/// TODO: Not need?
/// The [CeloBatchProvider] stage is a mux between the [BatchQueue] and [BatchValidator] stages.
///
/// Rules:
/// When Holocene is not active, the [BatchQueue] is used.
/// When Holocene is active, the [BatchValidator] is used.
///
/// When transitioning between the two stages, the mux will reset the active stage, but
/// retain `l1_blocks`.
#[derive(Debug)]
pub struct CeloBatchProvider<P, F>
where
    P: CeloNextBatchProvider + OriginAdvancer + OriginProvider + SignalReceiver + Debug,
    F: CeloL2ChainProvider + Send + Clone + Debug,
{
    /// The rollup configuration.
    cfg: Arc<RollupConfig>,
    /// The L2 chain provider.
    provider: F,
    /// The previous stage of the derivation pipeline.
    ///
    /// If this is set to [None], the multiplexer has been activated and the active stage
    /// owns the previous stage.
    ///
    /// Must be [None] if `batch_queue` or `batch_validator` is [Some].
    prev: Option<P>,
    /// The batch queue stage of the provider.
    ///
    /// Must be [None] if `prev` or `batch_validator` is [Some].
    batch_queue: Option<CeloBatchQueue<P, F>>,
    /// The batch validator stage of the provider.
    ///
    /// Must be [None] if `prev` or `batch_queue` is [Some].
    batch_validator: Option<CeloBatchValidator<P>>,
}

impl<P, F> CeloBatchProvider<P, F>
where
    P: CeloNextBatchProvider + OriginAdvancer + OriginProvider + SignalReceiver + Debug,
    F: CeloL2ChainProvider + Send + Clone + Debug,
{
    /// Creates a new [CeloBatchProvider] with the given configuration and previous stage.
    pub const fn new(cfg: Arc<RollupConfig>, prev: P, provider: F) -> Self {
        Self { cfg, provider, prev: Some(prev), batch_queue: None, batch_validator: None }
    }

    /// Attempts to update the active stage of the mux.
    pub(crate) fn attempt_update(&mut self) -> PipelineResult<()> {
        let origin = self.origin().ok_or(PipelineError::MissingOrigin.crit())?;
        if let Some(prev) = self.prev.take() {
            // On the first call to `attempt_update`, we need to determine the active stage to
            // initialize the mux with.
            if self.cfg.is_holocene_active(origin.timestamp) {
                self.batch_validator = Some(CeloBatchValidator::new(self.cfg.clone(), prev));
            } else {
                self.batch_queue =
                    Some(CeloBatchQueue::new(self.cfg.clone(), prev, self.provider.clone()));
            }
        } else if self.batch_queue.is_some() && self.cfg.is_holocene_active(origin.timestamp) {
            // If the batch queue is active and Holocene is also active, transition to the batch
            // validator.
            let batch_queue = self.batch_queue.take().expect("Must have batch queue");
            let mut bv = CeloBatchValidator::new(self.cfg.clone(), batch_queue.prev);
            bv.l1_blocks = batch_queue.l1_blocks;
            self.batch_validator = Some(bv);
        } else if self.batch_validator.is_some() && !self.cfg.is_holocene_active(origin.timestamp) {
            // If the batch validator is active, and Holocene is not active, it indicates an L1
            // reorg around Holocene activation. Transition back to the batch queue
            // until Holocene re-activates.
            let batch_validator = self.batch_validator.take().expect("Must have batch validator");
            let mut bq =
                CeloBatchQueue::new(self.cfg.clone(), batch_validator.prev, self.provider.clone());
            bq.l1_blocks = batch_validator.l1_blocks;
            self.batch_queue = Some(bq);
        }
        Ok(())
    }
}

#[async_trait]
impl<P, F> OriginAdvancer for CeloBatchProvider<P, F>
where
    P: CeloNextBatchProvider + OriginAdvancer + OriginProvider + SignalReceiver + Send + Debug,
    F: CeloL2ChainProvider + Clone + Send + Debug,
{
    async fn advance_origin(&mut self) -> PipelineResult<()> {
        self.attempt_update()?;

        if let Some(batch_validator) = self.batch_validator.as_mut() {
            batch_validator.advance_origin().await
        } else if let Some(batch_queue) = self.batch_queue.as_mut() {
            batch_queue.advance_origin().await
        } else {
            Err(PipelineError::NotEnoughData.temp())
        }
    }
}

impl<P, F> OriginProvider for CeloBatchProvider<P, F>
where
    P: CeloNextBatchProvider + OriginAdvancer + OriginProvider + SignalReceiver + Debug,
    F: CeloL2ChainProvider + Send + Clone + Debug,
{
    fn origin(&self) -> Option<BlockInfo> {
        self.batch_validator.as_ref().map_or_else(
            || {
                self.batch_queue.as_ref().map_or_else(
                    || self.prev.as_ref().and_then(|prev| prev.origin()),
                    |batch_queue| batch_queue.origin(),
                )
            },
            |batch_validator| batch_validator.origin(),
        )
    }
}

#[async_trait]
impl<P, F> SignalReceiver for CeloBatchProvider<P, F>
where
    P: CeloNextBatchProvider + OriginAdvancer + OriginProvider + SignalReceiver + Send + Debug,
    F: CeloL2ChainProvider + Clone + Send + Debug,
{
    async fn signal(&mut self, signal: Signal) -> PipelineResult<()> {
        self.attempt_update()?;

        if let Some(batch_validator) = self.batch_validator.as_mut() {
            batch_validator.signal(signal).await
        } else if let Some(batch_queue) = self.batch_queue.as_mut() {
            batch_queue.signal(signal).await
        } else {
            Err(PipelineError::NotEnoughData.temp())
        }
    }
}

#[async_trait]
impl<P, F> AttributesProvider for CeloBatchProvider<P, F>
where
    P: CeloNextBatchProvider + OriginAdvancer + OriginProvider + SignalReceiver + Debug + Send,
    F: CeloL2ChainProvider + Clone + Send + Debug,
{
    fn is_last_in_span(&self) -> bool {
        self.batch_validator.as_ref().map_or_else(
            || self.batch_queue.as_ref().is_some_and(|batch_queue| batch_queue.is_last_in_span()),
            |batch_validator| batch_validator.is_last_in_span(),
        )
    }

    async fn next_batch(&mut self, parent: L2BlockInfo) -> PipelineResult<SingleBatch> {
        self.attempt_update()?;

        if let Some(batch_validator) = self.batch_validator.as_mut() {
            batch_validator.next_batch(parent).await
        } else if let Some(batch_queue) = self.batch_queue.as_mut() {
            batch_queue.next_batch(parent).await
        } else {
            Err(PipelineError::NotEnoughData.temp())
        }
    }
}
