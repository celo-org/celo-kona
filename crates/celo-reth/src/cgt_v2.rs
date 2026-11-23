//! Deposits-only payload building and fallback for CGTv2 migration attempts.

use alloy_celo_evm::block::executor::CgtV2UpgradeFailed;
use alloy_eips::Decodable2718;
use alloy_evm::block::BlockExecutionError;
use celo_alloy_consensus::cgt_v2_group::{CGT_V2_STEPS, CgtV2GroupError, CgtV2UpgradeGroup};
use op_alloy_consensus::OpTxEnvelope;
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, HeaderForPayload, MissingPayloadBehaviour, PayloadBuilder,
    PayloadConfig,
};
use reth_node_api::PayloadBuilderError;
use reth_optimism_payload_builder::OpPayloadAttrs;

/// Keeps pool transactions out of CGTv2 builds and resolves optional migration attempts.
#[derive(Debug, Clone)]
pub struct CgtV2PayloadBuilder<B> {
    inner: B,
    chain_id: u64,
}

impl<B> CgtV2PayloadBuilder<B> {
    /// Wraps the node's payload builder for the selected chain.
    pub const fn new(inner: B, chain_id: u64) -> Self {
        Self { inner, chain_id }
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

impl<B> PayloadBuilder for CgtV2PayloadBuilder<B>
where
    B: PayloadBuilder<Attributes = OpPayloadAttrs>,
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
        let mut fallback_config = args.config.clone();
        fallback_config.attributes = fallback;
        // Do not stream the writes of a possibly discarded candidate into the engine's shared trie.
        args.state_root_handle = None;
        match self.inner.try_build(args) {
            Err(error) if upgrade_failed(&error).is_some() => {
                tracing::warn!(%error, parent = %fallback_config.parent_header.hash(), "Building CGTv2 fallback payload");
                // The forced list contains all L1 deposits. Build it from the same parent with
                // fresh execution state and retain the original payload job ID.
                self.inner.try_build(BuildArguments {
                    config: fallback_config,
                    cached_reads: Default::default(),
                    execution_cache: None,
                    state_root_handle: None,
                    // The failed build drops its cancellation token.
                    cancel: Default::default(),
                    best_payload: None,
                })
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
        config: PayloadConfig<Self::Attributes, HeaderForPayload<Self::BuiltPayload>>,
    ) -> Result<Self::BuiltPayload, PayloadBuilderError> {
        self.inner.build_empty_payload(config)
    }
}
