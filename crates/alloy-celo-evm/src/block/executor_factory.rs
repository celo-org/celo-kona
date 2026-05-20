//! Celo block executor factory.
//!
//! This factory is the per-block bridge between a [`CeloEvm`](crate::CeloEvm)'s own
//! [`Cip64Storage`] and the receipt builder that will consume it. Each
//! [`CeloEvm`](crate::CeloEvm) produced by a [`CeloEvmFactory`] owns a fresh [`Cip64Storage`],
//! and on every call to [`create_executor`](BlockExecutorFactory::create_executor) we
//! construct a *new* receipt builder bound to that storage. The factory itself holds no
//! long-lived storage handle, so two consumers running through the same [`CeloEvmFactory`]
//! (e.g. the main-chain block executor and a re-executing ExEx) cannot interfere with each
//! other's pending CIP-64 receipt data.

use crate::{CeloEvmFactory, cip64_storage::Cip64Storage};
use alloy_consensus::{Transaction, TransactionEnvelope, TxReceipt};
use alloy_eips::Encodable2718;
use alloy_evm::{
    EvmFactory, FromRecoveredTx, FromTxWithEncoded,
    block::{BlockExecutorFactory, StateDB},
};
use alloy_op_evm::{
    OpBlockExecutionCtx, OpBlockExecutor,
    block::{OpTxResult, receipt_builder::OpReceiptBuilder},
    post_exec::PostExecEvmFactoryAdapter,
};
use alloy_op_hardforks::OpHardforks;
use celo_revm::{CeloContext, CeloTransaction};
use op_alloy_consensus::OpTransaction as OpConsensusTransaction;
use op_revm::OpHaltReason;
use revm::{Inspector, context::TxEnv};

/// EVM factory used by [`CeloBlockExecutorFactory`]. Wraps [`CeloEvmFactory`] with
/// [`PostExecEvmFactoryAdapter`] so the resulting EVM satisfies the `PostExecEvm` bound
/// required by [`OpBlockExecutor`]. Post-exec is unscheduled on Celo; the adapter's hooks
/// are no-ops (see `PostExecEvmFactoryHooks for CeloEvmFactory` in `alloy-celo-evm`).
type CeloPostExecFactory = PostExecEvmFactoryAdapter<CeloEvmFactory>;

/// Celo block executor factory.
///
/// Behaves like [`alloy_op_evm::OpBlockExecutorFactory`] but rebuilds `R` per call to
/// [`create_executor`](BlockExecutorFactory::create_executor) so the receipt builder is always
/// bound to the EVM's own [`Cip64Storage`]. The `R` type parameter is the runtime receipt
/// builder used by [`OpBlockExecutor`]; the factory itself never holds an instance of `R`.
#[derive(Debug)]
pub struct CeloBlockExecutorFactory<R, Spec> {
    spec: Spec,
    evm_factory: CeloPostExecFactory,
    _phantom: core::marker::PhantomData<fn() -> R>,
}

impl<R, Spec: Clone> Clone for CeloBlockExecutorFactory<R, Spec> {
    fn clone(&self) -> Self {
        Self {
            spec: self.spec.clone(),
            evm_factory: self.evm_factory.clone(),
            _phantom: core::marker::PhantomData,
        }
    }
}

impl<R, Spec> CeloBlockExecutorFactory<R, Spec> {
    /// Creates a new factory with the given chain spec and EVM factory.
    pub fn new(spec: Spec, evm_factory: CeloEvmFactory) -> Self {
        Self {
            spec,
            evm_factory: PostExecEvmFactoryAdapter::new(evm_factory),
            _phantom: core::marker::PhantomData,
        }
    }

    /// Exposes the chain specification.
    pub const fn spec(&self) -> &Spec {
        &self.spec
    }

    /// Exposes the EVM factory (wrapped in the post-exec adapter).
    pub const fn evm_factory(&self) -> &CeloPostExecFactory {
        &self.evm_factory
    }
}

impl<R, Spec> BlockExecutorFactory for CeloBlockExecutorFactory<R, Spec>
where
    R: OpReceiptBuilder<
            Transaction: Transaction + Encodable2718 + OpConsensusTransaction,
            Receipt: TxReceipt,
        >
        + From<Cip64Storage>
        + Send
        + Sync
        + 'static,
    Spec: OpHardforks + Clone + Send + Sync + 'static,
    CeloTransaction<TxEnv>: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>,
{
    type EvmFactory = CeloPostExecFactory;
    type ExecutionCtx<'a> = OpBlockExecutionCtx;
    type Transaction = R::Transaction;
    type Receipt = R::Receipt;
    type TxExecutionResult =
        OpTxResult<OpHaltReason, <R::Transaction as TransactionEnvelope>::TxType>;
    type Executor<'a, DB: StateDB, I: Inspector<CeloContext<DB>>> =
        OpBlockExecutor<<CeloPostExecFactory as EvmFactory>::Evm<DB, I>, R, &'a Spec>;

    fn evm_factory(&self) -> &Self::EvmFactory {
        &self.evm_factory
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: <Self::EvmFactory as EvmFactory>::Evm<DB, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> Self::Executor<'a, DB, I>
    where
        DB: StateDB,
        I: Inspector<<Self::EvmFactory as EvmFactory>::Context<DB>>,
    {
        // Bind the receipt builder to the EVM's own CIP-64 storage. The factory holds no
        // long-lived receipt builder or storage handle — both are scoped to this executor.
        // The post-exec adapter is transparent to `cip64_storage` (accessor passes through
        // to the inner `CeloEvm`).
        let builder = R::from(evm.cip64_storage().clone());
        OpBlockExecutor::new(evm, ctx, &self.spec, builder)
    }
}
