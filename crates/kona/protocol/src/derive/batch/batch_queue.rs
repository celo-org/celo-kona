//! This module contains the `BatchQueue` stage implementation.
use crate::{
    CeloL2ChainProvider, CeloNextBatchProvider,
    batch::{CeloBatch, CeloBatchWithInclusionBlock},
};
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use async_trait::async_trait;
use core::fmt::Debug;
use kona_derive::{
    errors::{PipelineEncodingError, PipelineError, PipelineErrorKind, ResetError},
    traits::{AttributesProvider, OriginAdvancer, OriginProvider, SignalReceiver},
    types::{PipelineResult, ResetSignal, Signal},
};
use kona_genesis::RollupConfig;
use kona_protocol::{BatchValidity, BlockInfo, L2BlockInfo, SingleBatch};
use tracing::{error, info, warn};

/// TODO: Not need?
/// [BatchQueue] is responsible for ordering unordered batches
/// and generating empty batches when the sequence window has passed.
///
/// It receives batches that are tagged with the L1 Inclusion block of the batch.
/// It only considers batches that are inside the sequencing window of a specific L1 Origin.
/// It tries to eagerly pull batches based on the current L2 safe head.
/// Otherwise it filters/creates an entire epoch's worth of batches at once.
///
/// This stage tracks a range of L1 blocks with the assumption that all batches with an L1 inclusion
/// block inside that range have been added to the stage by the time that it attempts to advance a
/// full epoch.
///
/// It is internally responsible for making sure that batches with L1 inclusions block outside it's
/// working range are not considered or pruned.
#[derive(Debug)]
pub struct CeloBatchQueue<P, BF>
where
    P: CeloNextBatchProvider + OriginAdvancer + OriginProvider + SignalReceiver + Debug,
    BF: CeloL2ChainProvider + Send + Clone + Debug,
{
    /// The rollup config.
    pub(crate) cfg: Arc<RollupConfig>,
    /// The previous stage of the derivation pipeline.
    pub(crate) prev: P,
    /// The l1 block ref
    pub(crate) origin: Option<BlockInfo>,
    /// A consecutive, time-centric window of L1 Blocks.
    /// Every L1 origin of unsafe L2 Blocks must be included in this list.
    /// If every L2 Block corresponding to a single L1 Block becomes safe,
    /// the block is popped from this list.
    /// If new L2 Block's L1 origin is not included in this list, fetch and
    /// push it to the list.
    pub(crate) l1_blocks: Vec<BlockInfo>,
    /// A set of batches in order from when we've seen them.
    pub(crate) batches: Vec<CeloBatchWithInclusionBlock>,
    /// A set of cached [SingleBatch]es derived from [SpanBatch]es.
    ///
    /// [SpanBatch]: kona_protocol::SpanBatch
    pub(crate) next_spans: Vec<SingleBatch>,
    /// Used to validate the batches.
    pub(crate) fetcher: BF,
}

impl<P, BF> CeloBatchQueue<P, BF>
where
    P: CeloNextBatchProvider + OriginAdvancer + OriginProvider + SignalReceiver + Debug,
    BF: CeloL2ChainProvider + Send + Clone + Debug,
{
    /// Creates a new [BatchQueue] stage.
    #[allow(clippy::missing_const_for_fn)]
    pub fn new(cfg: Arc<RollupConfig>, prev: P, fetcher: BF) -> Self {
        Self {
            cfg,
            prev,
            origin: None,
            l1_blocks: Default::default(),
            batches: Default::default(),
            next_spans: Default::default(),
            fetcher,
        }
    }

    /// Pops the next batch from the current queued up span-batch cache.
    /// The parent is used to set the parent hash of the batch.
    /// The parent is verified when the batch is later validated.
    pub fn pop_next_batch(&mut self, parent: L2BlockInfo) -> Option<SingleBatch> {
        if self.next_spans.is_empty() {
            panic!("Invalid state: must have next spans to pop");
        }
        let mut next = self.next_spans.remove(0);
        next.parent_hash = parent.block_info.hash;
        Some(next)
    }

    /// Derives the next batch to apply on top of the current L2 safe head.
    /// Follows the validity rules imposed on consecutive batches.
    /// Based on currently available buffered batch and L1 origin information.
    /// A [PipelineError::Eof] is returned if no batch can be derived yet.
    pub async fn derive_next_batch(
        &mut self,
        empty: bool,
        parent: L2BlockInfo,
    ) -> PipelineResult<CeloBatch> {
        // Cannot derive a batch if no origin was prepared.
        if self.l1_blocks.is_empty() {
            return Err(PipelineError::MissingOrigin.crit());
        }

        // Get the epoch
        let epoch = self.l1_blocks[0];
        info!(target: "batch_queue", "Deriving next batch for epoch: {}", epoch.number);

        // Note: epoch origin can now be one block ahead of the L2 Safe Head
        // This is in the case where we auto generate all batches in an epoch & advance the epoch
        // but don't advance the L2 Safe Head's epoch
        if parent.l1_origin != epoch.id() && parent.l1_origin.number != epoch.number - 1 {
            return Err(PipelineErrorKind::Reset(ResetError::L1OriginMismatch(
                parent.l1_origin.number,
                epoch.number - 1,
            )));
        }

        // Find the first-seen batch that matches all validity conditions.
        // We may not have sufficient information to proceed filtering, and then we stop.
        // There may be none: in that case we force-create an empty batch
        let mut next_batch = None;
        let next_timestamp = parent.block_info.timestamp + self.cfg.block_time;

        let origin = self.origin.ok_or(PipelineError::MissingOrigin.crit())?;

        // Go over all batches, in order of inclusion, and find the first batch we can accept.
        // Filter in-place by only remembering the batches that may be processed in the future, or
        // any undecided ones.
        let mut remaining = Vec::new();
        for i in 0..self.batches.len() {
            let batch = &self.batches[i];
            let validity =
                batch.check_batch(&self.cfg, &self.l1_blocks, parent, self.fetcher.clone()).await;
            match validity {
                BatchValidity::Future => {
                    // Drop Future batches post-holocene.
                    //
                    // See: <https://specs.optimism.io/protocol/holocene/derivation.html#batch_queue>
                    if !self.cfg.is_holocene_active(origin.timestamp) {
                        remaining.push(batch.clone());
                    } else {
                        self.prev.flush();
                        warn!(target: "batch_queue", "[HOLOCENE] Dropping future batch with parent: {}", parent.block_info.number);
                    }
                }
                BatchValidity::Drop => {
                    // If we drop a batch, flush previous batches buffered in the BatchStream
                    // stage.
                    self.prev.flush();
                    warn!(target: "batch_queue", "Dropping batch with parent: {}", parent.block_info);
                    continue;
                }
                BatchValidity::Accept => {
                    next_batch = Some(batch.clone());
                    // Don't keep the current batch in the remaining items since we are processing
                    // it now, but retain every batch we didn't get to yet.
                    remaining.extend_from_slice(&self.batches[i + 1..]);
                    break;
                }
                BatchValidity::Undecided => {
                    remaining.extend_from_slice(&self.batches[i..]);
                    self.batches = remaining;
                    return Err(PipelineError::Eof.temp());
                }
                BatchValidity::Past => {
                    if !self.cfg.is_holocene_active(origin.timestamp) {
                        error!(target: "batch_queue", "BatchValidity::Past is not allowed pre-holocene");
                        return Err(PipelineError::InvalidBatchValidity.crit());
                    }

                    warn!(target: "batch_queue", "[HOLOCENE] Dropping outdated batch with parent: {}", parent.block_info.number);
                    continue;
                }
            }
        }
        self.batches = remaining;

        if let Some(nb) = next_batch {
            info!(target: "batch_queue", "Next batch found for timestamp {}", nb.batch.timestamp());
            return Ok(nb.batch);
        }

        // If the current epoch is too old compared to the L1 block we are at,
        // i.e. if the sequence window expired, we create empty batches for the current epoch
        let expiry_epoch = epoch.number + self.cfg.seq_window_size;
        let force_empty_batches =
            (expiry_epoch == origin.number && empty) || expiry_epoch < origin.number;
        let first_of_epoch = epoch.number == parent.l1_origin.number + 1;

        // If the sequencer window did not expire,
        // there is still room to receive batches for the current epoch.
        // No need to force-create empty batch(es) towards the next epoch yet.
        if !force_empty_batches {
            return Err(PipelineError::Eof.temp());
        }

        info!(
            target: "batch_queue",
            "Generating empty batches for epoch: {} | parent: {}",
            epoch.number, parent.l1_origin.number
        );

        // The next L1 block is needed to proceed towards the next epoch.
        if self.l1_blocks.len() < 2 {
            return Err(PipelineError::Eof.temp());
        }

        let next_epoch = self.l1_blocks[1];

        // Fill with empty L2 blocks of the same epoch until we meet the time of the next L1 origin,
        // to preserve that L2 time >= L1 time. If this is the first block of the epoch, always
        // generate a batch to ensure that we at least have one batch per epoch.
        if next_timestamp < next_epoch.timestamp || first_of_epoch {
            info!(target: "batch_queue", "Generating empty batch for epoch: {}", epoch.number);
            return Ok(CeloBatch::Single(SingleBatch {
                parent_hash: parent.block_info.hash,
                epoch_num: epoch.number,
                epoch_hash: epoch.hash,
                timestamp: next_timestamp,
                transactions: Vec::new(),
            }));
        }

        // At this point we have auto generated every batch for the current epoch
        // that we can, so we can advance to the next epoch.
        info!(
            target: "batch_queue",
            "Advancing to next epoch: {}, timestamp: {}, epoch timestamp: {}",
            next_epoch.number, next_timestamp, next_epoch.timestamp
        );
        self.l1_blocks.remove(0);
        Err(PipelineError::Eof.temp())
    }

    /// Adds a batch to the queue.
    pub async fn add_batch(&mut self, batch: CeloBatch, parent: L2BlockInfo) -> PipelineResult<()> {
        if self.l1_blocks.is_empty() {
            error!(target: "batch_queue", "Cannot add batch without an origin");
            panic!("Cannot add batch without an origin");
        }
        let origin = self.origin.ok_or(PipelineError::MissingOrigin.crit())?;
        let data = CeloBatchWithInclusionBlock { inclusion_block: origin, batch };
        // If we drop the batch, validation logs the drop reason with WARN level.
        let validity =
            data.check_batch(&self.cfg, &self.l1_blocks, parent, self.fetcher.clone()).await;
        // Post-Holocene, future batches are dropped due to prevent gaps.
        let drop = validity.is_drop() ||
            (self.cfg.is_holocene_active(origin.timestamp) && validity.is_future());
        if drop {
            self.prev.flush();
            return Ok(());
        } else if validity.is_outdated() {
            // If the batch is outdated, we drop it without flushing the previous stage.
            return Ok(());
        }
        self.batches.push(data);
        Ok(())
    }
}

#[async_trait]
impl<P, BF> OriginAdvancer for CeloBatchQueue<P, BF>
where
    P: CeloNextBatchProvider + OriginAdvancer + OriginProvider + SignalReceiver + Send + Debug,
    BF: CeloL2ChainProvider + Clone + Send + Debug,
{
    async fn advance_origin(&mut self) -> PipelineResult<()> {
        self.prev.advance_origin().await
    }
}

#[async_trait]
impl<P, BF> AttributesProvider for CeloBatchQueue<P, BF>
where
    P: CeloNextBatchProvider + OriginAdvancer + OriginProvider + SignalReceiver + Send + Debug,
    BF: CeloL2ChainProvider + Clone + Send + Debug,
{
    /// Returns the next valid batch upon the given safe head.
    /// Also returns the boolean that indicates if the batch is the last block in the batch.
    async fn next_batch(&mut self, parent: L2BlockInfo) -> PipelineResult<SingleBatch> {
        if !self.next_spans.is_empty() {
            // There are cached singular batches derived from the span batch.
            // Check if the next cached batch matches the given parent block.
            if self.next_spans[0].timestamp == parent.block_info.timestamp + self.cfg.block_time {
                return self.pop_next_batch(parent).ok_or(PipelineError::BatchQueueEmpty.crit());
            }
            // Parent block does not match the next batch.
            // Means the previously returned batch is invalid.
            // Drop cached batches and find another batch.
            warn!(
                target: "batch_queue",
                "Parent block does not match the next batch. Dropping {} cached batches.",
                self.next_spans.len()
            );
            self.next_spans.clear();
        }

        // If the epoch is advanced, update the l1 blocks.
        // Advancing epoch must be done after the pipeline successfully applies the entire span
        // batch to the chain.
        // Because the span batch can be reverted during processing the batch, then we must
        // preserve existing l1 blocks to verify the epochs of the next candidate batch.
        if !self.l1_blocks.is_empty() && parent.l1_origin.number > self.l1_blocks[0].number {
            for (i, block) in self.l1_blocks.iter().enumerate() {
                if parent.l1_origin.number == block.number {
                    self.l1_blocks.drain(0..i);
                    info!(target: "batch_queue", "Advancing epoch");
                    break;
                }
            }
            // If the origin of the parent block is not included, we must advance the origin.
        }

        // NOTE: The origin is used to determine if it's behind.
        // It is the future origin that gets saved into the l1 blocks array.
        // We always update the origin of this stage if it's not the same so
        // after the update code runs, this is consistent.
        let origin_behind =
            self.prev.origin().map_or(true, |origin| origin.number < parent.l1_origin.number);

        // Advance the origin if needed.
        // The entire pipeline has the same origin.
        // Batches prior to the l1 origin of the l2 safe head are not accepted.
        if self.origin != self.prev.origin() {
            self.origin = self.prev.origin();
            if !origin_behind {
                let origin = match self.origin.as_ref().ok_or(PipelineError::MissingOrigin.crit()) {
                    Ok(o) => o,
                    Err(e) => {
                        return Err(e);
                    }
                };
                self.l1_blocks.push(*origin);
            } else {
                // This is to handle the special case of startup.
                // At startup, the batch queue is reset and includes the
                // l1 origin. That is the only time where immediately after
                // reset is called, the origin behind is false.
                self.l1_blocks.clear();
            }
            info!(target: "batch_queue", "Advancing batch queue origin: {:?}", self.origin);
        }

        // Load more data into the batch queue.
        let mut out_of_data = false;
        match self.prev.next_batch(parent, &self.l1_blocks).await {
            Ok(b) => {
                if !origin_behind {
                    self.add_batch(b, parent).await.ok();
                } else {
                    warn!(target: "batch_queue", "Dropping batch: Origin is behind");
                }
            }
            Err(e) => {
                if let PipelineErrorKind::Temporary(PipelineError::Eof) = e {
                    out_of_data = true;
                } else {
                    return Err(e);
                }
            }
        }

        // Skip adding the data unless up to date with the origin,
        // but still fully empty the previous stages.
        if origin_behind {
            if out_of_data {
                return Err(PipelineError::Eof.temp());
            }
            return Err(PipelineError::NotEnoughData.temp());
        }

        // Attempt to derive more batches.
        let batch = match self.derive_next_batch(out_of_data, parent).await {
            Ok(b) => b,
            Err(e) => match e {
                PipelineErrorKind::Temporary(PipelineError::Eof) => {
                    if out_of_data {
                        return Err(PipelineError::Eof.temp());
                    }
                    return Err(PipelineError::NotEnoughData.temp());
                }
                _ => return Err(e),
            },
        };

        // If the next batch is derived from the span batch, it's the last batch of the span.
        // For singular batches, the span batch cache should be empty.
        match batch {
            CeloBatch::Single(sb) => Ok(sb),
            CeloBatch::Span(sb) => {
                let batches = match sb.get_singular_batches(&self.l1_blocks, parent).map_err(|e| {
                    PipelineError::BadEncoding(PipelineEncodingError::SpanBatchError(e)).crit()
                }) {
                    Ok(b) => b,
                    Err(e) => {
                        return Err(e);
                    }
                };
                self.next_spans = batches;
                let nb = match self
                    .pop_next_batch(parent)
                    .ok_or(PipelineError::BatchQueueEmpty.crit())
                {
                    Ok(b) => b,
                    Err(e) => {
                        return Err(e);
                    }
                };
                Ok(nb)
            }
        }
    }

    /// Returns if the previous batch was the last in the span.
    fn is_last_in_span(&self) -> bool {
        self.next_spans.is_empty()
    }
}

impl<P, BF> OriginProvider for CeloBatchQueue<P, BF>
where
    P: CeloNextBatchProvider + OriginAdvancer + OriginProvider + SignalReceiver + Debug,
    BF: CeloL2ChainProvider + Clone + Send + Debug,
{
    fn origin(&self) -> Option<BlockInfo> {
        self.prev.origin()
    }
}

#[async_trait]
impl<P, BF> SignalReceiver for CeloBatchQueue<P, BF>
where
    P: CeloNextBatchProvider + OriginAdvancer + OriginProvider + SignalReceiver + Send + Debug,
    BF: CeloL2ChainProvider + Clone + Send + Debug,
{
    async fn signal(&mut self, signal: Signal) -> PipelineResult<()> {
        match signal {
            s @ Signal::Reset(ResetSignal { l1_origin, .. }) => {
                self.prev.signal(s).await?;
                self.origin = Some(l1_origin);
                self.batches.clear();
                // Include the new origin as an origin to build on.
                // This is only for the initialization case.
                // During normal resets we will later throw out this block.
                self.l1_blocks.clear();
                self.l1_blocks.push(l1_origin);
                self.next_spans.clear();
            }
            s @ Signal::Activation(_) | s @ Signal::FlushChannel => {
                self.prev.signal(s).await?;
                self.batches.clear();
                self.next_spans.clear();
            }
        }
        Ok(())
    }
}
