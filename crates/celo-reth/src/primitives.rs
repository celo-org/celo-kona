//! Celo-specific primitive types for reth.

use crate::{receipt::CeloReceipt, signed_tx::CeloConsensusTx};

/// Celo-specific signed transaction type.
///
/// This is a thin wrapper around
/// [`CeloTxEnvelope`](celo_alloy_consensus::CeloTxEnvelope) that carries
/// ephemeral, native-equivalent fee values for CIP-64 transactions so that
/// the payload builder's miner-fee accounting produces correct results. See
/// [`CeloConsensusTx`] for details.
pub type CeloTransactionSigned = CeloConsensusTx;

/// Celo-specific block type.
pub type CeloBlock = alloy_consensus::Block<CeloTransactionSigned>;

/// Celo-specific block body type.
pub type CeloBlockBody = <CeloBlock as reth_primitives_traits::Block>::Body;

/// Primitive types for a Celo reth node.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CeloPrimitives;

impl reth_primitives_traits::NodePrimitives for CeloPrimitives {
    type Block = CeloBlock;
    type BlockHeader = alloy_consensus::Header;
    type BlockBody = CeloBlockBody;
    type SignedTx = CeloTransactionSigned;
    type Receipt = CeloReceipt;
}
