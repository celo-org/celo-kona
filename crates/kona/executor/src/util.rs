//! Contains utilities for the L2 executor.

use alloy_consensus::{BlockHeader, Header};
use alloy_eips::eip1559::BaseFeeParams;
use alloy_primitives::Bytes;
use celo_alloy_rpc_types_engine::CeloPayloadAttributes;
use celo_genesis::CeloRollupConfig;
use kona_executor::{Eip1559ValidationError, ExecutorError, ExecutorResult};
use op_alloy_consensus::{decode_holocene_extra_data, encode_holocene_extra_data};

/// Parse Holocene [Header] extra data.
///
/// ## Takes
/// - `extra_data`: The extra data field of the [Header].
///
/// ## Returns
/// - `Ok(BaseFeeParams)`: The EIP-1559 parameters.
/// - `Err(ExecutorError::InvalidExtraData)`: If the extra data is invalid.
pub(crate) fn decode_holocene_eip_1559_params(header: &Header) -> ExecutorResult<BaseFeeParams> {
    let (elasticity, denominator) = decode_holocene_extra_data(header.extra_data())?;

    // Check for potential division by zero.
    if denominator == 0 {
        return Err(ExecutorError::InvalidExtraData(Eip1559ValidationError::ZeroDenominator));
    }

    Ok(BaseFeeParams {
        elasticity_multiplier: elasticity.into(),
        max_change_denominator: denominator.into(),
    })
}

/// Encode Holocene [Header] extra data.
///
/// ## Takes
/// - `config`: The [CeloRollupConfig] for the chain.
/// - `attributes`: The [CeloPayloadAttributes] for the block.
///
/// ## Returns
/// - `Ok(data)`: The encoded extra data.
/// - `Err(ExecutorError::MissingEIP1559Params)`: If the EIP-1559 parameters are missing.
pub(crate) fn encode_holocene_eip_1559_params(
    config: &CeloRollupConfig,
    attributes: &CeloPayloadAttributes,
) -> ExecutorResult<Bytes> {
    Ok(encode_holocene_extra_data(
        attributes
            .op_payload_attributes
            .eip_1559_params
            .ok_or(ExecutorError::MissingEIP1559Params)?,
        config.chain_op_config.post_canyon_params(),
    )?)
}

#[cfg(test)]
mod test {
    use super::{decode_holocene_eip_1559_params, encode_holocene_eip_1559_params};
    use alloy_consensus::Header;
    use alloy_primitives::{B64, b64, hex};
    use alloy_rpc_types_engine::PayloadAttributes;
    use celo_alloy_rpc_types_engine::CeloPayloadAttributes;
    use celo_genesis::CeloRollupConfig;
    use kona_genesis::{BaseFeeConfig, RollupConfig};
    use op_alloy_rpc_types_engine::OpPayloadAttributes;

    fn mock_payload(eip_1559_params: Option<B64>) -> CeloPayloadAttributes {
        CeloPayloadAttributes {
            op_payload_attributes: OpPayloadAttributes {
                payload_attributes: PayloadAttributes {
                    timestamp: 0,
                    prev_randao: Default::default(),
                    suggested_fee_recipient: Default::default(),
                    withdrawals: Default::default(),
                    parent_beacon_block_root: Default::default(),
                },
                transactions: None,
                no_tx_pool: None,
                gas_limit: None,
                eip_1559_params,
                min_base_fee: None,
            },
        }
    }

    #[test]
    fn test_decode_holocene_eip_1559_params() {
        let params = hex!("00BEEFBABE0BADC0DE");
        let mock_header = Header { extra_data: params.to_vec().into(), ..Default::default() };
        let params = decode_holocene_eip_1559_params(&mock_header).unwrap();

        assert_eq!(params.elasticity_multiplier, 0x0BAD_C0DE);
        assert_eq!(params.max_change_denominator, 0xBEEF_BABE);
    }

    #[test]
    fn test_decode_holocene_eip_1559_params_invalid_version() {
        let params = hex!("01BEEFBABE0BADC0DE");
        let mock_header = Header { extra_data: params.to_vec().into(), ..Default::default() };
        assert!(decode_holocene_eip_1559_params(&mock_header).is_err());
    }

    #[test]
    fn test_decode_holocene_eip_1559_params_invalid_denominator() {
        let params = hex!("00000000000BADC0DE");
        let mock_header = Header { extra_data: params.to_vec().into(), ..Default::default() };
        assert!(decode_holocene_eip_1559_params(&mock_header).is_err());
    }

    #[test]
    fn test_decode_holocene_eip_1559_params_invalid_length() {
        let params = hex!("00");
        let mock_header = Header { extra_data: params.to_vec().into(), ..Default::default() };
        assert!(decode_holocene_eip_1559_params(&mock_header).is_err());
    }

    #[test]
    fn test_encode_holocene_eip_1559_params_missing() {
        let cfg = CeloRollupConfig(RollupConfig {
            chain_op_config: BaseFeeConfig {
                eip1559_denominator: 32,
                eip1559_elasticity: 64,
                eip1559_denominator_canyon: 32,
            },
            ..Default::default()
        });
        let attrs = mock_payload(None);

        assert!(encode_holocene_eip_1559_params(&cfg, &attrs).is_err());
    }

    #[test]
    fn test_encode_holocene_eip_1559_params_default() {
        // Use different values for pre-canyon and canyon denominators to verify
        // we're using the canyon params (0x30) not pre-canyon (0x20)
        let cfg = CeloRollupConfig(RollupConfig {
            chain_op_config: BaseFeeConfig {
                eip1559_denominator: 32,        // 0x20 - pre-canyon
                eip1559_elasticity: 64,         // 0x40
                eip1559_denominator_canyon: 48, // 0x30 - canyon
            },
            ..Default::default()
        });
        let attrs = mock_payload(Some(B64::ZERO));

        // Should use canyon denominator (0x30), not pre-canyon (0x20)
        assert_eq!(
            encode_holocene_eip_1559_params(&cfg, &attrs).unwrap(),
            hex!("000000003000000040").to_vec()
        );
    }

    #[test]
    fn test_encode_holocene_eip_1559_params() {
        let cfg = CeloRollupConfig(RollupConfig {
            chain_op_config: BaseFeeConfig {
                eip1559_denominator: 32,
                eip1559_elasticity: 64,
                eip1559_denominator_canyon: 32,
            },
            ..Default::default()
        });
        let attrs = mock_payload(Some(b64!("0000004000000060")));

        assert_eq!(
            encode_holocene_eip_1559_params(&cfg, &attrs).unwrap(),
            hex!("000000004000000060").to_vec()
        );
    }
}
