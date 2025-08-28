use alloc::vec::Vec;
use alloy_consensus::BlockBody;
use celo_alloy_consensus::{CeloBlock, CeloTxEnvelope};
use op_alloy_consensus::{OpBlock, OpTxEnvelope};
use tracing::warn;

/// Converts a CeloBlock to OpBlock.
/// NOTE: TxCip64 transactions are filtered out as they are not supported in the Optimism
pub fn convert_celo_block_to_op_block(celo_block: CeloBlock) -> OpBlock {
    OpBlock {
        header: celo_block.header,
        body: BlockBody {
            transactions: convert_celo_txs_to_op_txs(celo_block.body.transactions),
            ommers: celo_block.body.ommers,
            withdrawals: celo_block.body.withdrawals,
        },
    }
}

/// Converts a vector of CeloTxEnvelope to a vector of OpTxEnvelope.
///
/// This function processes a collection of Celo transactions and converts them
/// to Optimism transactions.
/// NOTE: TxCip64 transactions are filtered out as they are not supported in the Optimism
pub fn convert_celo_txs_to_op_txs(celo_txs: Vec<CeloTxEnvelope>) -> Vec<OpTxEnvelope> {
    celo_txs
        .into_iter()
        .enumerate()
        .filter_map(|(index, celo_tx)| match celo_tx {
            CeloTxEnvelope::Legacy(tx) => Some(OpTxEnvelope::Legacy(tx)),
            CeloTxEnvelope::Eip2930(tx) => Some(OpTxEnvelope::Eip2930(tx)),
            CeloTxEnvelope::Eip1559(tx) => Some(OpTxEnvelope::Eip1559(tx)),
            CeloTxEnvelope::Eip7702(tx) => Some(OpTxEnvelope::Eip7702(tx)),
            CeloTxEnvelope::Deposit(tx) => Some(OpTxEnvelope::Deposit(tx)),
            // TxCip64 is Celo-specific and not supported in OpTxEnvelope, so ignore it
            CeloTxEnvelope::Cip64(_) => {
                warn!(
                    target: "celo_batch_validation_adapter",
                    "Dropping TxCip64 transaction at index {} (not supported in OpTxEnvelope)",
                    index
                );
                None
            }
        })
        .collect()
}
