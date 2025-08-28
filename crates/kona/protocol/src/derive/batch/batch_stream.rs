//! This module contains the `BatchStream` stage.
use crate::{
    CeloBatchValidationProvider, CeloL2ChainProvider, CeloNextBatchProvider, CeloSpanBatch,
    batch::CeloBatch,
};
use alloc::{boxed::Box, collections::VecDeque, sync::Arc};
use async_trait::async_trait;
use core::fmt::Debug;
use kona_derive::{
    errors::{PipelineEncodingError, PipelineError},
    stages::BatchStreamProvider,
    traits::{OriginAdvancer, OriginProvider, SignalReceiver},
    types::{PipelineResult, Signal},
};
use kona_genesis::RollupConfig;
use kona_protocol::{
    Batch, BatchValidationProvider, BatchValidity, BatchWithInclusionBlock, BlockInfo, L2BlockInfo,
    SingleBatch, SpanBatch,
};
use op_alloy_consensus::OpBlock;
use tracing::{error, trace};

/// Not need?
/// [BatchStream] stage in the derivation pipeline.
#[derive(Debug)]
pub struct CeloBatchStream<P, BF>
where
    P: BatchStreamProvider + OriginAdvancer + OriginProvider + SignalReceiver + Debug,
    BF: CeloL2ChainProvider + Debug,
{
    /// The previous stage in the derivation pipeline.
    prev: P,
    /// There can only be a single staged span batch.
    span: Option<SpanBatch>,
    /// A buffer of single batches derived from the [SpanBatch].
    buffer: VecDeque<SingleBatch>,
    /// A reference to the rollup config, used to check
    /// if the [BatchStream] stage should be activated.
    config: Arc<RollupConfig>,
    /// Used to validate the batches.
    fetcher: BF, // TODO: wrap BatchValidationProvider outside
}

impl<P, BF> CeloBatchStream<P, BF>
where
    P: BatchStreamProvider + OriginAdvancer + OriginProvider + SignalReceiver + Debug,
    BF: CeloL2ChainProvider + Debug,
{
    /// Create a new [BatchStream] stage.
    pub const fn new(prev: P, config: Arc<RollupConfig>, fetcher: BF) -> Self {
        Self { prev, span: None, buffer: VecDeque::new(), config, fetcher }
    }

    /// Returns if the [BatchStream] stage is active based on the
    /// origin timestamp and holocene activation timestamp.
    pub fn is_active(&self) -> PipelineResult<bool> {
        let origin = self.prev.origin().ok_or(PipelineError::MissingOrigin.crit())?;
        Ok(self.config.is_holocene_active(origin.timestamp))
    }

    /// Gets a [SingleBatch] from the in-memory buffer.
    pub fn get_single_batch(
        &mut self,
        parent: L2BlockInfo,
        l1_origins: &[BlockInfo],
    ) -> PipelineResult<SingleBatch> {
        trace!(target: "batch_span", "Attempting to get a SingleBatch from buffer len: {}", self.buffer.len());

        self.try_hydrate_buffer(parent, l1_origins)?;
        self.buffer.pop_front().ok_or_else(|| PipelineError::NotEnoughData.temp())
    }

    /// Hydrates the buffer with single batches derived from the span batch, if there is one
    /// queued up.
    pub fn try_hydrate_buffer(
        &mut self,
        parent: L2BlockInfo,
        l1_origins: &[BlockInfo],
    ) -> PipelineResult<()> {
        if let Some(span) = self.span.take() {
            self.buffer.extend(
                span.get_singular_batches(l1_origins, parent).map_err(|e| {
                    PipelineError::BadEncoding(PipelineEncodingError::from(e)).crit()
                })?,
            );
        }
        Ok(())
    }
}

#[async_trait]
impl<P, BF> CeloNextBatchProvider for CeloBatchStream<P, BF>
where
    P: BatchStreamProvider + OriginAdvancer + OriginProvider + SignalReceiver + Send + Debug,
    BF: CeloL2ChainProvider + Send + Debug,
{
    fn flush(&mut self) {
        if self.is_active().unwrap_or(false) {
            self.prev.flush();
            self.span = None;
            self.buffer.clear();
        }
    }

    fn span_buffer_size(&self) -> usize {
        self.buffer.len()
    }

    async fn next_batch(
        &mut self,
        parent: L2BlockInfo,
        l1_origins: &[BlockInfo],
    ) -> PipelineResult<CeloBatch> {
        // If the stage is not active, "pass" the next batch
        // through this stage to the BatchQueue stage.
        if !self.is_active()? {
            trace!(target: "batch_span", "BatchStream stage is inactive, pass-through.");
            return self.prev.next_batch().await.map(|batch| match batch {
                Batch::Single(inner) => CeloBatch::Single(inner),
                Batch::Span(inner) => CeloBatch::Span(CeloSpanBatch { inner }),
            });
        }

        // If the buffer is empty, attempt to pull a batch from the previous stage.
        if self.buffer.is_empty() {
            // Safety: bubble up any errors from the batch reader.
            let batch_with_inclusion = BatchWithInclusionBlock::new(
                self.origin().ok_or(PipelineError::MissingOrigin.crit())?,
                self.prev.next_batch().await?,
            );

            // If the next batch is a singular batch, it is immediately
            // forwarded to the `BatchQueue` stage. Otherwise, we buffer
            // the span batch in this stage if it passes the validity checks.
            match batch_with_inclusion.batch {
                Batch::Single(b) => return Ok(CeloBatch::Single(b)),
                Batch::Span(b) => {
                    let (validity, _) = b
                        .check_batch_prefix(
                            self.config.as_ref(),
                            l1_origins,
                            parent,
                            &batch_with_inclusion.inclusion_block,
                            &mut L2BlockInfoFetcher(&mut self.fetcher),
                        )
                        .await;

                    match validity {
                        BatchValidity::Accept => self.span = Some(b),
                        BatchValidity::Drop => {
                            // Flush the stage.
                            self.flush();

                            return Err(PipelineError::Eof.temp());
                        }
                        BatchValidity::Past => {
                            if !self.is_active()? {
                                error!(target: "batch_stream", "BatchValidity::Past is not allowed pre-holocene");
                                return Err(PipelineError::InvalidBatchValidity.crit());
                            }

                            return Err(PipelineError::NotEnoughData.temp());
                        }
                        BatchValidity::Undecided | BatchValidity::Future => {
                            return Err(PipelineError::NotEnoughData.temp());
                        }
                    }
                }
            }
        }

        // Attempt to pull a SingleBatch out of the SpanBatch.
        self.get_single_batch(parent, l1_origins).map(CeloBatch::Single)
    }
}

#[async_trait]
impl<P, BF> OriginAdvancer for CeloBatchStream<P, BF>
where
    P: BatchStreamProvider + OriginAdvancer + OriginProvider + SignalReceiver + Send + Debug,
    BF: CeloL2ChainProvider + Send + Debug,
{
    async fn advance_origin(&mut self) -> PipelineResult<()> {
        self.prev.advance_origin().await
    }
}

impl<P, BF> OriginProvider for CeloBatchStream<P, BF>
where
    P: BatchStreamProvider + OriginAdvancer + OriginProvider + SignalReceiver + Debug,
    BF: CeloL2ChainProvider + Debug,
{
    fn origin(&self) -> Option<BlockInfo> {
        self.prev.origin()
    }
}

#[async_trait]
impl<P, BF> SignalReceiver for CeloBatchStream<P, BF>
where
    P: BatchStreamProvider + OriginAdvancer + OriginProvider + SignalReceiver + Debug + Send,
    BF: CeloL2ChainProvider + Send + Debug,
{
    async fn signal(&mut self, signal: Signal) -> PipelineResult<()> {
        self.prev.signal(signal).await?;
        self.buffer.clear();
        self.span.take();
        Ok(())
    }
}

/// TODO: Need?
/// Wrapper implementing BatchValidationProvider for CeloBatchValidationProvider
/// Used by check_batch_prefix which only needs l2_block_info_by_number (block_by_number is unused).
struct L2BlockInfoFetcher<'a, BF>(&'a mut BF)
where
    BF: CeloBatchValidationProvider + Send;

#[async_trait]
impl<'a, BF> BatchValidationProvider for L2BlockInfoFetcher<'a, BF>
where
    BF: CeloBatchValidationProvider + Send,
{
    type Error = BF::Error;

    async fn l2_block_info_by_number(&mut self, number: u64) -> Result<L2BlockInfo, Self::Error> {
        self.0.l2_block_info_by_number(number).await
    }

    async fn block_by_number(&mut self, _number: u64) -> Result<OpBlock, Self::Error> {
        error!("block_by_number should not be called on L2BlockInfoFetcher");
        todo!();
    }
}
