//! Contains the [`CeloEthereumDataSource`], a Celo-specific [`DataAvailabilityProvider`] that
//! mirrors kona's `EthereumDataSource` but routes through the Espresso-aware
//! [`CeloCalldataSource`] / [`CeloBlobSource`].
//!
//! celo-kona wraps upstream kona instead of patching it at source, so this factory duplicates the
//! ecotone calldata/blob dispatch from kona's `EthereumDataSource` while threading the Espresso
//! batch-authentication configuration (from celo-org/optimism#449) into the inner sources.

use crate::{batch_auth::BatchAuthConfig, blobs::CeloBlobSource, calldata::CeloCalldataSource};
use alloc::{boxed::Box, fmt::Debug};
use alloy_primitives::{Address, Bytes};
use async_trait::async_trait;
use celo_genesis::{CeloEspressoConfigError, CeloRollupConfig};
use kona_derive::{BlobProvider, ChainProvider, DataAvailabilityProvider, PipelineResult};
use kona_protocol::BlockInfo;

/// A factory for creating a Celo-aware Ethereum data source provider.
#[derive(Debug, Clone)]
pub struct CeloEthereumDataSource<C, B>
where
    C: ChainProvider + Send + Clone,
    B: BlobProvider + Send + Clone,
{
    /// The ecotone timestamp.
    pub ecotone_timestamp: Option<u64>,
    /// The blob source.
    pub blob_source: CeloBlobSource<C, B>,
    /// The calldata source.
    pub calldata_source: CeloCalldataSource<C>,
}

impl<C, B> CeloEthereumDataSource<C, B>
where
    C: ChainProvider + Send + Clone + Debug,
    B: BlobProvider + Send + Clone + Debug,
{
    /// Instantiates a new [`CeloEthereumDataSource`] from its inner sources.
    pub const fn new(
        blob_source: CeloBlobSource<C, B>,
        calldata_source: CeloCalldataSource<C>,
        ecotone_timestamp: Option<u64>,
    ) -> Self {
        Self { ecotone_timestamp, blob_source, calldata_source }
    }

    /// Instantiates a new [`CeloEthereumDataSource`] from parts, reading Espresso batch-auth
    /// settings off the [`CeloRollupConfig`].
    ///
    /// This is the Celo analogue of kona's `EthereumDataSource::new_from_parts`.
    ///
    /// Returns [`CeloEspressoConfigError::MissingAuthenticatorAddress`] if the rollup config
    /// schedules an Espresso fork (`espresso_time`) without a usable `BatchAuthenticator` address.
    /// The boot path validates this up front, but the check is repeated here so the invariant
    /// travels with the constructor.
    pub fn new_from_parts(
        provider: C,
        blobs: B,
        cfg: &CeloRollupConfig,
    ) -> Result<Self, CeloEspressoConfigError> {
        let batch_auth_config =
            cfg.batch_auth_params()?.map(|(authenticator_address, espresso_time)| {
                BatchAuthConfig { authenticator_address, espresso_time }
            });
        Ok(Self {
            ecotone_timestamp: cfg.op_rollup_config.hardforks.ecotone_time,
            blob_source: CeloBlobSource::new(
                provider.clone(),
                blobs,
                cfg.op_rollup_config.batch_inbox_address,
                batch_auth_config,
            ),
            calldata_source: CeloCalldataSource::new(
                provider,
                cfg.op_rollup_config.batch_inbox_address,
                batch_auth_config,
            ),
        })
    }
}

#[async_trait]
impl<C, B> DataAvailabilityProvider for CeloEthereumDataSource<C, B>
where
    C: ChainProvider + Send + Sync + Clone + Debug,
    B: BlobProvider + Send + Sync + Clone + Debug,
{
    type Item = Bytes;

    async fn next(
        &mut self,
        block_ref: &BlockInfo,
        batcher_address: Address,
    ) -> PipelineResult<Self::Item> {
        let ecotone_enabled = self.ecotone_timestamp.is_some_and(|e| block_ref.timestamp >= e);
        if ecotone_enabled {
            self.blob_source.next(block_ref, batcher_address).await
        } else {
            self.calldata_source.next(block_ref, batcher_address).await
        }
    }

    fn clear(&mut self) {
        self.blob_source.clear();
        self.calldata_source.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;
    use celo_genesis::CeloRollupConfig;
    use kona_derive::test_utils::{TestBlobProvider, TestChainProvider};
    use kona_genesis::RollupConfig;

    #[test]
    fn test_new_from_parts_disabled_batch_auth() {
        let cfg = CeloRollupConfig::new(RollupConfig::default());
        let ds = CeloEthereumDataSource::new_from_parts(
            TestChainProvider::default(),
            TestBlobProvider::default(),
            &cfg,
        )
        .unwrap();
        assert!(ds.calldata_source.batch_auth_config.is_none());
        assert!(ds.blob_source.batch_auth_config.is_none());
    }

    #[test]
    fn test_new_from_parts_enabled_batch_auth() {
        let mut cfg = CeloRollupConfig::new(RollupConfig::default());
        cfg.espresso.batch_authenticator_address =
            Some(address!("00000000000000000000000000000000000000aa"));
        cfg.espresso.espresso_time = Some(42);

        let ds = CeloEthereumDataSource::new_from_parts(
            TestChainProvider::default(),
            TestBlobProvider::default(),
            &cfg,
        )
        .unwrap();
        let calldata_cfg = ds.calldata_source.batch_auth_config.expect("batch auth config present");
        assert_eq!(
            calldata_cfg.authenticator_address,
            address!("00000000000000000000000000000000000000aa")
        );
        assert_eq!(calldata_cfg.espresso_time, 42);
        assert!(ds.blob_source.batch_auth_config.is_some());
    }

    #[test]
    fn test_new_from_parts_espresso_without_authenticator_errors() {
        // espresso_time set but no authenticator address: hard error.
        let mut cfg = CeloRollupConfig::new(RollupConfig::default());
        cfg.espresso.espresso_time = Some(42);
        let err = CeloEthereumDataSource::new_from_parts(
            TestChainProvider::default(),
            TestBlobProvider::default(),
            &cfg,
        )
        .unwrap_err();
        assert_eq!(err, CeloEspressoConfigError::MissingAuthenticatorAddress);
    }
}
