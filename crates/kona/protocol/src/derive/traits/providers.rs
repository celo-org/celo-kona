//! Chain providers for the derivation pipeline.

use crate::CeloBatchValidationProvider;
use alloc::{boxed::Box, sync::Arc};
use async_trait::async_trait;
use celo_genesis::CeloRollupConfig;
use core::fmt::Display;
use kona_derive::errors::PipelineErrorKind;
use kona_genesis::SystemConfig;

#[async_trait]
pub trait CeloL2ChainProvider: CeloBatchValidationProviderDerive {
    /// The error type for the [L2ChainProvider].
    type Error: Display + Into<PipelineErrorKind>;

    /// Returns the [SystemConfig] by L2 number.
    async fn system_config_by_number(
        &mut self,
        number: u64,
        rollup_config: Arc<CeloRollupConfig>,
    ) -> Result<SystemConfig, <Self as CeloL2ChainProvider>::Error>;
}

/// A super-trait for [BatchValidationProvider] that binds `Self::Error` to have a conversion into
/// [PipelineErrorKind].
pub trait CeloBatchValidationProviderDerive: CeloBatchValidationProvider {}

// Auto-implement the [CeloBatchValidationProviderDerive] trait for all types that implement
// [CeloBatchValidationProvider] where the error can be converted into [PipelineErrorKind].
impl<T> CeloBatchValidationProviderDerive for T
where
    T: CeloBatchValidationProvider,
    <T as CeloBatchValidationProvider>::Error: Into<PipelineErrorKind>,
{
}
