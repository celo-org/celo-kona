//! The Span Batch Type

use crate::CeloBatchValidationProvider;
use alloc::vec::Vec;
use alloy_consensus::{Block, Transaction, Typed2718};
use alloy_eips::eip2718::Encodable2718;
use celo_alloy_consensus::CeloTxEnvelope;
use core::ops::{Deref, DerefMut};
use kona_genesis::{ChainGenesis, RollupConfig};
use kona_protocol::{
    BatchValidity, BlockInfo, FromBlockError, L1BlockInfoTx, L2BlockInfo, SpanBatch,
};
use op_alloy_consensus::OpTxType;
use tracing::{info, warn};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CeloSpanBatch {
    pub inner: SpanBatch,
}

impl CeloSpanBatch {
    /// Checks if the span batch is valid.
    pub async fn check_batch<BV: CeloBatchValidationProvider>(
        &self,
        cfg: &RollupConfig,
        l1_blocks: &[BlockInfo],
        l2_safe_head: L2BlockInfo,
        inclusion_block: &BlockInfo,
        fetcher: &mut BV,
    ) -> BatchValidity {
        let (prefix_validity, parent_block) =
            self.check_batch_prefix(cfg, l1_blocks, l2_safe_head, inclusion_block, fetcher).await;
        if !matches!(prefix_validity, BatchValidity::Accept) {
            return prefix_validity;
        }

        let starting_epoch_num = self.starting_epoch_num();
        let parent_block = parent_block.expect("parent_block must be Some");

        let mut origin_index = 0;
        let mut origin_advanced = starting_epoch_num == parent_block.l1_origin.number + 1;
        for (i, batch) in self.batches.iter().enumerate() {
            if batch.timestamp <= l2_safe_head.block_info.timestamp {
                continue;
            }
            // Find the L1 origin for the batch.
            for (j, j_block) in l1_blocks.iter().enumerate().skip(origin_index) {
                if batch.epoch_num == j_block.number {
                    origin_index = j;
                    break;
                }
            }
            let l1_origin = l1_blocks[origin_index];
            if i > 0 {
                origin_advanced = false;
                if batch.epoch_num > self.batches[i - 1].epoch_num {
                    origin_advanced = true;
                }
            }
            let block_timestamp = batch.timestamp;
            if block_timestamp < l1_origin.timestamp {
                warn!(
                    "block timestamp is less than L1 origin timestamp, l2_timestamp: {}, l1_timestamp: {}, origin: {:?}",
                    block_timestamp,
                    l1_origin.timestamp,
                    l1_origin.id()
                );
                return BatchValidity::Drop;
            }

            // Check if we ran out of sequencer time drift
            let max_drift = cfg.max_sequencer_drift(l1_origin.timestamp);
            if block_timestamp > l1_origin.timestamp + max_drift {
                if batch.transactions.is_empty() {
                    // If the sequencer is co-operating by producing an empty batch,
                    // then allow the batch if it was the right thing to do to maintain the L2 time
                    // >= L1 time invariant. We only check batches that do not
                    // advance the epoch, to ensure epoch advancement regardless of time drift is
                    // allowed.
                    if !origin_advanced {
                        if origin_index + 1 >= l1_blocks.len() {
                            info!(
                                "without the next L1 origin we cannot determine yet if this empty batch that exceeds the time drift is still valid"
                            );
                            return BatchValidity::Undecided;
                        }
                        if block_timestamp >= l1_blocks[origin_index + 1].timestamp {
                            // check if the next L1 origin could have been adopted
                            info!(
                                "batch exceeded sequencer time drift without adopting next origin, and next L1 origin would have been valid"
                            );
                            return BatchValidity::Drop;
                        } else {
                            info!(
                                "continuing with empty batch before late L1 block to preserve L2 time invariant"
                            );
                        }
                    }
                } else {
                    // If the sequencer is ignoring the time drift rule, then drop the batch and
                    // force an empty batch instead, as the sequencer is not
                    // allowed to include anything past this point without moving to the next epoch.
                    warn!(
                        "batch exceeded sequencer time drift, sequencer must adopt new L1 origin to include transactions again, max_time: {}",
                        l1_origin.timestamp + max_drift
                    );
                    return BatchValidity::Drop;
                }
            }

            // Check that the transactions are not empty and do not contain any deposits.
            for (i, tx) in batch.transactions.iter().enumerate() {
                if tx.is_empty() {
                    warn!(
                        "transaction data must not be empty, but found empty tx, tx_index: {}",
                        i
                    );
                    return BatchValidity::Drop;
                }
                if tx.as_ref().first() == Some(&(OpTxType::Deposit as u8)) {
                    warn!(
                        "sequencers may not embed any deposits into batch data, but found tx that has one, tx_index: {}",
                        i
                    );
                    return BatchValidity::Drop;
                }

                // If isthmus is not active yet and the transaction is a 7702, drop the batch.
                if !cfg.is_isthmus_active(batch.timestamp) &&
                    tx.as_ref().first() == Some(&(OpTxType::Eip7702 as u8))
                {
                    warn!("EIP-7702 transactions are not supported pre-isthmus. tx_index: {}", i);
                    return BatchValidity::Drop;
                }
            }
        }

        // Check overlapped blocks
        let parent_num = parent_block.block_info.number;
        let next_timestamp = l2_safe_head.block_info.timestamp + cfg.block_time;
        if self.starting_timestamp() < next_timestamp {
            for i in 0..(l2_safe_head.block_info.number - parent_num) {
                let safe_block_num = parent_num + i + 1;
                let safe_block_payload = match fetcher.block_by_number(safe_block_num).await {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("failed to fetch block number {safe_block_num}: {e}");
                        return BatchValidity::Undecided;
                    }
                };
                let safe_block = &safe_block_payload.body;
                let batch_txs = &self.batches[i as usize].transactions;
                // Execution payload has deposit txs but batch does not.
                let deposit_count: usize = safe_block
                    .transactions
                    .iter()
                    .map(|tx| if tx.is_deposit() { 1 } else { 0 })
                    .sum();
                if safe_block.transactions.len() - deposit_count != batch_txs.len() {
                    warn!(
                        "overlapped block's tx count does not match, safe_block_txs: {}, batch_txs: {}",
                        safe_block.transactions.len(),
                        batch_txs.len()
                    );
                    return BatchValidity::Drop;
                }
                let batch_txs_len = batch_txs.len();
                #[allow(clippy::needless_range_loop)]
                for j in 0..batch_txs_len {
                    let mut buf = Vec::new();
                    safe_block.transactions[j + deposit_count].encode_2718(&mut buf);
                    if buf != batch_txs[j].0 {
                        warn!("overlapped block's transaction does not match");
                        return BatchValidity::Drop;
                    }
                }
                let safe_block_ref = match from_block_and_genesis(&safe_block_payload, &cfg.genesis)
                {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(
                            "failed to extract L2BlockInfo from execution payload, hash: {}, err: {e}",
                            safe_block_payload.header.hash_slow()
                        );
                        return BatchValidity::Drop;
                    }
                };
                if safe_block_ref.l1_origin.number != self.batches[i as usize].epoch_num {
                    warn!(
                        "overlapped block's L1 origin number does not match {}, {}",
                        safe_block_ref.l1_origin.number, self.batches[i as usize].epoch_num
                    );
                    return BatchValidity::Drop;
                }
            }
        }

        BatchValidity::Accept
    }

    /// Checks the validity of the batch's prefix.
    ///
    /// This function is used for post-Holocene hardfork to perform batch validation
    /// as each batch is being loaded in.
    pub async fn check_batch_prefix<BF: CeloBatchValidationProvider>(
        &self,
        cfg: &RollupConfig,
        l1_origins: &[BlockInfo],
        l2_safe_head: L2BlockInfo,
        inclusion_block: &BlockInfo,
        fetcher: &mut BF,
    ) -> (BatchValidity, Option<L2BlockInfo>) {
        if l1_origins.is_empty() {
            warn!("missing L1 block input, cannot proceed with batch checking");
            return (BatchValidity::Undecided, None);
        }
        if self.batches.is_empty() {
            warn!("empty span batch, cannot proceed with batch checking");
            return (BatchValidity::Undecided, None);
        }

        let epoch = l1_origins[0];
        let next_timestamp = l2_safe_head.block_info.timestamp + cfg.block_time;

        let starting_epoch_num = self.starting_epoch_num();
        let mut batch_origin = epoch;
        if starting_epoch_num == batch_origin.number + 1 {
            if l1_origins.len() < 2 {
                info!(
                    "eager batch wants to advance current epoch {:?}, but could not without more L1 blocks",
                    epoch.id()
                );
                return (BatchValidity::Undecided, None);
            }
            batch_origin = l1_origins[1];
        }
        if !cfg.is_delta_active(batch_origin.timestamp) {
            warn!(
                "received SpanBatch (id {:?}) with L1 origin (timestamp {}) before Delta hard fork",
                batch_origin.id(),
                batch_origin.timestamp
            );
            return (BatchValidity::Drop, None);
        }

        if self.starting_timestamp() > next_timestamp {
            warn!(
                "received out-of-order batch for future processing after next batch ({} > {})",
                self.starting_timestamp(),
                next_timestamp
            );

            // After holocene is activated, gaps are disallowed.
            if cfg.is_holocene_active(inclusion_block.timestamp) {
                return (BatchValidity::Drop, None);
            }
            return (BatchValidity::Future, None);
        }

        // Drop the batch if it has no new blocks after the safe head.
        if self.final_timestamp() < next_timestamp {
            warn!("span batch has no new blocks after safe head");
            return if cfg.is_holocene_active(inclusion_block.timestamp) {
                (BatchValidity::Past, None)
            } else {
                (BatchValidity::Drop, None)
            };
        }

        // Find the parent block of the span batch.
        // If the span batch does not overlap the current safe chain, parent block should be the L2
        // safe head.
        let mut parent_num = l2_safe_head.block_info.number;
        let mut parent_block = l2_safe_head;
        if self.starting_timestamp() < next_timestamp {
            if self.starting_timestamp() > l2_safe_head.block_info.timestamp {
                // Batch timestamp cannot be between safe head and next timestamp.
                warn!("batch has misaligned timestamp, block time is too short");
                return (BatchValidity::Drop, None);
            }
            if (l2_safe_head.block_info.timestamp - self.starting_timestamp()) % cfg.block_time != 0
            {
                warn!("batch has misaligned timestamp, not overlapped exactly");
                return (BatchValidity::Drop, None);
            }
            parent_num = l2_safe_head.block_info.number -
                (l2_safe_head.block_info.timestamp - self.starting_timestamp()) / cfg.block_time -
                1;
            parent_block = match fetcher.l2_block_info_by_number(parent_num).await {
                Ok(block) => block,
                Err(e) => {
                    warn!("failed to fetch L2 block number {parent_num}: {e}");
                    // Unable to validate the batch for now. Retry later.
                    return (BatchValidity::Undecided, None);
                }
            };
        }
        if !self.check_parent_hash(parent_block.block_info.hash) {
            warn!(
                "parent block mismatch, expected: {parent_num}, received: {}. parent hash: {}, parent hash check: {}",
                parent_block.block_info.number, parent_block.block_info.hash, self.parent_check,
            );
            return (BatchValidity::Drop, None);
        }

        // Filter out batches that were included too late.
        if starting_epoch_num + cfg.seq_window_size < inclusion_block.number {
            warn!("batch was included too late, sequence window expired");
            return (BatchValidity::Drop, None);
        }

        // Check the L1 origin of the batch
        if starting_epoch_num > parent_block.l1_origin.number + 1 {
            warn!(
                "batch is for future epoch too far ahead, while it has the next timestamp, so it must be invalid. starting epoch: {} | next epoch: {}",
                starting_epoch_num,
                parent_block.l1_origin.number + 1
            );
            return (BatchValidity::Drop, None);
        }

        // Verify the l1 origin hash for each l1 block.
        // SAFETY: `Self::batches` is not empty, so the last element is guaranteed to exist.
        let end_epoch_num = self.batches.last().unwrap().epoch_num;
        let mut origin_checked = false;
        // l1Blocks is supplied from batch queue and its length is limited to SequencerWindowSize.
        for l1_block in l1_origins {
            if l1_block.number == end_epoch_num {
                if !self.check_origin_hash(l1_block.hash) {
                    warn!(
                        "batch is for different L1 chain, epoch hash does not match, expected: {}",
                        l1_block.hash
                    );
                    return (BatchValidity::Drop, None);
                }
                origin_checked = true;
                break;
            }
        }
        if !origin_checked {
            info!("need more l1 blocks to check entire origins of span batch");
            return (BatchValidity::Undecided, None);
        }

        if starting_epoch_num < parent_block.l1_origin.number {
            warn!("dropped batch, epoch is too old, minimum: {:?}", parent_block.block_info.id());
            return (BatchValidity::Drop, None);
        }

        (BatchValidity::Accept, Some(parent_block))
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

fn from_block_and_genesis<T: Typed2718 + AsRef<CeloTxEnvelope>>(
    block: &Block<T>,
    genesis: &ChainGenesis,
) -> Result<L2BlockInfo, FromBlockError> {
    let block_info = BlockInfo::from(block);

    let (l1_origin, sequence_number) = if block_info.number == genesis.l2.number {
        if block_info.hash != genesis.l2.hash {
            return Err(FromBlockError::InvalidGenesisHash);
        }
        (genesis.l1, 0)
    } else {
        if block.body.transactions.is_empty() {
            return Err(FromBlockError::MissingL1InfoDeposit(block_info.hash));
        }

        let tx = block.body.transactions[0].as_ref();
        let Some(tx) = tx.as_deposit() else {
            return Err(FromBlockError::FirstTxNonDeposit(tx.ty()));
        };

        let l1_info = L1BlockInfoTx::decode_calldata(tx.input().as_ref())
            .map_err(FromBlockError::BlockInfoDecodeError)?;
        (l1_info.id(), l1_info.sequence_number())
    };

    Ok(L2BlockInfo { block_info, l1_origin, seq_num: sequence_number })
}
