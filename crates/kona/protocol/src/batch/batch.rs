//! Module containing the core [Batch] enum.

use alloy_primitives::bytes;
use kona_genesis::RollupConfig;
use kona_protocol::{Batch, BatchDecodingError, BatchEncodingError, SingleBatch};

use crate::CeloSpanBatch;

/// A Batch.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum CeloBatch {
    /// A single batch
    Single(SingleBatch),
    /// Span Batches
    Span(CeloSpanBatch),
}

impl CeloBatch {
    /// Returns the timestamp for the batch.
    pub fn timestamp(&self) -> u64 {
        match self {
            Self::Single(sb) => sb.timestamp,
            Self::Span(sb) => sb.starting_timestamp(),
        }
    }

    /// Attempts to decode a batch from a reader.
    pub fn decode(r: &mut &[u8], cfg: &RollupConfig) -> Result<Self, BatchDecodingError> {
        Batch::decode(r, cfg).map(|batch| match batch {
            Batch::Single(inner) => CeloBatch::Single(inner),
            Batch::Span(inner) => CeloBatch::Span(CeloSpanBatch { inner }),
        })
    }

    /// Attempts to encode the batch to a writer.
    pub fn encode(&self, out: &mut dyn bytes::BufMut) -> Result<(), BatchEncodingError> {
        let inner = match self {
            Self::Single(inner) => Batch::Single(inner.clone()),
            Self::Span(batch) => Batch::Span(batch.inner.clone()),
        };
        inner.encode(out)
    }
}
