//! Utility methods used by protocol types.

use alloy_consensus::{Transaction, Typed2718};
use alloy_primitives::B256;
use celo_alloy_consensus::CeloBlock;
use celo_genesis::CeloRollupConfig;
use kona_genesis::SystemConfig;
use kona_protocol::{
    L1BlockInfoBedrock, L1BlockInfoEcotone, L1BlockInfoIsthmus, L1BlockInfoTx,
    OpBlockConversionError,
};

/// Converts the [OpBlock] to a partial [SystemConfig].
pub fn to_system_config(
    block: &CeloBlock,
    celo_rollup_config: &CeloRollupConfig,
) -> Result<SystemConfig, OpBlockConversionError> {
    if block.header.number == celo_rollup_config.op_rollup_config.genesis.l2.number {
        if block.header.hash_slow() != celo_rollup_config.op_rollup_config.genesis.l2.hash {
            return Err(OpBlockConversionError::InvalidGenesisHash(
                celo_rollup_config.op_rollup_config.genesis.l2.hash,
                block.header.hash_slow(),
            ));
        }
        return celo_rollup_config
            .op_rollup_config
            .genesis
            .system_config
            .ok_or(OpBlockConversionError::MissingSystemConfigGenesis);
    }

    if block.body.transactions.is_empty() {
        return Err(OpBlockConversionError::EmptyTransactions(block.header.hash_slow()));
    }
    let Some(tx) = block.body.transactions[0].as_deposit() else {
        return Err(OpBlockConversionError::InvalidTxType(block.body.transactions[0].ty()));
    };

    let l1_info = L1BlockInfoTx::decode_calldata(tx.input().as_ref())?;
    let l1_fee_scalar = match l1_info {
        L1BlockInfoTx::Bedrock(L1BlockInfoBedrock { l1_fee_scalar, .. }) => l1_fee_scalar,
        L1BlockInfoTx::Ecotone(L1BlockInfoEcotone {
            base_fee_scalar,
            blob_base_fee_scalar,
            ..
        }) |
        L1BlockInfoTx::Isthmus(L1BlockInfoIsthmus {
            base_fee_scalar,
            blob_base_fee_scalar,
            ..
        }) => {
            // Translate Ecotone values back into encoded scalar if needed.
            // We do not know if it was derived from a v0 or v1 scalar,
            // but v1 is fine, a 0 blob base fee has the same effect.
            let mut buf = B256::ZERO;
            buf[0] = 0x01;
            buf[24..28].copy_from_slice(blob_base_fee_scalar.to_be_bytes().as_ref());
            buf[28..32].copy_from_slice(base_fee_scalar.to_be_bytes().as_ref());
            buf.into()
        }
    };

    let mut cfg = SystemConfig {
        batcher_address: l1_info.batcher_address(),
        overhead: l1_info.l1_fee_overhead(),
        scalar: l1_fee_scalar,
        gas_limit: block.header.gas_limit,
        ..Default::default()
    };

    // After holocene's activation, the EIP-1559 parameters are stored in the block header's nonce.
    if celo_rollup_config.op_rollup_config.is_holocene_active(block.header.timestamp) {
        let eip1559_params = &block.header.extra_data;

        if eip1559_params.len() != 9 {
            return Err(OpBlockConversionError::Eip1559DecodeError);
        }
        if eip1559_params[0] != 0 {
            return Err(OpBlockConversionError::Eip1559DecodeError);
        }

        cfg.eip1559_denominator = Some(u32::from_be_bytes(
            eip1559_params[1..5]
                .try_into()
                .map_err(|_| OpBlockConversionError::Eip1559DecodeError)?,
        ));
        cfg.eip1559_elasticity = Some(u32::from_be_bytes(
            eip1559_params[5..9]
                .try_into()
                .map_err(|_| OpBlockConversionError::Eip1559DecodeError)?,
        ));
    }

    if celo_rollup_config.op_rollup_config.is_isthmus_active(block.header.timestamp) {
        cfg.operator_fee_scalar = Some(l1_info.operator_fee_scalar());
        cfg.operator_fee_constant = Some(l1_info.operator_fee_constant());
    }

    Ok(cfg)
}
