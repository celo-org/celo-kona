//! Celo-specific primitive types for reth.

use crate::receipt::CeloReceipt;
use celo_alloy_consensus::CeloTxEnvelope;

/// Celo-specific signed transaction type.
pub type CeloTransactionSigned = CeloTxEnvelope;

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
