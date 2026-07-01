//! A Celo-specific preimage server backend that serves the rollup config with Espresso settings.

use async_trait::async_trait;
use kona_preimage::{HintRouter, PreimageFetcher, PreimageKey, errors::PreimageOracleResult};
use kona_proof::boot::L2_ROLLUP_CONFIG_KEY;
use std::sync::Arc;

/// Wraps a preimage server backend, serving the Celo rollup config (Espresso settings included) for
/// the `L2_ROLLUP_CONFIG_KEY` preimage and delegating every other request to the inner backend.
///
/// The upstream local key-value store serializes the rollup config as an upstream `RollupConfig`,
/// which has no place for the Celo Espresso fields (`espresso_time` /
/// `batch_authenticator_address`), so they would be dropped before reaching the client.
/// Intercepting here — at the async [`PreimageFetcher`] layer, above the key-value store — lets the
/// host reuse the upstream `create_key_value_store` factory unchanged while still serving a
/// [`CeloRollupConfig`], instead of reimplementing that (crate-private) factory to swap in a custom
/// local store.
///
/// [`CeloRollupConfig`]: celo_genesis::CeloRollupConfig
#[allow(missing_debug_implementations)]
pub struct CeloConfigBackend<B> {
    /// The wrapped backend, used for every preimage request except the rollup config.
    inner: B,
    /// The Celo rollup config served under `L2_ROLLUP_CONFIG_KEY`, pre-serialized to JSON. `None`
    /// when no rollup config is configured, in which case the request is delegated to `inner`
    /// (which yields `KeyNotFound`), matching upstream behaviour.
    rollup_config_json: Option<Arc<Vec<u8>>>,
}

impl<B> CeloConfigBackend<B> {
    /// Creates a new [`CeloConfigBackend`] wrapping `inner`, serving `rollup_config_json` (when
    /// present) for the `L2_ROLLUP_CONFIG_KEY` preimage.
    pub const fn new(inner: B, rollup_config_json: Option<Arc<Vec<u8>>>) -> Self {
        Self { inner, rollup_config_json }
    }
}

#[async_trait]
impl<B> PreimageFetcher for CeloConfigBackend<B>
where
    B: PreimageFetcher + Send + Sync,
{
    async fn get_preimage(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
        if let Some(json) = self.rollup_config_json.as_ref() &&
            key == PreimageKey::new_local(L2_ROLLUP_CONFIG_KEY.to())
        {
            return Ok(json.as_ref().clone());
        }
        self.inner.get_preimage(key).await
    }
}

#[async_trait]
impl<B> HintRouter for CeloConfigBackend<B>
where
    B: HintRouter + Send + Sync,
{
    async fn route_hint(&self, hint: String) -> PreimageOracleResult<()> {
        self.inner.route_hint(hint).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kona_preimage::errors::PreimageOracleError;
    use tokio::sync::Mutex;

    /// A stub backend that records the keys delegated to it and returns a canned response.
    struct StubBackend {
        /// The value returned for every delegated preimage request (`None` => `KeyNotFound`).
        response: Option<Vec<u8>>,
        /// Keys the wrapper delegated to this backend.
        delegated: Mutex<Vec<PreimageKey>>,
    }

    #[async_trait]
    impl PreimageFetcher for StubBackend {
        async fn get_preimage(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
            self.delegated.lock().await.push(key);
            self.response.clone().ok_or(PreimageOracleError::KeyNotFound)
        }
    }

    #[async_trait]
    impl HintRouter for StubBackend {
        async fn route_hint(&self, _hint: String) -> PreimageOracleResult<()> {
            Ok(())
        }
    }

    fn rollup_config_key() -> PreimageKey {
        PreimageKey::new_local(L2_ROLLUP_CONFIG_KEY.to())
    }

    /// The rollup config is served from the wrapper and never delegated to the inner backend.
    #[tokio::test]
    async fn serves_rollup_config_without_delegating() {
        let cfg_bytes = b"celo-rollup-config".to_vec();
        let inner = StubBackend { response: None, delegated: Mutex::new(Vec::new()) };
        let backend = CeloConfigBackend::new(inner, Some(Arc::new(cfg_bytes.clone())));

        let served = backend.get_preimage(rollup_config_key()).await.expect("config served");
        assert_eq!(served, cfg_bytes);
        assert!(backend.inner.delegated.lock().await.is_empty());
    }

    /// Every key other than the rollup config is delegated to the inner backend unchanged.
    #[tokio::test]
    async fn delegates_other_keys_to_inner() {
        use kona_proof::boot::L1_HEAD_KEY;

        let inner_bytes = b"inner".to_vec();
        let inner =
            StubBackend { response: Some(inner_bytes.clone()), delegated: Mutex::new(Vec::new()) };
        let backend = CeloConfigBackend::new(inner, Some(Arc::new(b"cfg".to_vec())));

        let key = PreimageKey::new_local(L1_HEAD_KEY.to());
        let served = backend.get_preimage(key).await.expect("delegated value");
        assert_eq!(served, inner_bytes);
        assert_eq!(backend.inner.delegated.lock().await.as_slice(), &[key]);
    }

    /// With no rollup config configured, the request falls through to the inner backend — which
    /// reports `KeyNotFound`, the upstream behaviour for an unserved local key.
    #[tokio::test]
    async fn delegates_rollup_config_when_none_configured() {
        let inner = StubBackend { response: None, delegated: Mutex::new(Vec::new()) };
        let backend = CeloConfigBackend::new(inner, None);

        let err = backend.get_preimage(rollup_config_key()).await.unwrap_err();
        assert!(matches!(err, PreimageOracleError::KeyNotFound));
        assert_eq!(backend.inner.delegated.lock().await.as_slice(), &[rollup_config_key()]);
    }
}
