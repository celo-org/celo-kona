//! Rollup Config Types
use alloy_hardforks::{EthereumHardfork, EthereumHardforks, ForkCondition};
use alloy_op_hardforks::{OpHardfork, OpHardforks};
use kona_genesis::RollupConfig;
#[derive(Debug, Clone, Eq, PartialEq, derive_more::Deref, derive_more::DerefMut)]
// #[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
/// CeloRollupConfig is thin wrapper around RollupConfig that ensures that deserialization can
/// handle config containing the cel2_time field.
pub struct CeloRollupConfig(pub RollupConfig);

// Custom deserialization that filters out cel2_time
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

        let op_rollup_config = RollupConfig::deserialize(serde_json::Value::Object(json_obj))
            .map_err(serde::de::Error::custom)?;

        Ok(Self(op_rollup_config))
    }
}

// Even though derive_more::Deref ensures that op_fork_activation can be called on CeloRollupConfig,
// we need to add this minimal wrapper to have CeloRollupConfig satisfy OpHardforks trait bounds.
impl OpHardforks for CeloRollupConfig {
    fn op_fork_activation(&self, fork: OpHardfork) -> ForkCondition {
        self.0.op_fork_activation(fork)
    }
}

// Even though derive_more::Deref ensures that ethereum_fork_activation can be called on
// CeloRollupConfig, we need to add this minimal wrapper to have CeloRollupConfig satisfy
// EthereumHardforks trait bounds.
impl EthereumHardforks for CeloRollupConfig {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        self.0.ethereum_fork_activation(fork)
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

        let expected = CeloRollupConfig(RollupConfig {
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
        });

        let deserialized: CeloRollupConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(deserialized, expected);
    }
}
