//! Deposits-only payload building and fallback for CGTv2 migration attempts.

use alloy_celo_evm::block::executor::CgtV2UpgradeFailed;
use alloy_eips::Decodable2718;
use alloy_evm::block::BlockExecutionError;
use alloy_primitives::B256;
use celo_alloy_consensus::cgt_v2_group::{
    CGT_V2_FLAG_SLOT, CGT_V2_L1_BLOCK, CGT_V2_STEPS, CgtV2GroupError, CgtV2UpgradeGroup,
};
use op_alloy_consensus::OpTxEnvelope;
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, HeaderForPayload, MissingPayloadBehaviour, PayloadBuilder,
    PayloadConfig,
};
use reth_node_api::PayloadBuilderError;
use reth_optimism_payload_builder::OpPayloadAttrs;
use reth_storage_api::StateProviderFactory;

/// Keeps pool transactions out of CGTv2 builds and resolves optional migration attempts.
#[derive(Debug, Clone)]
pub struct CgtV2PayloadBuilder<B, P> {
    inner: B,
    provider: P,
    chain_id: u64,
}

impl<B, P: StateProviderFactory> CgtV2PayloadBuilder<B, P> {
    /// Wraps the node's payload builder and provides access to the candidate parent's state.
    pub const fn new(inner: B, provider: P, chain_id: u64) -> Self {
        Self { inner, provider, chain_id }
    }

    fn is_active_at(&self, parent: B256) -> Result<bool, PayloadBuilderError> {
        let state = self.provider.state_by_block_hash(parent)?;
        let flag = state.storage(CGT_V2_L1_BLOCK, CGT_V2_FLAG_SLOT)?;
        Ok(flag.is_some_and(|flag| !flag.is_zero()))
    }
}

fn prepare_activation_payload(
    attrs: &mut OpPayloadAttrs,
    chain_id: u64,
) -> Result<Option<OpPayloadAttrs>, PayloadBuilderError> {
    let Some(transactions) = attrs.transactions.as_ref() else { return Ok(None) };
    let Some(start) =
        transactions.iter().position(|tx| CgtV2UpgradeGroup::is_activation_encoded(tx))
    else {
        return Ok(None);
    };

    // noTxPool controls pool selection only. The caller's forced list must also be deposits-only.
    if transactions.iter().any(|tx| tx.first() != Some(&op_alloy_consensus::DEPOSIT_TX_TYPE_ID)) {
        return Err(PayloadBuilderError::other(
            alloy_op_evm::block::OpBlockExecutionError::UnexpectedNonDepositTxInForkActivationBlock,
        ));
    }
    let invalid = || PayloadBuilderError::other(CgtV2GroupError::Invalid);
    let (prefix, group) = transactions.split_at(start);
    if prefix.is_empty() || group.len() != CGT_V2_STEPS {
        return Err(invalid());
    }

    // Only a well-formed group may be omitted when the parent is already active.
    for (step, encoded) in group.iter().enumerate() {
        let envelope = OpTxEnvelope::decode_2718_exact(encoded).map_err(|_| invalid())?;
        let deposit = envelope.as_deposit().ok_or_else(invalid)?;
        if CgtV2UpgradeGroup::validate(chain_id, deposit).map_err(PayloadBuilderError::other)? !=
            Some(step)
        {
            return Err(invalid());
        }
    }
    let allowance = CgtV2UpgradeGroup::gas_limit(chain_id).map_err(PayloadBuilderError::other)?;
    let gas_limit =
        attrs.gas_limit.and_then(|gas| gas.checked_sub(allowance)).ok_or_else(invalid)?;
    let fallback = OpPayloadAttrs(op_alloy_rpc_types_engine::OpPayloadAttributes {
        transactions: Some(prefix.to_vec()),
        gas_limit: Some(gas_limit),
        no_tx_pool: Some(true),
        ..attrs.0.clone()
    });
    attrs.0.no_tx_pool = Some(true);
    Ok(Some(fallback))
}

fn upgrade_failed(error: &PayloadBuilderError) -> Option<&CgtV2UpgradeFailed> {
    let PayloadBuilderError::EvmExecutionError(source) = error else { return None };
    CgtV2UpgradeFailed::from_block_error(source.downcast_ref::<BlockExecutionError>()?)
}

impl<B, P> PayloadBuilder for CgtV2PayloadBuilder<B, P>
where
    B: PayloadBuilder<Attributes = OpPayloadAttrs>,
    P: StateProviderFactory + Clone + 'static,
{
    type Attributes = B::Attributes;
    type BuiltPayload = B::BuiltPayload;

    fn try_build(
        &self,
        mut args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> Result<BuildOutcome<Self::BuiltPayload>, PayloadBuilderError> {
        let Some(fallback) =
            prepare_activation_payload(&mut args.config.attributes, self.chain_id)?
        else {
            return self.inner.try_build(args);
        };
        if self.is_active_at(args.config.parent_header.hash())? {
            args.config.attributes = fallback;
            return self.inner.try_build(args);
        }
        let mut fallback_config = args.config.clone();
        fallback_config.attributes = fallback;
        // Do not stream the writes of a possibly discarded candidate into the engine's shared trie.
        args.state_root_handle = None;
        match self.inner.try_build(args) {
            Err(error) if upgrade_failed(&error).is_some() => {
                tracing::warn!(%error, parent = %fallback_config.parent_header.hash(), "Building CGTv2 fallback payload");
                // The forced list contains all L1 deposits. Build it from the same parent with
                // fresh execution state and retain the original payload job ID.
                self.inner.build_empty_payload(fallback_config).map(BuildOutcome::Freeze)
            }
            result => result,
        }
    }

    fn on_missing_payload(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> MissingPayloadBehaviour<Self::BuiltPayload> {
        self.inner.on_missing_payload(args)
    }

    fn build_empty_payload(
        &self,
        mut config: PayloadConfig<Self::Attributes, HeaderForPayload<Self::BuiltPayload>>,
    ) -> Result<Self::BuiltPayload, PayloadBuilderError> {
        let Some(fallback) = prepare_activation_payload(&mut config.attributes, self.chain_id)?
        else {
            return self.inner.build_empty_payload(config);
        };
        if self.is_active_at(config.parent_header.hash())? {
            config.attributes = fallback;
            return self.inner.build_empty_payload(config);
        }
        match self.inner.build_empty_payload(config.clone()) {
            Err(error) if upgrade_failed(&error).is_some() => {
                config.attributes = fallback;
                self.inner.build_empty_payload(config)
            }
            result => result,
        }
    }
}
