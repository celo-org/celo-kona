//! Adapter for using CeloBatchValidationProvider with L2ChainProvider & BatchValidationProvider
//! interface.
use crate::{CeloBatchValidationProvider, convert_celo_block_to_op_block};
use alloc::{boxed::Box, sync::Arc};
use async_trait::async_trait;
use kona_derive::{errors::PipelineErrorKind, traits::L2ChainProvider};
use kona_genesis::{RollupConfig, SystemConfig};
use kona_proof::errors::OracleProviderError;
use kona_protocol::{BatchValidationProvider, L2BlockInfo, to_system_config};
use op_alloy_consensus::OpBlock;

/// Newtype wrapper that adapts CeloBatchValidationProvider to BatchValidationProvider.
///
/// NOTE: When converting blocks, the adapter automatically filters out Celo-specific transaction
/// types that are not supported in the Optimism protocol
#[derive(Debug, Clone)]
pub struct CeloL2ChainAdapter<T: CeloBatchValidationProvider>(pub T);

#[async_trait]
impl<T> BatchValidationProvider for CeloL2ChainAdapter<T>
where
    T: CeloBatchValidationProvider + Send,
    OracleProviderError: From<<T as CeloBatchValidationProvider>::Error>,
{
    type Error = OracleProviderError;

    async fn l2_block_info_by_number(&mut self, number: u64) -> Result<L2BlockInfo, Self::Error> {
        self.0.l2_block_info_by_number(number).await.map_err(OracleProviderError::from)
    }

    async fn block_by_number(&mut self, number: u64) -> Result<OpBlock, Self::Error> {
        self.0
            .block_by_number(number)
            .await
            .map(convert_celo_block_to_op_block)
            .map_err(OracleProviderError::from)
    }
}

#[async_trait]
impl<T> L2ChainProvider for CeloL2ChainAdapter<T>
where
    T: CeloBatchValidationProvider + Send,
    OracleProviderError: From<<T as CeloBatchValidationProvider>::Error>,
    PipelineErrorKind: From<OracleProviderError>,
{
    type Error = OracleProviderError;

    async fn system_config_by_number(
        &mut self,
        number: u64,
        rollup_config: Arc<RollupConfig>,
    ) -> Result<SystemConfig, <Self as L2ChainProvider>::Error> {
        let celo_block =
            self.0.block_by_number(number).await.map_err(<Self as L2ChainProvider>::Error::from)?;
        // Construct the system config from the payload.
        // `CeloBlock`` can be safely converted to `OpBlock`` here
        // since `to_system_config`` depends solely on the block header (and not on transactions)
        to_system_config(&convert_celo_block_to_op_block(celo_block), rollup_config.as_ref())
            .map_err(OracleProviderError::OpBlockConversion)
    }
}
