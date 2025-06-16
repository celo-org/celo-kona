//! Environment utility functions for [CeloStatelessL2Builder].

use super::CeloStatelessL2Builder;
use crate::{constants::CELO_EIP_1559_BASE_FEE_FLOOR, util::decode_holocene_eip_1559_params};
use alloy_consensus::{BlockHeader, Header};
use alloy_eips::{eip1559::BaseFeeParams, eip7840::BlobParams};
use alloy_evm::{EvmEnv, EvmFactory};
use celo_alloy_rpc_types_engine::CeloPayloadAttributes;
use celo_genesis::CeloRollupConfig;
use celo_revm::constants::MAX_CODE_SIZE;
use kona_executor::{ExecutorError, ExecutorResult, TrieDBProvider};
use kona_mpt::TrieHinter;
use op_revm::OpSpecId;
use revm::{
    context::{BlockEnv, CfgEnv},
    context_interface::block::BlobExcessGasAndPrice,
};

impl<P, H, Evm> CeloStatelessL2Builder<'_, P, H, Evm>
where
    P: TrieDBProvider,
    H: TrieHinter,
    Evm: EvmFactory,
{
    /// Returns the active [`EvmEnv`] for the executor.
    pub(crate) fn evm_env(
        &self,
        spec_id: OpSpecId,
        parent_header: &Header,
        payload_attrs: &CeloPayloadAttributes,
        base_fee_params: &BaseFeeParams,
    ) -> ExecutorResult<EvmEnv<OpSpecId>> {
        let block_env =
            Self::prepare_block_env(spec_id, parent_header, payload_attrs, base_fee_params)?;
        let cfg_env = self.evm_cfg_env(
            payload_attrs
                .op_payload_attributes
                .payload_attributes
                .timestamp,
        );
        Ok(EvmEnv::new(cfg_env, block_env))
    }

    /// Returns the active [CfgEnv] for the executor.
    pub(crate) fn evm_cfg_env(&self, timestamp: u64) -> CfgEnv<OpSpecId> {
        let mut cfg_env = CfgEnv::new()
            .with_chain_id(self.config.op_rollup_config.l2_chain_id)
            .with_spec(self.config.op_rollup_config.spec_id(timestamp));
        cfg_env.limit_contract_code_size = Some(MAX_CODE_SIZE);
        cfg_env
    }

    /// Prepares a [BlockEnv] with the given [CeloPayloadAttributes].
    pub(crate) fn prepare_block_env(
        spec_id: OpSpecId,
        parent_header: &Header,
        payload_attrs: &CeloPayloadAttributes,
        base_fee_params: &BaseFeeParams,
    ) -> ExecutorResult<BlockEnv> {
        let blob_excess_gas_and_price = parent_header
            .maybe_next_block_excess_blob_gas(if spec_id.is_enabled_in(OpSpecId::ISTHMUS) {
                Some(BlobParams::prague())
            } else if spec_id.is_enabled_in(OpSpecId::ECOTONE) {
                Some(BlobParams::cancun())
            } else {
                None
            })
            .or_else(|| spec_id.is_enabled_in(OpSpecId::ECOTONE).then_some(0))
            .map(|e| BlobExcessGasAndPrice::new(e, spec_id.is_enabled_in(OpSpecId::ISTHMUS)));
        let mut next_block_base_fee = parent_header
            .next_block_base_fee(*base_fee_params)
            .unwrap_or_default();
        next_block_base_fee = core::cmp::max(next_block_base_fee, CELO_EIP_1559_BASE_FEE_FLOOR);

        let op_payload_attrs = &payload_attrs.op_payload_attributes.clone();
        Ok(BlockEnv {
            number: parent_header.number + 1,
            beneficiary: op_payload_attrs.payload_attributes.suggested_fee_recipient,
            timestamp: op_payload_attrs.payload_attributes.timestamp,
            gas_limit: op_payload_attrs
                .gas_limit
                .ok_or(ExecutorError::MissingGasLimit)?,
            basefee: next_block_base_fee,
            prevrandao: Some(op_payload_attrs.payload_attributes.prev_randao),
            blob_excess_gas_and_price,
            ..Default::default()
        })
    }

    /// Returns the active base fee parameters for the given payload attributes.
    pub(crate) fn active_base_fee_params(
        config: &CeloRollupConfig,
        parent_header: &Header,
        payload_attrs: &CeloPayloadAttributes,
    ) -> ExecutorResult<BaseFeeParams> {
        let op_payload_attrs = &payload_attrs.op_payload_attributes.clone();
        let base_fee_params = if config
            .op_rollup_config
            .is_holocene_active(op_payload_attrs.payload_attributes.timestamp)
        {
            // After Holocene activation, the base fee parameters are stored in the
            // `extraData` field of the parent header. If Holocene wasn't active in the
            // parent block, the default base fee parameters are used.
            config
                .op_rollup_config
                .is_holocene_active(parent_header.timestamp)
                .then(|| decode_holocene_eip_1559_params(parent_header))
                .transpose()?
                .unwrap_or(
                    config
                        .op_rollup_config
                        .chain_op_config
                        .as_canyon_base_fee_params(),
                )
        } else if config
            .op_rollup_config
            .is_canyon_active(op_payload_attrs.payload_attributes.timestamp)
        {
            // If the payload attribute timestamp is past canyon activation,
            // use the canyon base fee params from the rollup config.
            config
                .op_rollup_config
                .chain_op_config
                .as_canyon_base_fee_params()
        } else {
            // If the payload attribute timestamp is prior to canyon activation,
            // use the default base fee params from the rollup config.
            config.op_rollup_config.chain_op_config.as_base_fee_params()
        };

        Ok(base_fee_params)
    }
}
