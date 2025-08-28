//! Chain providers for the derivation pipeline.
use crate::CeloBatchValidationProvider;
use alloc::{boxed::Box, sync::Arc};
use async_trait::async_trait;
use core::fmt::Display;
use kona_derive::errors::PipelineErrorKind;
use kona_genesis::{RollupConfig, SystemConfig};

/// Describes the functionality of a data source that fetches safe blocks.
/// TODO: Not need?
#[async_trait]
pub trait CeloL2ChainProvider: CeloBatchValidationProviderDerive {
    /// The error type for the [L2ChainProvider].
    type Error: Display + Into<PipelineErrorKind>;

    /// Returns the [SystemConfig] by L2 number.
    async fn system_config_by_number(
        &mut self,
        number: u64,
        rollup_config: Arc<RollupConfig>,
    ) -> Result<SystemConfig, <Self as CeloL2ChainProvider>::Error>;
}

/// A super-trait for [BatchValidationProvider] that binds `Self::Error` to have a conversion into
/// [PipelineErrorKind].
pub trait CeloBatchValidationProviderDerive: CeloBatchValidationProvider {}

// Auto-implement the [BatchValidationProviderDerive] trait for all types that implement
// [BatchValidationProvider] where the error can be converted into [PipelineErrorKind].
impl<T> CeloBatchValidationProviderDerive for T where T: CeloBatchValidationProvider {}
