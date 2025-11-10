//! The driver of the kona derivation pipeline.

use crate::CeloExecutorTr;
use alloc::{sync::Arc, vec::Vec};
use alloy_consensus::BlockBody;
use alloy_primitives::{B256, Bytes};
use alloy_rlp::Decodable;
use celo_alloy_consensus::{CeloBlock, CeloTxEnvelope, CeloTxType};
use celo_alloy_rpc_types_engine::CeloPayloadAttributes;
use celo_executor::CeloBlockBuildingOutcome;
use celo_genesis::CeloRollupConfig;
use celo_protocol::CeloL2BlockInfo;
use core::fmt::Debug;
use kona_derive::{Pipeline, PipelineError, PipelineErrorKind, Signal, SignalReceiver};
use kona_driver::{DriverError, DriverPipeline, DriverResult, PipelineCursor, TipCursor};
use kona_protocol::{L2BlockInfo, OpAttributesWithParent};
use spin::RwLock;

/// The Rollup Driver entrypoint.
#[derive(Debug)]
pub struct CeloDriver<E, DP, P>
where
    E: CeloExecutorTr + Send + Sync + Debug,
    DP: DriverPipeline<P> + Send + Sync + Debug,
    P: Pipeline + SignalReceiver + Send + Sync + Debug,
{
    /// Marker for the pipeline.
    _marker: core::marker::PhantomData<P>,
    /// Cursor to keep track of the L2 tip
    pub cursor: Arc<RwLock<PipelineCursor>>,
    /// The Executor.
    pub executor: E,
    /// A pipeline abstraction.
    pub pipeline: DP,
    /// The safe head's execution artifacts + Transactions
    pub safe_head_artifacts: Option<(CeloBlockBuildingOutcome, Vec<Bytes>)>,
}

impl<E, DP, P> CeloDriver<E, DP, P>
where
    E: CeloExecutorTr + Send + Sync + Debug,
    DP: DriverPipeline<P> + Send + Sync + Debug,
    P: Pipeline + SignalReceiver + Send + Sync + Debug,
{
    /// Creates a new [CeloDriver].
    pub const fn new(cursor: Arc<RwLock<PipelineCursor>>, executor: E, pipeline: DP) -> Self {
        Self {
            _marker: core::marker::PhantomData,
            cursor,
            executor,
            pipeline,
            safe_head_artifacts: None,
        }
    }

    /// Waits until the executor is ready.
    pub async fn wait_for_executor(&mut self) {
        self.executor.wait_until_ready().await;
    }

    /// Advances the derivation pipeline to the target block number.
    ///
    /// ## Takes
    /// - `cfg`: The rollup configuration.
    /// - `target`: The target block number.
    ///
    /// ## Returns
    /// - `Ok((l2_safe_head, output_root))` - A tuple containing the [L2BlockInfo] of the produced
    ///   block and the output root.
    /// - `Err(e)` - An error if the block could not be produced.
    pub async fn advance_to_target(
        &mut self,
        cfg: &CeloRollupConfig,
        mut target: Option<u64>,
    ) -> DriverResult<(L2BlockInfo, B256), E::Error> {
        loop {
            // Check if we have reached the target block number.
            let pipeline_cursor = self.cursor.read();
            let tip_cursor = pipeline_cursor.tip();
            if let Some(tb) = target
                && tip_cursor.l2_safe_head.block_info.number >= tb
            {
                info!(target: "client", "Derivation complete, reached L2 safe head.");
                return Ok((tip_cursor.l2_safe_head, tip_cursor.l2_safe_head_output_root));
            }

            let OpAttributesWithParent { inner: mut attributes, .. } = match self
                .pipeline
                .produce_payload(tip_cursor.l2_safe_head)
                .await
            {
                Ok(attrs) => attrs,
                Err(PipelineErrorKind::Critical(PipelineError::EndOfSource)) => {
                    warn!(target: "client", "Exhausted data source; Halting derivation and using current safe head.");

                    // Adjust the target block number to the current safe head, as no more blocks
                    // can be produced.
                    if target.is_some() {
                        target = Some(tip_cursor.l2_safe_head.block_info.number);
                    };

                    // If we are in interop mode, this error must be handled by the caller.
                    // Otherwise, we continue the loop to halt derivation on the next iteration.
                    if cfg.is_interop_active(self.cursor.read().l2_safe_head().block_info.number) {
                        return Err(PipelineError::EndOfSource.crit().into());
                    } else {
                        continue;
                    }
                }
                Err(e) => {
                    error!(target: "client", "Failed to produce payload: {:?}", e);
                    return Err(DriverError::Pipeline(e));
                }
            };

            let celo_attributes =
                CeloPayloadAttributes { op_payload_attributes: attributes.clone() };
            self.executor.update_safe_head(tip_cursor.l2_safe_head_header.clone());
            let outcome = match self.executor.execute_payload(celo_attributes).await {
                Ok(outcome) => outcome,
                Err(e) => {
                    error!(target: "client", "Failed to execute L2 block: {}", e);

                    if cfg.is_holocene_active(attributes.payload_attributes.timestamp) {
                        // Retry with a deposit-only block.
                        warn!(target: "client", "Flushing current channel and retrying deposit only block");

                        // Flush the current batch and channel - if a block was replaced with a
                        // deposit-only block due to execution failure, the
                        // batch and channel it is contained in is forwards
                        // invalidated.
                        self.pipeline.signal(Signal::FlushChannel).await?;

                        // Strip out all transactions that are not deposits.
                        attributes.transactions = attributes.transactions.map(|txs| {
                            txs.into_iter()
                                .filter(|tx| !tx.is_empty() && tx[0] == CeloTxType::Deposit as u8)
                                .collect::<Vec<_>>()
                        });

                        // Retry the execution.
                        let celo_attributes =
                            CeloPayloadAttributes { op_payload_attributes: attributes.clone() };
                        self.executor.update_safe_head(tip_cursor.l2_safe_head_header.clone());
                        match self.executor.execute_payload(celo_attributes).await {
                            Ok(header) => header,
                            Err(e) => {
                                error!(
                                    target: "client",
                                    "Critical - Failed to execute deposit-only block: {e}",
                                );
                                return Err(DriverError::Executor(e));
                            }
                        }
                    } else {
                        // Pre-Holocene, discard the block if execution fails.
                        continue;
                    }
                }
            };

            // Construct the block.
            let block = CeloBlock {
                header: outcome.header.inner().clone(),
                body: BlockBody {
                    transactions: attributes
                        .transactions
                        .as_ref()
                        .unwrap_or(&Vec::new())
                        .iter()
                        .map(|tx| {
                            CeloTxEnvelope::decode(&mut tx.as_ref()).map_err(DriverError::Rlp)
                        })
                        .collect::<DriverResult<Vec<CeloTxEnvelope>, E::Error>>()?,
                    ommers: Vec::new(),
                    withdrawals: None,
                },
            };

            // Get the pipeline origin and update the tip cursor.
            let origin = self.pipeline.origin().ok_or(PipelineError::MissingOrigin.crit())?;
            let celo_l2_info = CeloL2BlockInfo::from_block_and_genesis(
                &block,
                &self.pipeline.rollup_config().genesis,
            )?;
            let l2_info = celo_l2_info.op_l2_block_info;
            let tip_cursor = TipCursor::new(
                l2_info,
                outcome.header.clone(),
                self.executor.compute_output_root().map_err(DriverError::Executor)?,
            );

            // Advance the derivation pipeline cursor
            drop(pipeline_cursor);
            self.cursor.write().advance(origin, tip_cursor);

            // Update the latest safe head artifacts.
            self.safe_head_artifacts = Some((outcome, attributes.transactions.unwrap_or_default()));
        }
    }
}
