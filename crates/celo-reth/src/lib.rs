//! Celo EVM configuration for reth.
//!
//! Bridges the Celo EVM ([`CeloEvmFactory`]) with reth's node framework,
//! analogous to `reth-optimism-evm` for vanilla OP Stack chains.

// Most deps in this crate are only used by std-only modules (node, pool,
// payload, rpc, chainspec) or the binary target. Only warn on unused deps in
// std builds to avoid spurious warnings under `cargo hack check
// --no-default-features`.
#![cfg_attr(all(not(test), feature = "std"), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

// Binary-only deps (used by src/bin/celo_reth.rs but not by the library).
// Suppress `unused_crate_dependencies` on the std library compilation unit.
#[cfg(feature = "std")]
use {
    clap as _, reth_cli_runner as _, reth_cli_util as _, reth_node_metrics as _,
    reth_optimism_cli as _, reth_provider as _, reth_tracing as _, reth_transaction_pool as _,
    tracing as _,
};

use alloc::sync::Arc;
use alloy_consensus::{BlockHeader, Header};
use alloy_evm::{
    EvmFactory, FromRecoveredTx, FromTxWithEncoded, TransactionEnvMut as TransactionEnv,
    block::BlockExecutorFactory, precompiles::PrecompilesMap,
};
use alloy_op_evm::block::{OpTxEnv, receipt_builder::OpReceiptBuilder};
use core::fmt::Debug;
use op_alloy_consensus::EIP1559ParamError;
use op_revm::OpSpecId;
use reth_chainspec::EthChainSpec;
use reth_evm::{
    ConfigureEvm, Database, EvmEnv,
    eth::NextEvmEnvAttributes,
    execute::{BasicBlockBuilder, BlockBuilder, BlockExecutor},
};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_forks::OpHardforks;
use reth_primitives_traits::{NodePrimitives, SealedBlock, SealedHeader, SignedTransaction};
use revm::context::BlockEnv;

pub mod primitives;
pub mod receipt;
pub mod receipts;
pub mod signed_tx;

#[cfg(feature = "std")]
pub mod node;

#[cfg(feature = "std")]
pub mod pool;

#[cfg(feature = "std")]
pub mod payload;

#[cfg(feature = "std")]
pub mod rpc;

#[cfg(feature = "std")]
pub mod chainspec;

#[cfg(feature = "std")]
pub mod state_import;

#[cfg(all(test, feature = "std"))]
pub(crate) mod test_utils;

pub use primitives::*;
pub use receipt::CeloReceipt;
pub use receipts::CeloRethReceiptBuilder;

// Re-export block assembler and execution context from op-reth (same for Celo as OP Stack).
pub use alloy_op_evm::{
    OpBlockExecutor,
    post_exec::{PostExecEvmFactoryHooks, PostExecExecutorExt},
};
pub use reth_optimism_evm::{
    ConfigurePostExecEvm, OpBlockAssembler, OpBlockExecutionCtx, OpBlockExecutorFactory,
    OpNextBlockEnvAttributes, PostExecMode, l1,
};

// Re-export Celo EVM types.
pub use alloy_celo_evm::{CeloEvm, CeloEvmFactory};
pub use alloy_op_evm::post_exec::PostExecEvmFactoryAdapter;

use reth_optimism_primitives::DepositReceipt;

#[cfg(feature = "std")]
use {
    reth_evm::{ConfigureEngineEvm, EvmEnvFor, ExecutableTxIterator, ExecutionCtxFor},
    reth_optimism_payload_builder::OpExecData,
    reth_primitives_traits::TxTy,
};

pub use celo_revm::constants::CELO_EIP_1559_BASE_FEE_FLOOR as CELO_BASE_FEE_FLOOR;

/// Compute the next block's base fee for Celo.
///
/// Pre-Jovian, applies the 25 Gwei base fee floor. Post-Jovian, the chain spec already
/// enforces `min_base_fee` from the parent's extra data.
pub fn celo_next_block_base_fee<H, ChainSpec>(
    chain_spec: &Arc<ChainSpec>,
    parent: &H,
    timestamp: u64,
) -> Option<u64>
where
    H: alloy_consensus::BlockHeader,
    ChainSpec: reth_chainspec::EthChainSpec<Header = H> + OpHardforks,
{
    let raw = chain_spec.next_block_base_fee(parent, timestamp)?;
    if chain_spec.is_jovian_active_at_timestamp(timestamp) {
        Some(raw)
    } else {
        Some(raw.max(CELO_BASE_FEE_FLOOR))
    }
}

/// Celo EVM configuration (analogous to [`OpEvmConfig`](reth_optimism_evm::OpEvmConfig)).
#[derive(Debug)]
pub struct CeloEvmConfig<
    ChainSpec = OpChainSpec,
    N: NodePrimitives = CeloPrimitives,
    R = CeloRethReceiptBuilder,
    EvmFactory = PostExecEvmFactoryAdapter<CeloEvmFactory>,
> {
    /// Inner [`OpBlockExecutorFactory`].
    pub executor_factory: OpBlockExecutorFactory<R, Arc<ChainSpec>, EvmFactory>,
    /// Block assembler (shared with OP Stack).
    pub block_assembler: OpBlockAssembler<ChainSpec>,
    #[doc(hidden)]
    pub _pd: core::marker::PhantomData<N>,
}

impl<ChainSpec, N: NodePrimitives, R: Clone, EvmFactory: Clone> Clone
    for CeloEvmConfig<ChainSpec, N, R, EvmFactory>
{
    fn clone(&self) -> Self {
        Self {
            executor_factory: self.executor_factory.clone(),
            block_assembler: self.block_assembler.clone(),
            _pd: self._pd,
        }
    }
}

impl<ChainSpec: OpHardforks> CeloEvmConfig<ChainSpec> {
    /// Creates a new [`CeloEvmConfig`] with the given chain spec.
    pub fn celo(chain_spec: Arc<ChainSpec>) -> Self {
        Self::celo_with_blocklist(
            chain_spec,
            alloy_celo_evm::blocklist::FeeCurrencyBlocklist::default(),
        )
    }

    /// Creates a new [`CeloEvmConfig`] with the given chain spec and shared fee currency blocklist.
    pub fn celo_with_blocklist(
        chain_spec: Arc<ChainSpec>,
        blocklist: alloy_celo_evm::blocklist::FeeCurrencyBlocklist,
    ) -> Self {
        // Create a shared CIP-64 storage so the EVM and receipt builder can communicate.
        let cip64_storage = alloy_celo_evm::cip64_storage::Cip64Storage::default();
        let receipt_builder = CeloRethReceiptBuilder::new(cip64_storage.clone());
        let evm_factory =
            CeloEvmFactory::with_cip64_storage(cip64_storage).with_blocklist(blocklist);
        Self {
            block_assembler: OpBlockAssembler::new(chain_spec.clone()),
            executor_factory: OpBlockExecutorFactory::new(
                receipt_builder,
                chain_spec,
                PostExecEvmFactoryAdapter::new(evm_factory),
            ),
            _pd: core::marker::PhantomData,
        }
    }
}

impl<ChainSpec, N, R, EvmFactory> CeloEvmConfig<ChainSpec, N, R, EvmFactory>
where
    ChainSpec: OpHardforks,
    N: NodePrimitives,
{
    /// Returns the chain spec associated with this configuration.
    pub const fn chain_spec(&self) -> &Arc<ChainSpec> {
        self.executor_factory.spec()
    }

    /// Builds a block execution context with an optional post-exec mode override.
    pub fn context_for_block_with_post_exec_mode(
        &self,
        block: &SealedBlock<N::Block>,
        post_exec_mode: Option<PostExecMode>,
    ) -> OpBlockExecutionCtx {
        OpBlockExecutionCtx {
            parent_hash: block.header().parent_hash(),
            parent_beacon_block_root: block.header().parent_beacon_block_root(),
            extra_data: block.header().extra_data().clone(),
            post_exec_mode: post_exec_mode.unwrap_or_default(),
        }
    }

    /// Builds a next-block execution context with the provided post-exec mode.
    pub fn context_for_next_block_with_post_exec_mode(
        &self,
        parent: &SealedHeader<N::BlockHeader>,
        attributes: OpNextBlockEnvAttributes,
        post_exec_mode: PostExecMode,
    ) -> OpBlockExecutionCtx {
        OpBlockExecutionCtx {
            parent_hash: parent.hash(),
            parent_beacon_block_root: attributes.parent_beacon_block_root,
            extra_data: attributes.extra_data,
            post_exec_mode,
        }
    }
}

impl<ChainSpec, N, R, EvmF> ConfigureEvm for CeloEvmConfig<ChainSpec, N, R, EvmF>
where
    ChainSpec: EthChainSpec<Header = Header> + OpHardforks,
    N: NodePrimitives<
            Receipt = R::Receipt,
            SignedTx = R::Transaction,
            BlockHeader = Header,
            BlockBody = alloy_consensus::BlockBody<R::Transaction>,
            Block = alloy_consensus::Block<R::Transaction>,
        >,
    R: OpReceiptBuilder<Receipt: DepositReceipt, Transaction: SignedTransaction>,
    EvmF: EvmFactory<
            Tx: FromRecoveredTx<R::Transaction>
                    + FromTxWithEncoded<R::Transaction>
                    + TransactionEnv
                    + OpTxEnv,
            Spec = OpSpecId,
            BlockEnv = BlockEnv,
            Precompiles = PrecompilesMap,
        > + Debug,
    OpBlockExecutorFactory<R, Arc<ChainSpec>, EvmF>: for<'a> BlockExecutorFactory<
            EvmFactory = EvmF,
            ExecutionCtx<'a> = OpBlockExecutionCtx,
            Transaction = R::Transaction,
            Receipt = R::Receipt,
        >,
    Self: Send + Sync + Unpin + Clone + 'static,
{
    type Primitives = N;
    type Error = EIP1559ParamError;
    type NextBlockEnvCtx = OpNextBlockEnvAttributes;
    type BlockExecutorFactory = OpBlockExecutorFactory<R, Arc<ChainSpec>, EvmF>;
    type BlockAssembler = OpBlockAssembler<ChainSpec>;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        &self.executor_factory
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        &self.block_assembler
    }

    fn evm_env(&self, header: &Header) -> Result<EvmEnv<OpSpecId>, Self::Error> {
        Ok(alloy_op_evm::evm_env_for_op_block(
            header,
            self.chain_spec(),
            self.chain_spec().chain().id(),
        ))
    }

    fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnv<OpSpecId>, Self::Error> {
        Ok(alloy_op_evm::evm_env_for_op_next_block(
            parent,
            NextEvmEnvAttributes {
                timestamp: attributes.timestamp,
                suggested_fee_recipient: attributes.suggested_fee_recipient,
                prev_randao: attributes.prev_randao,
                gas_limit: attributes.gas_limit,
                slot_number: None,
            },
            celo_next_block_base_fee(self.chain_spec(), parent, attributes.timestamp)
                .unwrap_or_default(),
            self.chain_spec(),
            self.chain_spec().chain().id(),
        ))
    }

    fn context_for_block(
        &self,
        block: &'_ SealedBlock<N::Block>,
    ) -> Result<OpBlockExecutionCtx, Self::Error> {
        Ok(OpBlockExecutionCtx {
            parent_hash: block.header().parent_hash(),
            parent_beacon_block_root: block.header().parent_beacon_block_root(),
            extra_data: block.header().extra_data().clone(),
            // SDM is unscheduled on Celo (`RollupConfig::is_sdm_active` returns false),
            // so post-exec verification never runs.
            post_exec_mode: PostExecMode::default(),
        })
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader<N::BlockHeader>,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<OpBlockExecutionCtx, Self::Error> {
        Ok(OpBlockExecutionCtx {
            parent_hash: parent.hash(),
            parent_beacon_block_root: attributes.parent_beacon_block_root,
            extra_data: attributes.extra_data,
            post_exec_mode: PostExecMode::default(),
        })
    }
}

impl<ChainSpec, N, R, F> ConfigurePostExecEvm
    for CeloEvmConfig<ChainSpec, N, R, PostExecEvmFactoryAdapter<F>>
where
    ChainSpec: EthChainSpec<Header = Header> + OpHardforks + Send + Sync + Unpin + 'static,
    N: NodePrimitives<
            Receipt = R::Receipt,
            SignedTx = R::Transaction,
            BlockHeader = Header,
            BlockBody = alloy_consensus::BlockBody<R::Transaction>,
            Block = alloy_consensus::Block<R::Transaction>,
        >,
    R: OpReceiptBuilder<
            Receipt: DepositReceipt,
            Transaction: SignedTransaction + op_alloy_consensus::OpTransaction,
        > + Clone
        + Send
        + Sync
        + Unpin
        + 'static,
    F: PostExecEvmFactoryHooks<
            Tx: FromRecoveredTx<R::Transaction>
                    + FromTxWithEncoded<R::Transaction>
                    + TransactionEnv
                    + OpTxEnv,
            Precompiles = PrecompilesMap,
            Spec = OpSpecId,
            BlockEnv = BlockEnv,
        > + Debug
        + Clone
        + Send
        + Sync
        + Unpin
        + 'static,
    Self: Send + Sync + Unpin + Clone + 'static,
{
    fn post_exec_executor_for_block<'a, DB: Database>(
        &'a self,
        db: &'a mut revm::database::State<DB>,
        block: &'a SealedBlock<<Self::Primitives as NodePrimitives>::Block>,
        post_exec_mode: PostExecMode,
    ) -> Result<
        impl BlockExecutor<
            Transaction = <Self::Primitives as NodePrimitives>::SignedTx,
            Receipt = <Self::Primitives as NodePrimitives>::Receipt,
        > + PostExecExecutorExt
        + 'a,
        Self::Error,
    > {
        let evm = self.evm_for_block(db, block.header())?;
        let ctx = self.context_for_block_with_post_exec_mode(block, Some(post_exec_mode));

        Ok(OpBlockExecutor::new(
            evm,
            ctx,
            self.executor_factory.spec(),
            self.executor_factory.receipt_builder(),
        ))
    }

    fn post_exec_builder_for_next_block<'a, DB: Database + 'a>(
        &'a self,
        db: &'a mut revm::database::State<DB>,
        parent: &'a SealedHeader<<Self::Primitives as NodePrimitives>::BlockHeader>,
        attributes: Self::NextBlockEnvCtx,
        post_exec_mode: PostExecMode,
    ) -> Result<
        impl BlockBuilder<Primitives = Self::Primitives, Executor: PostExecExecutorExt> + 'a,
        Self::Error,
    > {
        let evm_env = self.next_evm_env(parent, &attributes)?;
        let evm = self.evm_with_env(db, evm_env);
        let ctx =
            self.context_for_next_block_with_post_exec_mode(parent, attributes, post_exec_mode);
        let executor = OpBlockExecutor::new(
            evm,
            ctx.clone(),
            self.executor_factory.spec(),
            self.executor_factory.receipt_builder(),
        );

        Ok(BasicBlockBuilder::<
            'a,
            OpBlockExecutorFactory<R, Arc<ChainSpec>, PostExecEvmFactoryAdapter<F>>,
            _,
            _,
            N,
        > {
            executor,
            transactions: alloc::vec::Vec::new(),
            ctx,
            parent,
            assembler: self.block_assembler(),
        })
    }
}

#[cfg(feature = "std")]
impl<ChainSpec, N, R, EvmF> ConfigureEngineEvm<OpExecData> for CeloEvmConfig<ChainSpec, N, R, EvmF>
where
    ChainSpec: EthChainSpec<Header = Header> + OpHardforks,
    N: NodePrimitives<
            Receipt = R::Receipt,
            SignedTx = R::Transaction,
            BlockHeader = Header,
            BlockBody = alloy_consensus::BlockBody<R::Transaction>,
            Block = alloy_consensus::Block<R::Transaction>,
        >,
    R: OpReceiptBuilder<Receipt: DepositReceipt, Transaction: SignedTransaction>,
    EvmF: EvmFactory<
            Tx: FromRecoveredTx<R::Transaction>
                    + FromTxWithEncoded<R::Transaction>
                    + TransactionEnv
                    + OpTxEnv,
            Spec = OpSpecId,
            BlockEnv = BlockEnv,
            Precompiles = PrecompilesMap,
        > + Debug,
    OpBlockExecutorFactory<R, Arc<ChainSpec>, EvmF>: for<'a> BlockExecutorFactory<
            EvmFactory = EvmF,
            ExecutionCtx<'a> = OpBlockExecutionCtx,
            Transaction = R::Transaction,
            Receipt = R::Receipt,
        >,
    Self: Send + Sync + Unpin + Clone + 'static,
{
    fn evm_env_for_payload(&self, payload: &OpExecData) -> Result<EvmEnvFor<Self>, Self::Error> {
        use alloy_primitives::U256;
        use revm::{
            context::CfgEnv, context_interface::block::BlobExcessGasAndPrice,
            primitives::hardfork::SpecId,
        };

        let timestamp = payload.payload.timestamp();
        let spec =
            reth_optimism_evm::revm_spec_by_timestamp_after_bedrock(self.chain_spec(), timestamp);

        let cfg_env = CfgEnv::new()
            .with_chain_id(self.chain_spec().chain().id())
            .with_spec_and_mainnet_gas_params(spec);

        let blob_excess_gas_and_price = spec
            .into_eth_spec()
            .is_enabled_in(SpecId::CANCUN)
            .then_some(BlobExcessGasAndPrice { excess_blob_gas: 0, blob_gasprice: 1 });

        let block_env = BlockEnv {
            number: U256::from(payload.payload.block_number()),
            beneficiary: payload.payload.as_v1().fee_recipient,
            timestamp: U256::from(timestamp),
            difficulty: if spec.into_eth_spec() >= SpecId::MERGE {
                U256::ZERO
            } else {
                payload.payload.as_v1().prev_randao.into()
            },
            prevrandao: (spec.into_eth_spec() >= SpecId::MERGE)
                .then(|| payload.payload.as_v1().prev_randao),
            gas_limit: payload.payload.as_v1().gas_limit,
            basefee: payload.payload.as_v1().base_fee_per_gas.to(),
            blob_excess_gas_and_price,
            slot_num: 0,
        };

        Ok(alloy_evm::EvmEnv { cfg_env, block_env })
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a OpExecData,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        Ok(OpBlockExecutionCtx {
            parent_hash: payload.parent_hash(),
            parent_beacon_block_root: payload.sidecar.parent_beacon_block_root(),
            extra_data: payload.payload.as_v1().extra_data.clone(),
            post_exec_mode: PostExecMode::default(),
        })
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &OpExecData,
    ) -> Result<impl ExecutableTxIterator<Self>, Self::Error> {
        use alloy_primitives::Bytes;
        use reth_primitives_traits::WithEncoded;
        use reth_storage_errors::any::AnyError;

        let transactions = payload.payload.transactions().clone();
        let convert = |encoded: Bytes| {
            let tx = <TxTy<Self::Primitives> as alloy_eips::Decodable2718>::decode_2718_exact(
                encoded.as_ref(),
            )
            .map_err(AnyError::new)?;
            let signer = tx.try_recover().map_err(AnyError::new)?;
            Ok::<_, AnyError>(WithEncoded::new(encoded, tx.with_signer(signer)))
        };

        Ok((transactions, convert))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use reth_optimism_chainspec::OpChainSpecBuilder;

    /// Helper: build a pre-Jovian chain spec (Granite activated at genesis,
    /// pre-Holocene so standard EIP-1559 base fee computation is used).
    fn pre_jovian_chain_spec() -> Arc<reth_optimism_chainspec::OpChainSpec> {
        Arc::new(
            OpChainSpecBuilder::default()
                .chain(reth_chainspec::Chain::from_id(42220))
                .genesis(Default::default())
                .granite_activated()
                .build(),
        )
    }

    /// Helper: build a post-Jovian chain spec (Jovian activated at genesis).
    fn jovian_chain_spec() -> Arc<reth_optimism_chainspec::OpChainSpec> {
        Arc::new(
            OpChainSpecBuilder::default()
                .chain(reth_chainspec::Chain::from_id(42220))
                .genesis(Default::default())
                .jovian_activated()
                .build(),
        )
    }

    #[test]
    fn base_fee_returned_without_floor_post_jovian() {
        use alloy_primitives::B64;
        use op_alloy_consensus::encode_jovian_extra_data;
        use reth_chainspec::BaseFeeParams;

        let cs = jovian_chain_spec();
        // Jovian requires encoded extra_data: use chain defaults (zero params) with
        // min_base_fee = 0 so the floor doesn't interfere.
        let extra_data = encode_jovian_extra_data(
            B64::ZERO,
            BaseFeeParams::ethereum(),
            0, // min_base_fee
        )
        .unwrap();
        // Parent header with base fee = 7 wei, 50% utilization.
        // EIP-1559 at 50% yields the same base fee (7).
        // Post-Jovian, the 25 Gwei floor is NOT applied.
        let parent = Header {
            base_fee_per_gas: Some(7),
            gas_limit: 30_000_000,
            gas_used: 15_000_000,
            timestamp: 10,
            extra_data,
            ..Default::default()
        };
        let result = celo_next_block_base_fee(&cs, &parent, 12);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), 7, "Post-Jovian should not clamp to the base fee floor");
    }

    #[test]
    fn base_fee_floor_applied_pre_jovian() {
        let cs = pre_jovian_chain_spec();
        // Parent header with base fee = 7 wei, gas limit 30M, gas used 15M (50%).
        // EIP-1559 formula at 50% utilization yields the same base fee.
        // The floor should bump it to 25 Gwei.
        let parent = Header {
            base_fee_per_gas: Some(7),
            gas_limit: 30_000_000,
            gas_used: 15_000_000,
            timestamp: 10,
            ..Default::default()
        };
        let result = celo_next_block_base_fee(&cs, &parent, 12);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), CELO_BASE_FEE_FLOOR);
    }

    #[test]
    fn base_fee_above_floor_unchanged_pre_jovian() {
        let cs = pre_jovian_chain_spec();
        // Parent with base fee well above the floor (50 Gwei), 50% utilization.
        let parent = Header {
            base_fee_per_gas: Some(50_000_000_000),
            gas_limit: 30_000_000,
            gas_used: 15_000_000,
            timestamp: 10,
            ..Default::default()
        };
        let result = celo_next_block_base_fee(&cs, &parent, 12).unwrap();
        // At 50% utilization, EIP-1559 keeps base fee the same.
        assert_eq!(result, 50_000_000_000);
    }

    /// Regression: at the Jovian activation boundary, the floor must be decided by the
    /// *new* block's timestamp, not the parent's. The first Jovian block should NOT
    /// have the 25 Gwei floor applied even though its parent is pre-Jovian.
    #[test]
    fn jovian_activation_boundary_no_floor() {
        use alloy_primitives::B64;
        use op_alloy_consensus::encode_holocene_extra_data;
        use reth_chainspec::{BaseFeeParams, ForkCondition};
        use reth_optimism_forks::OpHardfork;

        // Chain spec where Jovian activates at timestamp 100.
        let cs = Arc::new(
            OpChainSpecBuilder::default()
                .chain(reth_chainspec::Chain::from_id(42220))
                .genesis(Default::default())
                .isthmus_activated()
                .with_fork(OpHardfork::Jovian, ForkCondition::Timestamp(100))
                .build(),
        );

        // Parent at timestamp 99 (pre-Jovian, Holocene-active) with Holocene-encoded
        // extra_data (9 bytes, version 0). Use low base fee (7 wei) that would be
        // clamped by the 25 Gwei floor.
        let extra_data = encode_holocene_extra_data(B64::ZERO, BaseFeeParams::ethereum()).unwrap();
        let parent = Header {
            base_fee_per_gas: Some(7),
            gas_limit: 30_000_000,
            gas_used: 15_000_000,
            timestamp: 99,
            extra_data,
            ..Default::default()
        };

        // New block at timestamp 100 (first Jovian block): floor should NOT apply.
        let result = celo_next_block_base_fee(&cs, &parent, 100);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), 7, "First Jovian block should not apply the 25 Gwei floor");

        // Sanity: a block at timestamp 99 (still pre-Jovian) SHOULD get the floor.
        let result_pre = celo_next_block_base_fee(&cs, &parent, 99);
        assert!(result_pre.is_some());
        assert_eq!(result_pre.unwrap(), CELO_BASE_FEE_FLOOR);
    }

    #[test]
    fn base_fee_floor_clamps_low_computed_fee() {
        let cs = pre_jovian_chain_spec();
        // Parent with base fee = 1 Gwei, empty block (gas_used = 0).
        // EIP-1559 with empty block would lower base fee, but floor should clamp.
        let parent = Header {
            base_fee_per_gas: Some(1_000_000_000),
            gas_limit: 30_000_000,
            gas_used: 0,
            timestamp: 10,
            ..Default::default()
        };
        let result = celo_next_block_base_fee(&cs, &parent, 12).unwrap();
        assert_eq!(result, CELO_BASE_FEE_FLOOR, "Low base fee should be clamped to floor");
    }
}
