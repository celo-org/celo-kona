//! Rollup Config Types

use alloy_hardforks::{EthereumHardfork, EthereumHardforks, ForkCondition};
use alloy_op_hardforks::{OpHardfork, OpHardforks};
use kona_genesis::RollupConfig;

/// The Rollup configuration.
#[derive(Default, Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct CeloRollupConfig {
    /// The OP rollup config.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub op_rollup_config: RollupConfig,
}

#[cfg(feature = "arbitrary")]
impl<'a> arbitrary::Arbitrary<'a> for CeloRollupConfig {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(Self { op_rollup_config: RollupConfig::arbitrary(u)? })
    }
}

impl EthereumHardforks for CeloRollupConfig {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        if fork <= EthereumHardfork::Berlin {
            // We assume that OP chains were launched with all forks before Berlin activated.
            ForkCondition::Block(0)
        } else if fork <= EthereumHardfork::Paris {
            // Bedrock activates all hardforks up to Paris.
            self.op_fork_activation(OpHardfork::Bedrock)
        } else if fork <= EthereumHardfork::Shanghai {
            // Canyon activates Shanghai hardfork.
            self.op_fork_activation(OpHardfork::Canyon)
        } else if fork <= EthereumHardfork::Cancun {
            // Ecotone activates Cancun hardfork.
            self.op_fork_activation(OpHardfork::Ecotone)
        } else if fork <= EthereumHardfork::Prague {
            // Isthmus activates Prague hardfork.
            self.op_fork_activation(OpHardfork::Isthmus)
        } else {
            ForkCondition::Never
        }
    }
}

impl OpHardforks for CeloRollupConfig {
    fn op_fork_activation(&self, fork: OpHardfork) -> ForkCondition {
        match fork {
            OpHardfork::Bedrock => ForkCondition::Block(0),
            OpHardfork::Regolith => self
                .op_rollup_config
                .hardforks
                .regolith_time
                .map(ForkCondition::Timestamp)
                .unwrap_or(self.op_fork_activation(OpHardfork::Canyon)),
            OpHardfork::Canyon => self
                .op_rollup_config
                .hardforks
                .canyon_time
                .map(ForkCondition::Timestamp)
                .unwrap_or(self.op_fork_activation(OpHardfork::Ecotone)),
            OpHardfork::Ecotone => self
                .op_rollup_config
                .hardforks
                .ecotone_time
                .map(ForkCondition::Timestamp)
                .unwrap_or(self.op_fork_activation(OpHardfork::Fjord)),
            OpHardfork::Fjord => self
                .op_rollup_config
                .hardforks
                .fjord_time
                .map(ForkCondition::Timestamp)
                .unwrap_or(self.op_fork_activation(OpHardfork::Granite)),
            OpHardfork::Granite => self
                .op_rollup_config
                .hardforks
                .granite_time
                .map(ForkCondition::Timestamp)
                .unwrap_or(self.op_fork_activation(OpHardfork::Holocene)),
            OpHardfork::Holocene => self
                .op_rollup_config
                .hardforks
                .holocene_time
                .map(ForkCondition::Timestamp)
                .unwrap_or(self.op_fork_activation(OpHardfork::Isthmus)),
            OpHardfork::Isthmus => self
                .op_rollup_config
                .hardforks
                .isthmus_time
                .map(ForkCondition::Timestamp)
                .unwrap_or(self.op_fork_activation(OpHardfork::Interop)),
            OpHardfork::Interop => self
                .op_rollup_config
                .hardforks
                .interop_time
                .map(ForkCondition::Timestamp)
                .unwrap_or(ForkCondition::Never),
        }
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for CeloRollupConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;

        // Remove the cel2_time field if it exists
        let mut json_obj =
            value.as_object().ok_or_else(|| serde::de::Error::custom("expected object"))?.clone();

        json_obj.remove("cel2_time");

        // Deserialize the RollupConfig from the cleaned JSON
        let op_rollup_config = RollupConfig::deserialize(serde_json::Value::Object(json_obj))
            .map_err(serde::de::Error::custom)?;

        Ok(Self { op_rollup_config })
    }
}

#[cfg(test)]
#[cfg(feature = "serde")]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloy_eips::BlockNumHash;
    use alloy_primitives::{Address, U256, address, b256};
    use kona_genesis::{
        ChainGenesis, FJORD_MAX_SEQUENCER_DRIFT, GRANITE_CHANNEL_TIMEOUT, HardForkConfig,
    };

    #[test]
    #[cfg(feature = "arbitrary")]
    fn test_arbitrary_rollup_config() {
        use arbitrary::Arbitrary;
        use rand::Rng;
        let mut bytes = [0u8; 1024];
        rand::rng().fill(bytes.as_mut_slice());
        CeloRollupConfig::arbitrary(&mut arbitrary::Unstructured::new(&bytes)).unwrap();
    }

    #[test]
    #[cfg(feature = "revm")]
    fn test_revm_spec_id() {
        // By default, the spec ID should be BEDROCK.
        let mut config = CeloRollupConfig {
            op_rollup_config: RollupConfig {
                hardforks: HardForkConfig { regolith_time: Some(10), ..Default::default() },
                ..Default::default()
            },
        };
        assert_eq!(config.op_rollup_config.spec_id(0), op_revm::OpSpecId::BEDROCK);
        assert_eq!(config.op_rollup_config.spec_id(10), op_revm::OpSpecId::REGOLITH);
        config.op_rollup_config.hardforks.canyon_time = Some(20);
        assert_eq!(config.op_rollup_config.spec_id(20), op_revm::OpSpecId::CANYON);
        config.op_rollup_config.hardforks.ecotone_time = Some(30);
        assert_eq!(config.op_rollup_config.spec_id(30), op_revm::OpSpecId::ECOTONE);
        config.op_rollup_config.hardforks.fjord_time = Some(40);
        assert_eq!(config.op_rollup_config.spec_id(40), op_revm::OpSpecId::FJORD);
        config.op_rollup_config.hardforks.holocene_time = Some(50);
        assert_eq!(config.op_rollup_config.spec_id(50), op_revm::OpSpecId::HOLOCENE);
        config.op_rollup_config.hardforks.isthmus_time = Some(60);
        assert_eq!(config.op_rollup_config.spec_id(60), op_revm::OpSpecId::ISTHMUS);
    }

    #[test]
    fn test_regolith_active() {
        let mut config = CeloRollupConfig::default();
        assert!(!config.op_rollup_config.is_regolith_active(0));
        config.op_rollup_config.hardforks.regolith_time = Some(10);
        assert!(config.op_rollup_config.is_regolith_active(10));
        assert!(!config.op_rollup_config.is_regolith_active(9));
    }

    #[test]
    fn test_canyon_active() {
        let mut config = CeloRollupConfig::default();
        assert!(!config.op_rollup_config.is_canyon_active(0));
        config.op_rollup_config.hardforks.canyon_time = Some(10);
        assert!(config.op_rollup_config.is_regolith_active(10));
        assert!(config.op_rollup_config.is_canyon_active(10));
        assert!(!config.op_rollup_config.is_canyon_active(9));
    }

    #[test]
    fn test_delta_active() {
        let mut config = CeloRollupConfig::default();
        assert!(!config.op_rollup_config.is_delta_active(0));
        config.op_rollup_config.hardforks.delta_time = Some(10);
        assert!(config.op_rollup_config.is_regolith_active(10));
        assert!(config.op_rollup_config.is_canyon_active(10));
        assert!(config.op_rollup_config.is_delta_active(10));
        assert!(!config.op_rollup_config.is_delta_active(9));
    }

    #[test]
    fn test_ecotone_active() {
        let mut config = CeloRollupConfig::default();
        assert!(!config.op_rollup_config.is_ecotone_active(0));
        config.op_rollup_config.hardforks.ecotone_time = Some(10);
        assert!(config.op_rollup_config.is_regolith_active(10));
        assert!(config.op_rollup_config.is_canyon_active(10));
        assert!(config.op_rollup_config.is_delta_active(10));
        assert!(config.op_rollup_config.is_ecotone_active(10));
        assert!(!config.op_rollup_config.is_ecotone_active(9));
    }

    #[test]
    fn test_fjord_active() {
        let mut config = CeloRollupConfig::default();
        assert!(!config.op_rollup_config.is_fjord_active(0));
        config.op_rollup_config.hardforks.fjord_time = Some(10);
        assert!(config.op_rollup_config.is_regolith_active(10));
        assert!(config.op_rollup_config.is_canyon_active(10));
        assert!(config.op_rollup_config.is_delta_active(10));
        assert!(config.op_rollup_config.is_ecotone_active(10));
        assert!(config.op_rollup_config.is_fjord_active(10));
        assert!(!config.op_rollup_config.is_fjord_active(9));
    }

    #[test]
    fn test_granite_active() {
        let mut config = CeloRollupConfig::default();
        assert!(!config.op_rollup_config.is_granite_active(0));
        config.op_rollup_config.hardforks.granite_time = Some(10);
        assert!(config.op_rollup_config.is_regolith_active(10));
        assert!(config.op_rollup_config.is_canyon_active(10));
        assert!(config.op_rollup_config.is_delta_active(10));
        assert!(config.op_rollup_config.is_ecotone_active(10));
        assert!(config.op_rollup_config.is_fjord_active(10));
        assert!(config.op_rollup_config.is_granite_active(10));
        assert!(!config.op_rollup_config.is_granite_active(9));
    }

    #[test]
    fn test_holocene_active() {
        let mut config = CeloRollupConfig::default();
        assert!(!config.op_rollup_config.is_holocene_active(0));
        config.op_rollup_config.hardforks.holocene_time = Some(10);
        assert!(config.op_rollup_config.is_regolith_active(10));
        assert!(config.op_rollup_config.is_canyon_active(10));
        assert!(config.op_rollup_config.is_delta_active(10));
        assert!(config.op_rollup_config.is_ecotone_active(10));
        assert!(config.op_rollup_config.is_fjord_active(10));
        assert!(config.op_rollup_config.is_granite_active(10));
        assert!(config.op_rollup_config.is_holocene_active(10));
        assert!(!config.op_rollup_config.is_holocene_active(9));
    }

    #[test]
    fn test_pectra_blob_schedule_active() {
        let mut config = CeloRollupConfig::default();
        config.op_rollup_config.hardforks.pectra_blob_schedule_time = Some(10);
        // Pectra blob schedule is a unique fork, not included in the hierarchical ordering. Its
        // activation does not imply the activation of any other forks.
        assert!(!config.op_rollup_config.is_regolith_active(10));
        assert!(!config.op_rollup_config.is_canyon_active(10));
        assert!(!config.op_rollup_config.is_delta_active(10));
        assert!(!config.op_rollup_config.is_ecotone_active(10));
        assert!(!config.op_rollup_config.is_fjord_active(10));
        assert!(!config.op_rollup_config.is_granite_active(10));
        assert!(!config.op_rollup_config.is_holocene_active(0));
        assert!(config.op_rollup_config.is_pectra_blob_schedule_active(10));
        assert!(!config.op_rollup_config.is_pectra_blob_schedule_active(9));
    }

    #[test]
    fn test_isthmus_active() {
        let mut config = CeloRollupConfig::default();
        assert!(!config.op_rollup_config.is_isthmus_active(0));
        config.op_rollup_config.hardforks.isthmus_time = Some(10);
        assert!(config.op_rollup_config.is_regolith_active(10));
        assert!(config.op_rollup_config.is_canyon_active(10));
        assert!(config.op_rollup_config.is_delta_active(10));
        assert!(config.op_rollup_config.is_ecotone_active(10));
        assert!(config.op_rollup_config.is_fjord_active(10));
        assert!(config.op_rollup_config.is_granite_active(10));
        assert!(config.op_rollup_config.is_holocene_active(10));
        assert!(!config.op_rollup_config.is_pectra_blob_schedule_active(10));
        assert!(config.op_rollup_config.is_isthmus_active(10));
        assert!(!config.op_rollup_config.is_isthmus_active(9));
    }

    #[test]
    fn test_interop_active() {
        let mut config = CeloRollupConfig::default();
        assert!(!config.op_rollup_config.is_interop_active(0));
        config.op_rollup_config.hardforks.interop_time = Some(10);
        assert!(config.op_rollup_config.is_regolith_active(10));
        assert!(config.op_rollup_config.is_canyon_active(10));
        assert!(config.op_rollup_config.is_delta_active(10));
        assert!(config.op_rollup_config.is_ecotone_active(10));
        assert!(config.op_rollup_config.is_fjord_active(10));
        assert!(config.op_rollup_config.is_granite_active(10));
        assert!(config.op_rollup_config.is_holocene_active(10));
        assert!(!config.op_rollup_config.is_pectra_blob_schedule_active(10));
        assert!(config.op_rollup_config.is_isthmus_active(10));
        assert!(config.op_rollup_config.is_interop_active(10));
        assert!(!config.op_rollup_config.is_interop_active(9));
    }

    #[test]
    fn test_is_first_fork_block() {
        let cfg = CeloRollupConfig {
            op_rollup_config: RollupConfig {
                hardforks: HardForkConfig {
                    regolith_time: Some(10),
                    canyon_time: Some(20),
                    delta_time: Some(30),
                    ecotone_time: Some(40),
                    fjord_time: Some(50),
                    granite_time: Some(60),
                    holocene_time: Some(70),
                    pectra_blob_schedule_time: Some(80),
                    isthmus_time: Some(90),
                    interop_time: Some(100),
                },
                block_time: 2,
                ..Default::default()
            },
        };

        // Regolith
        assert!(!cfg.op_rollup_config.is_first_regolith_block(8));
        assert!(cfg.op_rollup_config.is_first_regolith_block(10));
        assert!(!cfg.op_rollup_config.is_first_regolith_block(12));

        // Canyon
        assert!(!cfg.op_rollup_config.is_first_canyon_block(18));
        assert!(cfg.op_rollup_config.is_first_canyon_block(20));
        assert!(!cfg.op_rollup_config.is_first_canyon_block(22));

        // Delta
        assert!(!cfg.op_rollup_config.is_first_delta_block(28));
        assert!(cfg.op_rollup_config.is_first_delta_block(30));
        assert!(!cfg.op_rollup_config.is_first_delta_block(32));

        // Ecotone
        assert!(!cfg.op_rollup_config.is_first_ecotone_block(38));
        assert!(cfg.op_rollup_config.is_first_ecotone_block(40));
        assert!(!cfg.op_rollup_config.is_first_ecotone_block(42));

        // Fjord
        assert!(!cfg.op_rollup_config.is_first_fjord_block(48));
        assert!(cfg.op_rollup_config.is_first_fjord_block(50));
        assert!(!cfg.op_rollup_config.is_first_fjord_block(52));

        // Granite
        assert!(!cfg.op_rollup_config.is_first_granite_block(58));
        assert!(cfg.op_rollup_config.is_first_granite_block(60));
        assert!(!cfg.op_rollup_config.is_first_granite_block(62));

        // Holocene
        assert!(!cfg.op_rollup_config.is_first_holocene_block(68));
        assert!(cfg.op_rollup_config.is_first_holocene_block(70));
        assert!(!cfg.op_rollup_config.is_first_holocene_block(72));

        // Pectra blob schedule
        assert!(!cfg.op_rollup_config.is_first_pectra_blob_schedule_block(78));
        assert!(cfg.op_rollup_config.is_first_pectra_blob_schedule_block(80));
        assert!(!cfg.op_rollup_config.is_first_pectra_blob_schedule_block(82));

        // Isthmus
        assert!(!cfg.op_rollup_config.is_first_isthmus_block(88));
        assert!(cfg.op_rollup_config.is_first_isthmus_block(90));
        assert!(!cfg.op_rollup_config.is_first_isthmus_block(92));
    }

    #[test]
    fn test_alt_da_enabled() {
        let mut config = CeloRollupConfig::default();
        assert!(!config.op_rollup_config.is_alt_da_enabled());
        config.op_rollup_config.da_challenge_address = Some(Address::ZERO);
        assert!(!config.op_rollup_config.is_alt_da_enabled());
        config.op_rollup_config.da_challenge_address =
            Some(address!("0000000000000000000000000000000000000001"));
        assert!(config.op_rollup_config.is_alt_da_enabled());
    }

    #[test]
    fn test_granite_channel_timeout() {
        let mut config = CeloRollupConfig {
            op_rollup_config: RollupConfig {
                channel_timeout: 100,
                hardforks: HardForkConfig { granite_time: Some(10), ..Default::default() },
                ..Default::default()
            },
        };
        assert_eq!(config.op_rollup_config.channel_timeout(0), 100);
        assert_eq!(config.op_rollup_config.channel_timeout(10), GRANITE_CHANNEL_TIMEOUT);
        config.op_rollup_config.hardforks.granite_time = None;
        assert_eq!(config.op_rollup_config.channel_timeout(10), 100);
    }

    #[test]
    fn test_max_sequencer_drift() {
        let mut config = CeloRollupConfig {
            op_rollup_config: RollupConfig { max_sequencer_drift: 100, ..Default::default() },
        };
        assert_eq!(config.op_rollup_config.max_sequencer_drift(0), 100);
        config.op_rollup_config.hardforks.fjord_time = Some(10);
        assert_eq!(config.op_rollup_config.max_sequencer_drift(0), 100);
        assert_eq!(config.op_rollup_config.max_sequencer_drift(10), FJORD_MAX_SEQUENCER_DRIFT);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_deserialize_reference_rollup_config() {
        use kona_genesis::{
            AltDAConfig, ChainGenesis, DEFAULT_INTEROP_MESSAGE_EXPIRY_WINDOW,
            OP_MAINNET_BASE_FEE_CONFIG, RollupConfig, SystemConfig,
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
          "protocol_versions_address": "0x0000000000000000000000000000000000000000",
          "chain_op_config": {
            "eip1559Elasticity": "0x6",
            "eip1559Denominator": "0x32",
            "eip1559DenominatorCanyon": "0xfa"
          },
          "alt_da": {
            "da_challenge_contract_address": "0x0000000000000000000000000000000000000000",
            "da_commitment_type": "GenericCommitment",
            "da_challenge_window": 1,
            "da_resolve_window": 1
          }
        }
        "#;

        let expected = CeloRollupConfig {
            op_rollup_config: RollupConfig {
                genesis: ChainGenesis {
                    l1: BlockNumHash {
                        hash: b256!(
                            "481724ee99b1f4cb71d826e2ec5a37265f460e9b112315665c977f4050b0af54"
                        ),
                        number: 10,
                    },
                    l2: BlockNumHash {
                        hash: b256!(
                            "88aedfbf7dea6bfa2c4ff315784ad1a7f145d8f650969359c003bbed68c87631"
                        ),
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
                    }),
                },
                block_time: 2,
                max_sequencer_drift: 600,
                seq_window_size: 3600,
                channel_timeout: 300,
                granite_channel_timeout: GRANITE_CHANNEL_TIMEOUT,
                l1_chain_id: 3151908,
                l2_chain_id: 1337,
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
                protocol_versions_address: Address::ZERO,
                superchain_config_address: None,
                blobs_enabled_l1_timestamp: None,
                da_challenge_address: None,
                interop_message_expiry_window: DEFAULT_INTEROP_MESSAGE_EXPIRY_WINDOW,
                chain_op_config: OP_MAINNET_BASE_FEE_CONFIG,
                alt_da_config: Some(AltDAConfig {
                    da_challenge_address: Some(Address::ZERO),
                    da_challenge_window: Some(1),
                    da_resolve_window: Some(1),
                    da_commitment_type: Some(String::from("GenericCommitment")),
                }),
            },
        };

        let deserialized: CeloRollupConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(deserialized, expected);
    }

    #[test]
    fn test_rollup_config_unknown_field() {
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
          "protocol_versions_address": "0x0000000000000000000000000000000000000000",
          "chain_op_config": {
            "eip1559_elasticity": 100,
            "eip1559_denominator": 100,
            "eip1559_denominator_canyon": 100
          },
          "unknown_field": "unknown"
        }
        "#;

        let err = serde_json::from_str::<CeloRollupConfig>(raw).unwrap_err();
        assert_eq!(err.classify(), serde_json::error::Category::Data);
    }

    #[test]
    fn test_compute_block_number_from_time() {
        let cfg = CeloRollupConfig {
            op_rollup_config: RollupConfig {
                genesis: ChainGenesis { l2_time: 10, ..Default::default() },
                block_time: 2,
                ..Default::default()
            },
        };

        assert_eq!(cfg.op_rollup_config.block_number_from_timestamp(20), 5);
        assert_eq!(cfg.op_rollup_config.block_number_from_timestamp(30), 10);
    }
}
