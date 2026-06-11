use alloc::vec::Vec;
use alloy_consensus::BlockBody;
use celo_alloy_consensus::{CeloBlock, CeloTxEnvelope, CeloTxType};
use kona_protocol::OpBlockConversionError;
use op_alloy_consensus::{OpBlock, OpTxEnvelope};
use tracing::debug;

/// Converts a [`CeloBlock`] to an [`OpBlock`], **silently dropping any CIP-64 transaction**
/// (CIP-64 has no `OpTxEnvelope` representation).
///
/// This is only safe for consumers that read solely the header and the L1-info deposit
/// (`transactions[0]`, which is never a CIP-64 tx), such as system-config derivation. Any
/// consumer sensitive to the full transaction set (e.g. span-batch overlap checks) must use
/// [`convert_celo_block_to_op_block_checked`], which fails closed instead of dropping.
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

/// Like [`convert_celo_block_to_op_block`] but fails closed when the block carries any CIP-64
/// transaction, instead of silently dropping it.
///
/// Use this for any consumer sensitive to the full transaction set. Today the upstream
/// span-batch codec rejects the CIP-64 type byte (`0x7b`) at decode, so a CIP-64 tx can never
/// reach a span batch and the silent drop is unreachable in practice; this guard turns that
/// into a hard error rather than a silent drop, so adding span-batch CIP-64 support (or any new
/// transaction-content-sensitive consumer of `block_by_number`) cannot silently forge a block.
pub fn convert_celo_block_to_op_block_checked(
    celo_block: CeloBlock,
) -> Result<OpBlock, OpBlockConversionError> {
    if celo_block.body.transactions.iter().any(|tx| matches!(tx, CeloTxEnvelope::Cip64(_))) {
        return Err(OpBlockConversionError::InvalidTxType(CeloTxType::Cip64 as u8));
    }
    Ok(convert_celo_block_to_op_block(celo_block))
}

/// Converts a vector of CeloTxEnvelope to a vector of OpTxEnvelope.
///
/// This function processes a collection of Celo transactions and converts them
/// to Optimism transactions.
/// NOTE: TxCip64 transactions are filtered out as they are not supported in Optimism
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
                debug!(
                    target: "convert_celo_txs_to_op_txs",
                    "Dropping CIP-64 transaction at index {} (not supported in OpTxEnvelope)",
                    index
                );
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Signed, TxLegacy};
    use alloy_primitives::{B256, Signature, U256};
    use celo_alloy_consensus::TxCip64;

    fn dummy_sig() -> Signature {
        Signature::new(U256::ONE, U256::ONE, false)
    }

    fn cip64_envelope() -> CeloTxEnvelope {
        CeloTxEnvelope::Cip64(Signed::new_unchecked(TxCip64::default(), dummy_sig(), B256::ZERO))
    }

    fn legacy_envelope() -> CeloTxEnvelope {
        CeloTxEnvelope::Legacy(Signed::new_unchecked(TxLegacy::default(), dummy_sig(), B256::ZERO))
    }

    #[test]
    fn checked_conversion_rejects_cip64() {
        let mut block = CeloBlock::default();
        block.body.transactions.push(legacy_envelope());
        block.body.transactions.push(cip64_envelope());
        let err = convert_celo_block_to_op_block_checked(block).unwrap_err();
        assert!(
            matches!(err, OpBlockConversionError::InvalidTxType(t) if t == CeloTxType::Cip64 as u8),
            "expected InvalidTxType(0x7b), got {err:?}",
        );
    }

    #[test]
    fn checked_conversion_keeps_non_cip64_txs() {
        let mut block = CeloBlock::default();
        block.body.transactions.push(legacy_envelope());
        let op_block = convert_celo_block_to_op_block_checked(block).expect("no cip64 → ok");
        assert_eq!(op_block.body.transactions.len(), 1);
    }
}
