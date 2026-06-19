//! Rollup Config Types
use alloy_hardforks::{EthereumHardfork, EthereumHardforks, ForkCondition};
use alloy_op_hardforks::{OpHardfork, OpHardforks};
use alloy_primitives::Address;
use kona_genesis::RollupConfig;

/// The batch-auth lookback window, in L1 blocks.
///
/// The scan covers the batch's L1 origin block plus this many ancestor blocks — i.e. an
/// inclusive range of `window + 1` blocks (see `celo_derive::collect_authenticated_batches`).
/// Roughly 20 minutes of ancestry on Ethereum mainnet (12s blocks).
///
/// The constant is duplicated here (rather than added to upstream `kona-genesis`) because
/// celo-kona wraps upstream kona instead of patching it at source.
pub const BATCH_AUTH_LOOKBACK_WINDOW: u64 = 100;

/// Celo-specific Espresso batch-authentication configuration.
///
/// These fields live on [`CeloRollupConfig`] rather than on upstream
/// [`kona_genesis::RollupConfig`] / [`kona_genesis::HardForkConfig`], because celo-kona consumes
/// upstream kona unmodified. They are parsed from the same `rollup.json` document that produces
/// the wrapped [`RollupConfig`].
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CeloEspressoConfig {
    /// Activation timestamp (L2) for the Espresso event-only batch authorization hardfork.
    ///
    /// Pre-fork the derivation pipeline runs vanilla OP Stack semantics (sender-based
    /// authorization, no `BatchAuthenticator` event lookup). Post-fork batches must be
    /// authenticated by `BatchInfoAuthenticated` events; sender-based fallback is rejected.
    ///
    /// The per-L1-block decision in the data source is gated on the L1 origin time of the block
    /// being scanned, mirroring the upstream `ecotoneTime` precedent.
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Option::is_none"))]
    pub espresso_time: Option<u64>,
    /// Address of the `BatchAuthenticator` contract on L1. When set, enables event-based batch
    /// authentication instead of sender verification. The derivation pipeline scans L1 receipts
    /// for `BatchInfoAuthenticated` events emitted by this contract in a lookback window to
    /// authenticate batches.
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Option::is_none"))]
    pub batch_authenticator_address: Option<Address>,
}

#[derive(Debug, Clone, Eq, PartialEq, derive_more::Deref, derive_more::DerefMut)]
// #[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
/// CeloRollupConfig is thin wrapper around RollupConfig that ensures that deserialization can
/// handle config containing the cel2_time field.
///
/// It additionally carries Celo-specific Espresso batch-authentication settings
/// ([`CeloEspressoConfig`]) which have no home on the upstream [`RollupConfig`].
pub struct CeloRollupConfig {
    /// The wrapped upstream OP Stack rollup config. `Deref`/`DerefMut` target.
    #[deref]
    #[deref_mut]
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub op_rollup_config: RollupConfig,
    /// Celo-specific Espresso batch-authentication settings parsed from the same `rollup.json`.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub espresso: CeloEspressoConfig,
}

impl CeloRollupConfig {
    /// Constructs a [`CeloRollupConfig`] from an upstream [`RollupConfig`] with default (disabled)
    /// Espresso settings.
    pub const fn new(op_rollup_config: RollupConfig) -> Self {
        Self {
            op_rollup_config,
            espresso: CeloEspressoConfig { espresso_time: None, batch_authenticator_address: None },
        }
    }

    /// Returns true if event-based batch authentication is configured (a non-zero
    /// `BatchAuthenticator` address is present).
    ///
    /// When enabled, the derivation pipeline scans L1 receipts for `BatchInfoAuthenticated`
    /// events instead of relying on sender verification.
    pub fn is_batch_auth_enabled(&self) -> bool {
        self.espresso.batch_authenticator_address.is_some_and(|addr| !addr.is_zero())
    }

    /// Returns true if Espresso event-only batch authorization is active at the given L1 origin
    /// timestamp.
    ///
    /// Pre-fork the derivation pipeline runs vanilla OP Stack semantics (sender-based
    /// authorization, no `BatchAuthenticator` event lookup). Post-fork batches must be
    /// authenticated by `BatchInfoAuthenticated` events; sender-based fallback is rejected.
    ///
    /// This is intentionally orthogonal to the chained OP Stack hardforks and to
    /// [`Self::is_batch_auth_enabled`] (which only signals that a `BatchAuthenticator` contract
    /// address is configured).
    pub fn is_espresso_active(&self, timestamp: u64) -> bool {
        self.espresso.espresso_time.is_some_and(|t| timestamp >= t)
    }

    /// Resolves the Espresso batch-authentication parameters into a validated bundle suitable for
    /// the derivation data sources.
    ///
    /// Returns:
    /// - `Ok(None)` when no `espresso_time` is configured (Espresso is off; the derivation pipeline
    ///   runs vanilla OP Stack semantics). A `BatchAuthenticator` address without an
    ///   `espresso_time` is treated as "not yet scheduled" and likewise yields `None`.
    /// - `Ok(Some((authenticator_address, espresso_time)))` when both an `espresso_time` and a
    ///   non-zero authenticator address are configured.
    /// - `Err(MissingAuthenticatorAddress)` when `espresso_time` is set but no usable authenticator
    ///   address is configured (missing or zero) — otherwise no batch could ever be authorized
    ///   post-fork and derivation would silently stall at the fork boundary.
    ///
    /// The tuple bundles the two coupled parameters so downstream code cannot represent the
    /// illegal "fork scheduled but no authenticator" state.
    pub fn batch_auth_params(&self) -> Result<Option<(Address, u64)>, CeloEspressoConfigError> {
        let Some(espresso_time) = self.espresso.espresso_time else {
            return Ok(None);
        };
        let authenticator_address = self
            .espresso
            .batch_authenticator_address
            .filter(|addr| !addr.is_zero())
            .ok_or(CeloEspressoConfigError::MissingAuthenticatorAddress)?;
        Ok(Some((authenticator_address, espresso_time)))
    }

    /// Validates that the Espresso batch-authentication settings are internally consistent.
    ///
    /// Espresso activation (`espresso_time`) enables event-only batch authorization: post-fork a
    /// batch is authorized solely by a `BatchInfoAuthenticated` event from the configured
    /// `BatchAuthenticator`. If `espresso_time` is set but no usable authenticator address is
    /// configured (missing or zero), there is no way for any batch to be authorized post-fork, so
    /// derivation would silently stall at the fork boundary. Reject that combination up front so
    /// the misconfiguration surfaces as a clear error instead of a stuck pipeline.
    pub fn validate_espresso(&self) -> Result<(), CeloEspressoConfigError> {
        self.batch_auth_params().map(|_| ())
    }
}

/// Errors produced when validating the Celo Espresso batch-authentication configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, derive_more::Display)]
pub enum CeloEspressoConfigError {
    /// `espresso_time` is configured but no non-zero `batch_authenticator_address` is set, which
    /// would make post-fork derivation reject every batch.
    #[display(
        "espresso_time is set but batch_authenticator_address is missing or zero; \
         post-fork derivation would reject all batches"
    )]
    MissingAuthenticatorAddress,
}

impl core::error::Error for CeloEspressoConfigError {}

impl From<RollupConfig> for CeloRollupConfig {
    fn from(op_rollup_config: RollupConfig) -> Self {
        Self::new(op_rollup_config)
    }
}

// Custom deserialization that filters out cel2_time and the Celo Espresso fields before handing
// the remainder to the upstream `RollupConfig` deserializer (which would reject unknown keys
// otherwise), then re-attaches the Espresso settings.
#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for CeloRollupConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let mut json_obj =
            value.as_object().ok_or_else(|| serde::de::Error::custom("expected object"))?.clone();

        json_obj.remove("cel2_time");

        // Extract the Espresso fields (if present) before deserializing the upstream config,
        // since `RollupConfig` does not know about them.
        let espresso = CeloEspressoConfig {
            espresso_time: take_u64(&mut json_obj, "espresso_time")
                .map_err(serde::de::Error::custom)?,
            batch_authenticator_address: take_address(&mut json_obj, "batch_authenticator_address")
                .map_err(serde::de::Error::custom)?,
        };

        let op_rollup_config = RollupConfig::deserialize(serde_json::Value::Object(json_obj))
            .map_err(serde::de::Error::custom)?;

        Ok(Self { op_rollup_config, espresso })
    }
}

#[cfg(feature = "serde")]
fn take_u64(
    obj: &mut serde_json::Map<alloc::string::String, serde_json::Value>,
    key: &str,
) -> Result<Option<u64>, alloc::string::String> {
    match obj.remove(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => {
            serde_json::from_value(v).map(Some).map_err(|e| alloc::format!("invalid `{key}`: {e}"))
        }
    }
}

#[cfg(feature = "serde")]
fn take_address(
    obj: &mut serde_json::Map<alloc::string::String, serde_json::Value>,
    key: &str,
) -> Result<Option<Address>, alloc::string::String> {
    match obj.remove(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => {
            serde_json::from_value(v).map(Some).map_err(|e| alloc::format!("invalid `{key}`: {e}"))
        }
    }
}

// Even though derive_more::Deref ensures that op_fork_activation can be called on CeloRollupConfig,
// we need to add this minimal wrapper to have CeloRollupConfig satisfy OpHardforks trait bounds.
impl OpHardforks for CeloRollupConfig {
    fn op_fork_activation(&self, fork: OpHardfork) -> ForkCondition {
        self.op_rollup_config.op_fork_activation(fork)
    }
}

// Even though derive_more::Deref ensures that ethereum_fork_activation can be called on
// CeloRollupConfig, we need to add this minimal wrapper to have CeloRollupConfig satisfy
// EthereumHardforks trait bounds.
impl EthereumHardforks for CeloRollupConfig {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        self.op_rollup_config.ethereum_fork_activation(fork)
    }
}

#[cfg(test)]
#[cfg(feature = "serde")]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloy_eips::BlockNumHash;
    use alloy_primitives::{Address, U256, address, b256};
    use kona_genesis::{GRANITE_CHANNEL_TIMEOUT, HardForkConfig};

    #[test]
    #[cfg(feature = "serde")]
    fn test_deserialize_reference_rollup_config() {
        use kona_genesis::{
            AltDAConfig, ChainGenesis, FJORD_MAX_SEQUENCER_DRIFT, OP_MAINNET_BASE_FEE_CONFIG,
            RollupConfig, SystemConfig,
        };

        let raw: &str = r#"
        {
          "genesis": {
            "l1": {
              "hash": "0x481724ee99b1f4cb71d826e2ec5a37265f460e9b112315665c977f4050b0af54",
              "number": 10
            },
            "l2": {
              "hash": "0x88aedfbf7dea6bfa2c4ff315784ad1a7f145d8f650969359c003bbed68c87631",
              "number": 0
            },
            "l2_time": 1725557164,
            "system_config": {
              "batcherAddr": "0xc81f87a644b41e49b3221f41251f15c6cb00ce03",
              "overhead": "0x0000000000000000000000000000000000000000000000000000000000000000",
              "scalar": "0x00000000000000000000000000000000000000000000000000000000000f4240",
              "gasLimit": 30000000
            }
          },
          "block_time": 2,
          "max_sequencer_drift": 600,
          "seq_window_size": 3600,
          "channel_timeout": 300,
          "l1_chain_id": 3151908,
          "l2_chain_id": 1337,
          "regolith_time": 0,
          "canyon_time": 0,
          "delta_time": 0,
          "ecotone_time": 0,
          "fjord_time": 0,
          "cel2_time": 0,
          "batch_inbox_address": "0xff00000000000000000000000000000000042069",
          "deposit_contract_address": "0x08073dc48dde578137b8af042bcbc1c2491f1eb2",
          "l1_system_config_address": "0x94ee52a9d8edd72a85dea7fae3ba6d75e4bf1710",
          "chain_op_config": {
            "eip1559Elasticity": 6,
            "eip1559Denominator": 50,
            "eip1559DenominatorCanyon": 250
          },
          "alt_da": {
            "da_challenge_contract_address": "0x0000000000000000000000000000000000000000",
            "da_commitment_type": "GenericCommitment",
            "da_challenge_window": 1,
            "da_resolve_window": 1
          }
        }
        "#;

        let expected = CeloRollupConfig::new(RollupConfig {
            genesis: ChainGenesis {
                l1: BlockNumHash {
                    hash: b256!("481724ee99b1f4cb71d826e2ec5a37265f460e9b112315665c977f4050b0af54"),
                    number: 10,
                },
                l2: BlockNumHash {
                    hash: b256!("88aedfbf7dea6bfa2c4ff315784ad1a7f145d8f650969359c003bbed68c87631"),
                    number: 0,
                },
                l2_time: 1725557164,
                system_config: Some(SystemConfig {
                    batcher_address: address!("c81f87a644b41e49b3221f41251f15c6cb00ce03"),
                    overhead: U256::ZERO,
                    scalar: U256::from(0xf4240),
                    gas_limit: 30_000_000,
                    base_fee_scalar: None,
                    blob_base_fee_scalar: None,
                    eip1559_denominator: None,
                    eip1559_elasticity: None,
                    operator_fee_scalar: None,
                    operator_fee_constant: None,
                    min_base_fee: None,
                    da_footprint_gas_scalar: None,
                }),
            },
            block_time: 2,
            max_sequencer_drift: 600,
            seq_window_size: 3600,
            channel_timeout: 300,
            granite_channel_timeout: GRANITE_CHANNEL_TIMEOUT,
            fjord_max_sequencer_drift: FJORD_MAX_SEQUENCER_DRIFT,
            l1_chain_id: 3151908,
            l2_chain_id: 1337.into(),
            hardforks: HardForkConfig {
                regolith_time: Some(0),
                canyon_time: Some(0),
                delta_time: Some(0),
                ecotone_time: Some(0),
                fjord_time: Some(0),
                ..Default::default()
            },
            batch_inbox_address: address!("ff00000000000000000000000000000000042069"),
            deposit_contract_address: address!("08073dc48dde578137b8af042bcbc1c2491f1eb2"),
            l1_system_config_address: address!("94ee52a9d8edd72a85dea7fae3ba6d75e4bf1710"),
            superchain_config_address: None,
            blobs_enabled_l1_timestamp: None,
            da_challenge_address: None,
            chain_op_config: OP_MAINNET_BASE_FEE_CONFIG,
            alt_da_config: Some(AltDAConfig {
                da_challenge_address: Some(Address::ZERO),
                da_challenge_window: Some(1),
                da_resolve_window: Some(1),
                da_commitment_type: Some(String::from("GenericCommitment")),
            }),
        });

        let deserialized: CeloRollupConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(deserialized, expected);
        // No Espresso fields in this config => defaults (disabled).
        assert_eq!(deserialized.espresso, CeloEspressoConfig::default());
        assert!(!deserialized.is_batch_auth_enabled());
        assert!(!deserialized.is_espresso_active(u64::MAX));
    }

    #[test]
    fn test_is_espresso_active() {
        let mut cfg = CeloRollupConfig::new(RollupConfig::default());
        // Unset: never active.
        assert!(!cfg.is_espresso_active(0));
        assert!(!cfg.is_espresso_active(u64::MAX));
        // Set: boundary semantics match the OP Stack forks.
        cfg.espresso.espresso_time = Some(100);
        assert!(!cfg.is_espresso_active(99));
        assert!(cfg.is_espresso_active(100));
        assert!(cfg.is_espresso_active(101));
    }

    #[test]
    fn test_batch_auth_helpers() {
        let mut cfg = CeloRollupConfig::new(RollupConfig::default());
        assert!(!cfg.is_batch_auth_enabled());

        // Zero address does not enable batch auth.
        cfg.espresso.batch_authenticator_address = Some(Address::ZERO);
        assert!(!cfg.is_batch_auth_enabled());

        cfg.espresso.batch_authenticator_address =
            Some(address!("1234567890123456789012345678901234567890"));
        assert!(cfg.is_batch_auth_enabled());
    }

    #[test]
    fn test_validate_espresso() {
        // Nothing configured: valid (Espresso disabled).
        let mut cfg = CeloRollupConfig::new(RollupConfig::default());
        assert_eq!(cfg.validate_espresso(), Ok(()));

        // espresso_time set without an authenticator: invalid (would stall derivation).
        cfg.espresso.espresso_time = Some(100);
        assert_eq!(
            cfg.validate_espresso(),
            Err(CeloEspressoConfigError::MissingAuthenticatorAddress)
        );

        // Zero authenticator address is treated as unset: still invalid.
        cfg.espresso.batch_authenticator_address = Some(Address::ZERO);
        assert_eq!(
            cfg.validate_espresso(),
            Err(CeloEspressoConfigError::MissingAuthenticatorAddress)
        );

        // espresso_time + valid authenticator: valid.
        cfg.espresso.batch_authenticator_address =
            Some(address!("1234567890123456789012345678901234567890"));
        assert_eq!(cfg.validate_espresso(), Ok(()));

        // Authenticator configured without espresso_time is allowed (fork not yet active).
        let mut cfg2 = CeloRollupConfig::new(RollupConfig::default());
        cfg2.espresso.batch_authenticator_address =
            Some(address!("1234567890123456789012345678901234567890"));
        assert_eq!(cfg2.validate_espresso(), Ok(()));
    }

    #[test]
    fn test_batch_auth_params() {
        let auth = address!("1234567890123456789012345678901234567890");

        // No espresso_time: Espresso off => None (even if an authenticator is set).
        let mut cfg = CeloRollupConfig::new(RollupConfig::default());
        assert_eq!(cfg.batch_auth_params(), Ok(None));
        cfg.espresso.batch_authenticator_address = Some(auth);
        assert_eq!(cfg.batch_auth_params(), Ok(None));

        // espresso_time + valid authenticator => bundled 2-tuple (no lookback window).
        let mut cfg = CeloRollupConfig::new(RollupConfig::default());
        cfg.espresso.espresso_time = Some(42);
        cfg.espresso.batch_authenticator_address = Some(auth);
        assert_eq!(cfg.batch_auth_params(), Ok(Some((auth, 42))));

        // espresso_time without authenticator (or zero) => hard error.
        let mut cfg = CeloRollupConfig::new(RollupConfig::default());
        cfg.espresso.espresso_time = Some(42);
        assert_eq!(
            cfg.batch_auth_params(),
            Err(CeloEspressoConfigError::MissingAuthenticatorAddress)
        );
        cfg.espresso.batch_authenticator_address = Some(Address::ZERO);
        assert_eq!(
            cfg.batch_auth_params(),
            Err(CeloEspressoConfigError::MissingAuthenticatorAddress)
        );
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_deserialize_espresso_fields() {
        let raw: &str = r#"
        {
          "genesis": {
            "l1": { "hash": "0x481724ee99b1f4cb71d826e2ec5a37265f460e9b112315665c977f4050b0af54", "number": 10 },
            "l2": { "hash": "0x88aedfbf7dea6bfa2c4ff315784ad1a7f145d8f650969359c003bbed68c87631", "number": 0 },
            "l2_time": 1725557164,
            "system_config": {
              "batcherAddr": "0xc81f87a644b41e49b3221f41251f15c6cb00ce03",
              "overhead": "0x0000000000000000000000000000000000000000000000000000000000000000",
              "scalar": "0x00000000000000000000000000000000000000000000000000000000000f4240",
              "gasLimit": 30000000
            }
          },
          "block_time": 2,
          "max_sequencer_drift": 600,
          "seq_window_size": 3600,
          "channel_timeout": 300,
          "l1_chain_id": 3151908,
          "l2_chain_id": 1337,
          "regolith_time": 0,
          "cel2_time": 0,
          "espresso_time": 1234,
          "batch_authenticator_address": "0x00000000000000000000000000000000000000aa",
          "batch_inbox_address": "0xff00000000000000000000000000000000042069",
          "deposit_contract_address": "0x08073dc48dde578137b8af042bcbc1c2491f1eb2",
          "l1_system_config_address": "0x94ee52a9d8edd72a85dea7fae3ba6d75e4bf1710",
          "chain_op_config": { "eip1559Elasticity": 6, "eip1559Denominator": 50, "eip1559DenominatorCanyon": 250 }
        }
        "#;

        let cfg: CeloRollupConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(cfg.espresso.espresso_time, Some(1234));
        assert_eq!(
            cfg.espresso.batch_authenticator_address,
            Some(address!("00000000000000000000000000000000000000aa"))
        );
        assert!(cfg.is_batch_auth_enabled());
        assert!(cfg.is_espresso_active(1234));
        assert!(!cfg.is_espresso_active(1233));
    }

    /// The derived `Serialize` must emit the same flat shape the custom `Deserialize` consumes, so
    /// a config can survive a serialize -> deserialize round-trip. Before `#[serde(flatten)]` was
    /// applied, the named-field struct serialized to a nested `{op_rollup_config, espresso}` object
    /// that the flat deserializer could not read back. Exercises the Espresso fields specifically,
    /// since those are the ones the host/oracle round-trip (`CeloBootInfo::load`) relies on.
    #[test]
    #[cfg(feature = "serde")]
    fn test_serialize_deserialize_roundtrip_with_espresso() {
        let mut original = CeloRollupConfig::new(RollupConfig::default());
        original.espresso.espresso_time = Some(1234);
        original.espresso.batch_authenticator_address =
            Some(address!("00000000000000000000000000000000000000aa"));

        let json = serde_json::to_string(&original).unwrap();

        // The flattened serialization must place the Espresso keys at the top level (not nested
        // under an `espresso` object), matching the flat `rollup.json` shape.
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value.get("espresso").is_none(), "espresso must be flattened, not nested");
        assert_eq!(value.get("espresso_time").and_then(serde_json::Value::as_u64), Some(1234));
        assert!(value.get("batch_authenticator_address").is_some());

        let roundtripped: CeloRollupConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped, original);
    }

    /// Non-Espresso configs round-trip too: with both Espresso fields unset, `skip_serializing_if`
    /// omits the keys entirely and the deserializer restores the disabled defaults.
    #[test]
    #[cfg(feature = "serde")]
    fn test_serialize_deserialize_roundtrip_without_espresso() {
        let original = CeloRollupConfig::new(RollupConfig::default());

        let json = serde_json::to_string(&original).unwrap();

        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value.get("espresso").is_none());
        assert!(value.get("espresso_time").is_none());
        assert!(value.get("batch_authenticator_address").is_none());

        let roundtripped: CeloRollupConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped, original);
        assert_eq!(roundtripped.espresso, CeloEspressoConfig::default());
    }
}
