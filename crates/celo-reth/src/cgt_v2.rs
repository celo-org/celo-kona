//! Deposits-only payload building for the CGTv2 activation block.

use celo_alloy_consensus::cgt_v2_group::CgtV2UpgradeGroup;
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, HeaderForPayload, MissingPayloadBehaviour, PayloadBuilder,
    PayloadConfig,
};
use reth_node_api::PayloadBuilderError;
use reth_optimism_payload_builder::OpPayloadAttrs;

/// Keeps pool transactions out of CGTv2 activation payloads.
/// Import and proving enforce the same rule through `OpBlockExecutionCtx`.
#[derive(Debug, Clone)]
pub struct CgtV2PayloadBuilder<B> {
    inner: B,
}

impl<B> CgtV2PayloadBuilder<B> {
    /// Wraps the node's payload builder.
    pub const fn new(inner: B) -> Self {
        Self { inner }
    }
}

fn prepare_activation_payload(attrs: &mut OpPayloadAttrs) -> Result<(), PayloadBuilderError> {
    let Some(transactions) = attrs.transactions.as_ref() else { return Ok(()) };
    if !transactions.iter().any(|tx| CgtV2UpgradeGroup::is_activation_encoded(tx)) {
        return Ok(());
    }
    // noTxPool controls pool selection only. Forced user transactions must be rejected too,
    // before or after the upgrade group. Match the executor's protocol post-exec exception.
    if transactions.iter().any(|tx| {
        !matches!(
            tx.first(),
            Some(&op_alloy_consensus::DEPOSIT_TX_TYPE_ID) |
                Some(&op_alloy_consensus::post_exec::POST_EXEC_TX_TYPE_ID)
        )
    }) {
        return Err(PayloadBuilderError::other(
            alloy_op_evm::block::OpBlockExecutionError::UnexpectedNonDepositTxInForkActivationBlock,
        ));
    }
    attrs.0.no_tx_pool = Some(true);
    Ok(())
}

impl<B: PayloadBuilder<Attributes = OpPayloadAttrs>> PayloadBuilder for CgtV2PayloadBuilder<B> {
    type Attributes = B::Attributes;
    type BuiltPayload = B::BuiltPayload;

    fn try_build(
        &self,
        mut args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> Result<BuildOutcome<Self::BuiltPayload>, PayloadBuilderError> {
        prepare_activation_payload(&mut args.config.attributes)?;
        self.inner.try_build(args)
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
        prepare_activation_payload(&mut config.attributes)?;
        self.inner.build_empty_payload(config)
    }
}
