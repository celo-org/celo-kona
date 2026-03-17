//! Celo EVM configuration for reth.
//!
//! Bridges the Celo EVM ([`CeloEvmFactory`]) with reth's node framework,
//! analogous to `reth-optimism-evm` for vanilla OP Stack chains.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

// These deps are only used by the binary target; declare them here to silence
// the `unused_crate_dependencies` lint on the library compilation unit.
#[cfg(feature = "std")]
use clap as _;
#[cfg(feature = "std")]
use reth_cli_util as _;
#[cfg(feature = "std")]
use reth_optimism_cli as _;
#[cfg(feature = "std")]
use reth_provider as _;
#[cfg(feature = "std")]
use reth_transaction_pool as _;
#[cfg(feature = "std")]
use tracing as _;

use alloc::sync::Arc;
use alloy_consensus::{BlockHeader, Header};
use alloy_evm::{EvmFactory, FromRecoveredTx, FromTxWithEncoded, precompiles::PrecompilesMap};
use alloy_op_evm::block::{OpTxEnv, receipt_builder::OpReceiptBuilder};
use core::fmt::Debug;
use op_alloy_consensus::EIP1559ParamError;
use op_revm::OpSpecId;
use reth_chainspec::EthChainSpec;
use reth_evm::{ConfigureEvm, EvmEnv, TransactionEnv, eth::NextEvmEnvAttributes};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_forks::OpHardforks;
use reth_primitives_traits::{NodePrimitives, SealedBlock, SealedHeader, SignedTransaction};
use revm::context::BlockEnv;

pub mod receipt;
pub mod primitives;
pub mod receipts;

#[cfg(feature = "std")]
pub mod node;

#[cfg(feature = "std")]
pub mod pool;

#[cfg(feature = "std")]
pub mod payload;

#[cfg(feature = "std")]
pub mod rpc;

#[cfg(all(test, feature = "std"))]
pub(crate) mod test_utils;

pub use primitives::*;
pub use receipt::CeloReceipt;
pub use receipts::CeloRethReceiptBuilder;

// Re-export block assembler and execution context from op-reth (same for Celo as OP Stack).
pub use reth_optimism_evm::{
    OpBlockAssembler, OpBlockExecutionCtx, OpBlockExecutorFactory, OpNextBlockEnvAttributes,
    l1,
};

// Re-export Celo EVM types.
pub use alloy_celo_evm::{CeloEvm, CeloEvmFactory};

use reth_optimism_primitives::DepositReceipt;

#[cfg(feature = "std")]
use {
    op_alloy_rpc_types_engine::OpExecutionData,
    reth_evm::{ConfigureEngineEvm, EvmEnvFor, ExecutableTxIterator, ExecutionCtxFor},
    reth_primitives_traits::TxTy,
};


/// The Celo EIP-1559 base fee floor in wei (25 Gwei).
///
/// Applied as `max(computed_base_fee, CELO_BASE_FEE_FLOOR)` for blocks before Jovian activation.
/// After Jovian, `min_base_fee` is read from the parent block's `extraData` instead.
pub const CELO_BASE_FEE_FLOOR: u64 = 25_000_000_000;

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
    if chain_spec.is_jovian_active_at_timestamp(parent.timestamp()) {
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
    EvmFactory = CeloEvmFactory,
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
        Self::celo_with_blocklist(chain_spec, alloy_celo_evm::blocklist::FeeCurrencyBlocklist::default())
    }

    /// Creates a new [`CeloEvmConfig`] with the given chain spec and shared fee currency blocklist.
    pub fn celo_with_blocklist(
        chain_spec: Arc<ChainSpec>,
        blocklist: alloy_celo_evm::blocklist::FeeCurrencyBlocklist,
    ) -> Self {
        // Create a shared CIP-64 storage so the EVM and receipt builder can communicate.
        let cip64_storage = alloy_celo_evm::cip64_storage::Cip64Storage::default();
        let receipt_builder = CeloRethReceiptBuilder::new(cip64_storage.clone());
        let evm_factory = CeloEvmFactory::with_cip64_storage(cip64_storage)
            .with_blocklist(blocklist);
        Self {
            block_assembler: OpBlockAssembler::new(chain_spec.clone()),
            executor_factory: OpBlockExecutorFactory::new(
                receipt_builder,
                chain_spec,
                evm_factory,
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
        Ok(EvmEnv::for_op_block(header, self.chain_spec(), self.chain_spec().chain().id()))
    }

    fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnv<OpSpecId>, Self::Error> {
        Ok(EvmEnv::for_op_next_block(
            parent,
            NextEvmEnvAttributes {
                timestamp: attributes.timestamp,
                suggested_fee_recipient: attributes.suggested_fee_recipient,
                prev_randao: attributes.prev_randao,
                gas_limit: attributes.gas_limit,
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
        })
    }
}

#[cfg(feature = "std")]
impl<ChainSpec, N, R, EvmF> ConfigureEngineEvm<OpExecutionData>
    for CeloEvmConfig<ChainSpec, N, R, EvmF>
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
    Self: Send + Sync + Unpin + Clone + 'static,
{
    fn evm_env_for_payload(
        &self,
        payload: &OpExecutionData,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        use alloy_primitives::U256;
        use revm::context::CfgEnv;
        use revm::context_interface::block::BlobExcessGasAndPrice;
        use revm::primitives::hardfork::SpecId;

        let timestamp = payload.payload.timestamp();
        let spec = reth_optimism_evm::revm_spec_by_timestamp_after_bedrock(
            self.chain_spec(),
            timestamp,
        );

        let cfg_env = CfgEnv::new()
            .with_chain_id(self.chain_spec().chain().id())
            .with_spec_and_mainnet_gas_params(spec);

        let blob_excess_gas_and_price = spec
            .into_eth_spec()
            .is_enabled_in(SpecId::CANCUN)
            .then_some(BlobExcessGasAndPrice {
                excess_blob_gas: 0,
                blob_gasprice: 1,
            });

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
        };

        Ok(alloy_evm::EvmEnv { cfg_env, block_env })
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a OpExecutionData,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        Ok(OpBlockExecutionCtx {
            parent_hash: payload.parent_hash(),
            parent_beacon_block_root: payload.sidecar.parent_beacon_block_root(),
            extra_data: payload.payload.as_v1().extra_data.clone(),
        })
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &OpExecutionData,
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
