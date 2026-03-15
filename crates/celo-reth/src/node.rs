//! Celo Node types configuration.

use crate::{
    celo_next_block_base_fee,
    primitives::{CeloBlock, CeloPrimitives},
    rpc::CeloEthApiBuilder,
    CeloEvmConfig,
};
use alloy_eips::eip1559::INITIAL_BASE_FEE;
use alloy_eips::eip2718::Encodable2718;
use alloy_rpc_types_engine::{ExecutionPayloadEnvelopeV2, ExecutionPayloadV1};
use celo_alloy_consensus::{CeloPooledTransaction, CeloTxEnvelope};
use op_alloy_rpc_types_engine::{
    OpExecutionData, OpExecutionPayloadEnvelopeV3, OpExecutionPayloadEnvelopeV4,
};
use reth_chainspec::{EthChainSpec, EthereumHardfork, EthereumHardforks};
use reth_consensus::{Consensus, ConsensusError, FullConsensus, HeaderValidator, ReceiptRootBloom};
use reth_consensus_common::validation::{
    validate_against_parent_hash_number, validate_against_parent_timestamp,
};
use reth_engine_local::LocalPayloadAttributesBuilder;
use reth_node_api::{
    BuiltPayload, EngineTypes, FullNodeComponents, NodePrimitives, PayloadAttributesBuilder,
    payload::PayloadTypes,
};
use reth_node_builder::{
    BuilderContext, DebugNode, Node, NodeAdapter,
    components::{
        BasicPayloadServiceBuilder, ComponentsBuilder, ConsensusBuilder, ExecutorBuilder,
    },
    node::{FullNodeTypes, NodeTypes},
    rpc::BasicEngineValidatorBuilder,
};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_consensus::OpBeaconConsensus;
use reth_optimism_forks::OpHardforks;
use reth_optimism_node::{
    OpEngineApiBuilder, OpEngineValidatorBuilder,
    node::{OpAddOns, OpNetworkBuilder, OpPayloadBuilder, OpPoolBuilder},
};
use reth_optimism_payload_builder::OpPayloadTypes;
use reth_optimism_primitives::DepositReceipt;
use reth_optimism_storage::OpStorage;
use reth_primitives_traits::{
    Block, GotExpected, RecoveredBlock, SealedBlock, SealedHeader, SignedTransaction,
};
use std::sync::Arc;

pub use reth_optimism_node::args::RollupArgs;

// ---------------------------------------------------------------------------
// CeloNode
// ---------------------------------------------------------------------------

/// Type configuration for a Celo reth node.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct CeloNode {
    /// The inner OP node (shared args, DA config, etc.).
    pub args: RollupArgs,
}

impl CeloNode {
    /// Creates a new instance with the given rollup args.
    pub fn new(args: RollupArgs) -> Self {
        Self { args }
    }
}

impl NodeTypes for CeloNode {
    type Primitives = CeloPrimitives;
    type ChainSpec = OpChainSpec;
    type Storage = OpStorage<crate::primitives::CeloTransactionSigned>;
    type Payload = CeloEngineTypes;
}

// ---------------------------------------------------------------------------
// CeloEngineTypes
// ---------------------------------------------------------------------------

/// Engine types for Celo, mirroring [`OpEngineTypes`] but with `Block = CeloBlock`
/// instead of `Block = OpBlock`.
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub struct CeloEngineTypes<T: PayloadTypes = OpPayloadTypes<CeloPrimitives>> {
    _marker: core::marker::PhantomData<T>,
}

impl<T: PayloadTypes<ExecutionData = OpExecutionData>> PayloadTypes for CeloEngineTypes<T> {
    type ExecutionData = T::ExecutionData;
    type BuiltPayload = T::BuiltPayload;
    type PayloadAttributes = T::PayloadAttributes;
    type PayloadBuilderAttributes = T::PayloadBuilderAttributes;

    fn block_to_payload(
        block: SealedBlock<
            <<Self::BuiltPayload as BuiltPayload>::Primitives as NodePrimitives>::Block,
        >,
    ) -> <T as PayloadTypes>::ExecutionData {
        OpExecutionData::from_block_unchecked(
            block.hash(),
            &block.into_block().into_ethereum_block(),
        )
    }
}

impl<T: PayloadTypes<ExecutionData = OpExecutionData>> EngineTypes for CeloEngineTypes<T>
where
    T::BuiltPayload: BuiltPayload<
            Primitives: NodePrimitives<
                Block = CeloBlock,
                SignedTx: SignedTransaction + Encodable2718,
            >,
        > + TryInto<ExecutionPayloadV1>
        + TryInto<ExecutionPayloadEnvelopeV2>
        + TryInto<OpExecutionPayloadEnvelopeV3>
        + TryInto<OpExecutionPayloadEnvelopeV4>,
{
    type ExecutionPayloadEnvelopeV1 = ExecutionPayloadV1;
    type ExecutionPayloadEnvelopeV2 = ExecutionPayloadEnvelopeV2;
    type ExecutionPayloadEnvelopeV3 = OpExecutionPayloadEnvelopeV3;
    type ExecutionPayloadEnvelopeV4 = OpExecutionPayloadEnvelopeV4;
    type ExecutionPayloadEnvelopeV5 = OpExecutionPayloadEnvelopeV4;
    type ExecutionPayloadEnvelopeV6 = OpExecutionPayloadEnvelopeV4;
}

/// Pooled-transaction type used in the Celo transaction pool.
type CeloPooledTx = reth_optimism_txpool::OpPooledTransaction<
    crate::primitives::CeloTransactionSigned,
    CeloPooledTransaction,
>;

// ---------------------------------------------------------------------------
// CeloPoolBuilder — OpPoolBuilder with CIP-64 tx type registered
// ---------------------------------------------------------------------------

/// Celo transaction pool builder.
///
/// Wraps [`OpPoolBuilder`] but registers CIP-64 (type `0x7b`) as an accepted
/// transaction type so that the inner [`EthTransactionValidator`] doesn't reject it.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct CeloPoolBuilder {
    inner: OpPoolBuilder<CeloPooledTx>,
}

impl<Node, Evm> reth_node_builder::components::PoolBuilder<Node, Evm> for CeloPoolBuilder
where
    Node: reth_node_builder::node::FullNodeTypes<
        Types: NodeTypes<
            ChainSpec: reth_optimism_forks::OpHardforks,
            Primitives = CeloPrimitives,
        >,
    >,
    Evm: reth_evm::ConfigureEvm<Primitives = CeloPrimitives> + Clone + 'static,
{
    type Pool = reth_optimism_txpool::OpTransactionPool<
        Node::Provider,
        reth_transaction_pool::blobstore::DiskFileBlobStore,
        Evm,
        CeloPooledTx,
    >;

    async fn build_pool(
        self,
        ctx: &BuilderContext<Node>,
        evm_config: Evm,
    ) -> eyre::Result<Self::Pool> {
        let pool_config_overrides = self.inner.pool_config_overrides;

        let blob_store = reth_node_builder::components::create_blob_store(ctx)?;
        let validator =
            reth_transaction_pool::TransactionValidationTaskExecutor::eth_builder(
                ctx.provider().clone(),
                evm_config,
            )
            .no_eip4844()
            .with_custom_tx_type(celo_alloy_consensus::CeloTxType::Cip64 as u8)
            .with_max_tx_input_bytes(ctx.config().txpool.max_tx_input_bytes)
            .kzg_settings(ctx.kzg_settings()?)
            .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
            .with_max_tx_gas_limit(ctx.config().txpool.max_tx_gas_limit)
            .with_minimum_priority_fee(ctx.config().txpool.minimum_priority_fee)
            .with_additional_tasks(
                pool_config_overrides
                    .additional_validation_tasks
                    .unwrap_or_else(|| ctx.config().txpool.additional_validation_tasks),
            )
            .build_with_tasks(ctx.task_executor().clone(), blob_store.clone())
            .map(|validator| {
                reth_optimism_txpool::OpTransactionValidator::new(validator)
                    // In --dev mode we can't require gas fees because we're unable to decode
                    // the L1 block info
                    .require_l1_data_gas_fee(!ctx.config().dev.dev)
            });

        let final_pool_config = pool_config_overrides.apply(ctx.pool_config());

        let transaction_pool =
            reth_node_builder::components::TxPoolBuilder::new(ctx)
                .with_validator(validator)
                .build_and_spawn_maintenance_task(blob_store, final_pool_config)?;

        tracing::info!(target: "reth::cli", "Transaction pool initialized");
        tracing::debug!(target: "reth::cli", "Spawned txpool maintenance task");

        Ok(transaction_pool)
    }
}

impl<N> Node<N> for CeloNode
where
    N: FullNodeTypes<Types = Self>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        CeloPoolBuilder,
        BasicPayloadServiceBuilder<OpPayloadBuilder>,
        OpNetworkBuilder,
        CeloExecutorBuilder,
        CeloConsensusBuilder,
    >;

    type AddOns = OpAddOns<
        NodeAdapter<N, <Self::ComponentsBuilder as reth_node_builder::NodeComponentsBuilder<N>>::Components>,
        CeloEthApiBuilder,
        OpEngineValidatorBuilder,
        OpEngineApiBuilder<OpEngineValidatorBuilder>,
        BasicEngineValidatorBuilder<OpEngineValidatorBuilder>,
    >;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        let RollupArgs { disable_txpool_gossip, discovery_v4, .. } = self.args;
        ComponentsBuilder::default()
            .node_types::<N>()
            .pool(CeloPoolBuilder::default())
            .executor(CeloExecutorBuilder)
            .payload(BasicPayloadServiceBuilder::new(OpPayloadBuilder::new(false)))
            .network(OpNetworkBuilder::new(disable_txpool_gossip, !discovery_v4))
            .consensus(CeloConsensusBuilder)
    }

    fn add_ons(&self) -> Self::AddOns {
        use reth_node_builder::rpc::RpcAddOns;
        OpAddOns::new(
            RpcAddOns::new(
                CeloEthApiBuilder,
                OpEngineValidatorBuilder::default(),
                OpEngineApiBuilder::default(),
                BasicEngineValidatorBuilder::default(),
                reth_node_builder::rpc::Identity::new(),
            ),
            Default::default(),
            Default::default(),
            self.args.sequencer.clone(),
            self.args.sequencer_headers.clone(),
            None, // historical_rpc
            self.args.enable_tx_conditional,
            0,    // min_suggested_priority_fee
        )
    }
}

// ---------------------------------------------------------------------------
// DebugNode — enables `--dev` mode (auto-mining) for CeloNode
// ---------------------------------------------------------------------------

impl<N> DebugNode<N> for CeloNode
where
    N: FullNodeComponents<Types = Self>,
{
    type RpcBlock = alloy_rpc_types_eth::Block<CeloTxEnvelope>;

    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> reth_node_api::BlockTy<Self> {
        rpc_block.into_consensus()
    }

    fn local_payload_attributes_builder(
        chain_spec: &Self::ChainSpec,
    ) -> impl PayloadAttributesBuilder<<Self::Payload as PayloadTypes>::PayloadAttributes> {
        LocalPayloadAttributesBuilder::new(Arc::new(chain_spec.clone()))
    }
}

// ---------------------------------------------------------------------------
// CeloExecutorBuilder
// ---------------------------------------------------------------------------

/// Celo EVM and executor builder.
#[derive(Debug, Copy, Clone, Default)]
#[non_exhaustive]
pub struct CeloExecutorBuilder;

impl<Node> ExecutorBuilder<Node> for CeloExecutorBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec: OpHardforks, Primitives = CeloPrimitives>>,
{
    type EVM = CeloEvmConfig<
        <Node::Types as NodeTypes>::ChainSpec,
        <Node::Types as NodeTypes>::Primitives,
    >;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        Ok(CeloEvmConfig::celo(ctx.chain_spec()))
    }
}

// ---------------------------------------------------------------------------
// CeloConsensus — OpBeaconConsensus with Celo-corrected base fee validation
// ---------------------------------------------------------------------------

/// Celo consensus validator.
///
/// Identical to [`OpBeaconConsensus`] except that `validate_header_against_parent`
/// uses [`celo_next_block_base_fee`] to compute the expected base fee, applying
/// Celo's 25 Gwei base fee floor (pre-Jovian) instead of the raw EIP-1559 formula.
#[derive(Debug, Clone)]
pub struct CeloConsensus<ChainSpec = OpChainSpec> {
    inner: OpBeaconConsensus<ChainSpec>,
    chain_spec: Arc<ChainSpec>,
}

impl<ChainSpec> CeloConsensus<ChainSpec> {
    /// Create a new [`CeloConsensus`].
    pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self {
            inner: OpBeaconConsensus::new(chain_spec.clone()),
            chain_spec,
        }
    }

    /// Returns the chain spec.
    pub fn chain_spec(&self) -> &Arc<ChainSpec> {
        &self.chain_spec
    }
}

impl<N, ChainSpec> FullConsensus<N> for CeloConsensus<ChainSpec>
where
    N: NodePrimitives<Receipt: DepositReceipt>,
    ChainSpec: EthChainSpec<Header = N::BlockHeader> + OpHardforks + core::fmt::Debug + Send + Sync,
{
    fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock<N::Block>,
        result: &reth_execution_types::BlockExecutionResult<N::Receipt>,
        receipt_root_bloom: Option<ReceiptRootBloom>,
    ) -> Result<(), ConsensusError> {
        <OpBeaconConsensus<ChainSpec> as FullConsensus<N>>::validate_block_post_execution(
            &self.inner,
            block,
            result,
            receipt_root_bloom,
        )
    }
}

impl<B, ChainSpec> Consensus<B> for CeloConsensus<ChainSpec>
where
    B: Block,
    ChainSpec: EthChainSpec<Header = B::Header> + OpHardforks + core::fmt::Debug + Send + Sync,
{
    fn validate_body_against_header(
        &self,
        body: &B::Body,
        header: &SealedHeader<B::Header>,
    ) -> Result<(), ConsensusError> {
        <OpBeaconConsensus<ChainSpec> as Consensus<B>>::validate_body_against_header(
            &self.inner,
            body,
            header,
        )
    }

    fn validate_block_pre_execution(
        &self,
        block: &SealedBlock<B>,
    ) -> Result<(), ConsensusError> {
        <OpBeaconConsensus<ChainSpec> as Consensus<B>>::validate_block_pre_execution(
            &self.inner,
            block,
        )
    }
}

impl<H, ChainSpec> HeaderValidator<H> for CeloConsensus<ChainSpec>
where
    H: reth_primitives_traits::BlockHeader,
    ChainSpec:
        EthChainSpec<Header = H> + EthereumHardforks + OpHardforks + core::fmt::Debug + Send + Sync,
{
    fn validate_header(&self, header: &SealedHeader<H>) -> Result<(), ConsensusError> {
        <OpBeaconConsensus<ChainSpec> as HeaderValidator<H>>::validate_header(&self.inner, header)
    }

    fn validate_header_against_parent(
        &self,
        header: &SealedHeader<H>,
        parent: &SealedHeader<H>,
    ) -> Result<(), ConsensusError> {
        // Standard hash/number and timestamp checks (from OpBeaconConsensus).
        validate_against_parent_hash_number(header.header(), parent)?;
        if self.chain_spec().is_bedrock_active_at_block(header.number()) {
            validate_against_parent_timestamp(header.header(), parent.header())?;
        }

        // Celo-specific base fee validation: applies the 25 Gwei floor pre-Jovian.
        if self.chain_spec().is_london_active_at_block(header.number()) {
            let base_fee =
                header.base_fee_per_gas().ok_or(ConsensusError::BaseFeeMissing)?;
            let expected = if self
                .chain_spec
                .ethereum_fork_activation(EthereumHardfork::London)
                .transitions_at_block(header.number())
            {
                Some(INITIAL_BASE_FEE)
            } else {
                celo_next_block_base_fee(self.chain_spec(), parent.header(), header.timestamp())
            };
            // If the expected base fee can be computed, validate it. When
            // `celo_next_block_base_fee` returns `None` (e.g. dev mode with
            // an empty genesis extra-data under Holocene), skip the check and
            // fall back to OP's default behavior (trusted sequencer).
            if let Some(expected) = expected {
                if expected != base_fee {
                    return Err(ConsensusError::BaseFeeDiff(GotExpected {
                        expected,
                        got: base_fee,
                    }));
                }
            }
        }

        // OP-specific blob gas validation (inlined from OpBeaconConsensus).
        // After Ecotone, blob_gas_used and excess_blob_gas must be present.
        // Before Jovian, blob_gas_used must be 0. excess_blob_gas must always be 0.
        if self.chain_spec.is_ecotone_active_at_timestamp(header.timestamp()) {
            let blob_gas_used =
                header.blob_gas_used().ok_or(ConsensusError::BlobGasUsedMissing)?;

            if !self.chain_spec.is_jovian_active_at_timestamp(header.timestamp())
                && blob_gas_used != 0
            {
                return Err(ConsensusError::BlobGasUsedDiff(GotExpected {
                    got: blob_gas_used,
                    expected: 0,
                }));
            }

            let excess_blob_gas =
                header.excess_blob_gas().ok_or(ConsensusError::ExcessBlobGasMissing)?;
            if excess_blob_gas != 0 {
                return Err(ConsensusError::ExcessBlobGasDiff {
                    diff: GotExpected { got: excess_blob_gas, expected: 0 },
                    parent_excess_blob_gas: parent.excess_blob_gas().unwrap_or(0),
                    parent_blob_gas_used: parent.blob_gas_used().unwrap_or(0),
                });
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CeloConsensusBuilder
// ---------------------------------------------------------------------------

/// Builder for [`CeloConsensus`].
#[derive(Debug, Copy, Clone, Default)]
#[non_exhaustive]
pub struct CeloConsensusBuilder;

impl<Node> ConsensusBuilder<Node> for CeloConsensusBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<
            ChainSpec: OpHardforks + EthereumHardforks,
            Primitives: NodePrimitives<Receipt: DepositReceipt>,
        >,
    >,
{
    type Consensus = Arc<CeloConsensus<<Node::Types as NodeTypes>::ChainSpec>>;

    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        Ok(Arc::new(CeloConsensus::new(ctx.chain_spec())))
    }
}
