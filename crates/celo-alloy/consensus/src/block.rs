//! Celo Block Type.

use crate::CeloTxEnvelope;

/// An Celo block type.
pub type CeloBlock = alloy_consensus::Block<CeloTxEnvelope>;
