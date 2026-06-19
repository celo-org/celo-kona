//! A Celo-specific local key-value store that serves the rollup config as a
//! [`CeloRollupConfig`], preserving the Celo Espresso settings that the upstream
//! [`SingleChainLocalInputs`] would drop.

use celo_genesis::CeloRollupConfig;
use kona_host::{KeyValueStore, Result as KvResult, single::SingleChainLocalInputs};
use kona_preimage::PreimageKey;
use kona_proof::boot::L2_ROLLUP_CONFIG_KEY;
use std::sync::Arc;

/// A [`KeyValueStore`] that wraps the upstream [`SingleChainLocalInputs`] and overrides the
/// `L2_ROLLUP_CONFIG_KEY` entry so the client receives a [`CeloRollupConfig`] (including the Celo
/// Espresso settings: `espresso_time` / `batch_authenticator_address`) instead of the upstream
/// [`kona_genesis::RollupConfig`].
///
/// The upstream store serializes the upstream `RollupConfig`, which has no notion of the Espresso
/// fields, so an Espresso chain booted via `--rollup-config-path` would otherwise reach the client
/// with batch authentication disabled. This mirrors op-program's `SingleChainLocalInputs`, which
/// serializes the whole `rollup.Config` (Espresso fields included) because there the Espresso
/// settings live directly on `rollup.Config`.
///
/// Every key other than `L2_ROLLUP_CONFIG_KEY` is delegated to the inner store unchanged.
#[derive(Debug)]
pub struct CeloSingleChainLocalInputs {
    /// The upstream local inputs store, used for every key except the rollup config.
    inner: SingleChainLocalInputs,
    /// The Celo rollup config served under `L2_ROLLUP_CONFIG_KEY`, pre-serialized to its flat
    /// JSON form. `None` when no rollup config path is configured (the inner store then yields
    /// `None` for that key, matching upstream behaviour).
    rollup_config_json: Option<Arc<Vec<u8>>>,
}

impl CeloSingleChainLocalInputs {
    /// Creates a new [`CeloSingleChainLocalInputs`] from the upstream inputs store and the resolved
    /// [`CeloRollupConfig`] (if a rollup config path was provided).
    pub fn new(inner: SingleChainLocalInputs, rollup_config: Option<CeloRollupConfig>) -> Self {
        let rollup_config_json = rollup_config
            .as_ref()
            .and_then(|cfg| serde_json::to_vec(cfg).ok())
            .map(Arc::new);
        Self { inner, rollup_config_json }
    }
}

impl KeyValueStore for CeloSingleChainLocalInputs {
    fn get(&self, key: alloy_primitives::B256) -> Option<Vec<u8>> {
        if let Ok(preimage_key) = PreimageKey::try_from(*key)
            && preimage_key.key_value() == L2_ROLLUP_CONFIG_KEY
        {
            return self.rollup_config_json.as_ref().map(|json| json.as_ref().clone());
        }
        self.inner.get(key)
    }

    fn set(&mut self, key: alloy_primitives::B256, value: Vec<u8>) -> KvResult<()> {
        self.inner.set(key, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, U256, address};
    use kona_host::single::SingleChainHost;
    use kona_proof::boot::L2_ROLLUP_CONFIG_KEY;

    /// The `B256` the client uses to query the rollup config preimage.
    fn rollup_config_kv_key() -> B256 {
        PreimageKey::new_local(L2_ROLLUP_CONFIG_KEY.to::<u64>()).into()
    }

    fn espresso_config() -> CeloRollupConfig {
        let mut cfg = CeloRollupConfig::new(Default::default());
        cfg.espresso.espresso_time = Some(1234);
        cfg.espresso.batch_authenticator_address =
            Some(address!("00000000000000000000000000000000000000aa"));
        cfg
    }

    /// The rollup-config preimage must round-trip back to a `CeloRollupConfig` with the Espresso
    /// fields intact — the behaviour the upstream store lacks (it serializes a bare `RollupConfig`).
    #[test]
    fn serves_celo_rollup_config_with_espresso_fields() {
        let cfg = espresso_config();
        let store = CeloSingleChainLocalInputs::new(
            SingleChainLocalInputs::new(SingleChainHost::default()),
            Some(cfg.clone()),
        );

        let bytes = store.get(rollup_config_kv_key()).expect("rollup config should be served");
        let decoded: CeloRollupConfig =
            serde_json::from_slice(&bytes).expect("served bytes must deserialize as CeloRollupConfig");

        assert_eq!(decoded, cfg);
        assert_eq!(decoded.espresso.espresso_time, Some(1234));
        assert_eq!(
            decoded.espresso.batch_authenticator_address,
            Some(address!("00000000000000000000000000000000000000aa"))
        );
    }

    /// With no rollup config configured, the override yields nothing for the rollup-config key
    /// (matching the upstream store, which serves no preimage in that case).
    #[test]
    fn serves_nothing_without_rollup_config() {
        let store = CeloSingleChainLocalInputs::new(
            SingleChainLocalInputs::new(SingleChainHost::default()),
            None,
        );
        assert!(store.get(rollup_config_kv_key()).is_none());
    }

    /// Non-rollup-config keys are delegated to the inner store. The L1 head is a fixed local input,
    /// so the wrapper must return exactly what the inner store holds.
    #[test]
    fn delegates_other_keys_to_inner() {
        use kona_proof::boot::L1_HEAD_KEY;

        let l1_head = B256::repeat_byte(0x42);
        let host = SingleChainHost { l1_head, ..Default::default() };
        let store = CeloSingleChainLocalInputs::new(
            SingleChainLocalInputs::new(host.clone()),
            Some(espresso_config()),
        );
        let inner = SingleChainLocalInputs::new(host);

        let key: B256 = PreimageKey::new_local(L1_HEAD_KEY.to::<u64>()).into();
        assert_eq!(store.get(key), inner.get(key));
        // Sanity: it is actually serving the L1 head bytes, not None.
        assert_eq!(store.get(key), Some(l1_head.to_vec()));
    }

    /// Guards against `U256::to::<u64>()` truncation assumptions: the keys we rely on fit in a u64.
    #[test]
    fn rollup_config_key_fits_u64() {
        assert!(L2_ROLLUP_CONFIG_KEY <= U256::from(u64::MAX));
    }
}
