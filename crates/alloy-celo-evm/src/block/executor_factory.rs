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

use crate::{
    CeloEvmFactory,
    block::{executor::CeloBlockExecutor, upgrade18::Upgrade18Overrides},
    cip64_storage::Cip64Storage,
};
use alloy_consensus::{Transaction, TransactionEnvelope, TxReceipt};
use alloy_eips::Encodable2718;
use alloy_evm::{
    EvmFactory, FromRecoveredTx, FromTxWithEncoded,
    block::{BlockExecutorFactory, StateDB},
};
use alloy_op_evm::{
    OpBlockExecutionCtx, OpBlockExecutor,
    block::{OpTxResult, receipt_builder::OpReceiptBuilder},
};
use alloy_op_hardforks::OpHardforks;
use celo_revm::{CeloContext, CeloTransaction};
use op_alloy_consensus::OpTransaction as OpConsensusTransaction;
use op_revm::OpHaltReason;
use revm::{Inspector, context::TxEnv};

/// Celo block executor factory.
///
/// Behaves like [`alloy_op_evm::OpBlockExecutorFactory`] but rebuilds `R` per call to
/// [`create_executor`](BlockExecutorFactory::create_executor) so the receipt builder is always
/// bound to the EVM's own [`Cip64Storage`]. The `R` type parameter is the runtime receipt
/// builder used by [`OpBlockExecutor`]; the factory itself never holds an instance of `R`.
#[derive(Debug, Default)]
pub struct CeloBlockExecutorFactory<R, Spec> {
    spec: Spec,
    evm_factory: CeloEvmFactory,
    /// Provisional Upgrade 18 (CGT v2) activation timestamp; `None` = not scheduled.
    /// Consumed by [`CeloBlockExecutor`] to gate the CGT v2 irregular state transition.
    upgrade18_time: Option<u64>,
    /// Caller-supplied values for the Upgrade 18 activation artifact's `param:`
    /// placeholders (override > artifact per-network constant).
    upgrade18_overrides: Upgrade18Overrides,
    _phantom: core::marker::PhantomData<fn() -> R>,
}

impl<R, Spec: Clone> Clone for CeloBlockExecutorFactory<R, Spec> {
    fn clone(&self) -> Self {
        Self {
            spec: self.spec.clone(),
            evm_factory: self.evm_factory.clone(),
            upgrade18_time: self.upgrade18_time,
            upgrade18_overrides: self.upgrade18_overrides.clone(),
            _phantom: core::marker::PhantomData,
        }
    }
}

impl<R, Spec> CeloBlockExecutorFactory<R, Spec> {
    /// Creates a new factory with the given chain spec and EVM factory. Upgrade 18 starts
    /// unscheduled; see [`with_upgrade18_time`](Self::with_upgrade18_time).
    pub const fn new(spec: Spec, evm_factory: CeloEvmFactory) -> Self {
        Self {
            spec,
            evm_factory,
            upgrade18_time: None,
            upgrade18_overrides: Upgrade18Overrides {
                liquidity_controller_owner: None,
                celo_token_l1: None,
                celo_gas_bridge_l1: None,
                native_asset_liquidity_amount: None,
            },
            _phantom: core::marker::PhantomData,
        }
    }

    /// Sets the provisional Upgrade 18 (CGT v2) activation timestamp.
    pub const fn with_upgrade18_time(mut self, upgrade18_time: Option<u64>) -> Self {
        self.upgrade18_time = upgrade18_time;
        self
    }

    /// Sets override values for the Upgrade 18 activation artifact's `param:`
    /// placeholders (dev chains and tests; production values ship in the artifact).
    pub const fn with_upgrade18_overrides(mut self, overrides: Upgrade18Overrides) -> Self {
        self.upgrade18_overrides = overrides;
        self
    }

    /// Exposes the chain specification.
    pub const fn spec(&self) -> &Spec {
        &self.spec
    }

    /// The configured Upgrade 18 (CGT v2) activation timestamp.
    pub const fn upgrade18_time(&self) -> Option<u64> {
        self.upgrade18_time
    }

    /// The configured Upgrade 18 artifact param overrides.
    pub const fn upgrade18_overrides(&self) -> &Upgrade18Overrides {
        &self.upgrade18_overrides
    }

    /// Exposes the EVM factory.
    pub const fn evm_factory(&self) -> &CeloEvmFactory {
        &self.evm_factory
    }
}

impl<R, Spec> BlockExecutorFactory for CeloBlockExecutorFactory<R, Spec>
where
    R: OpReceiptBuilder<
            Transaction: Transaction + Encodable2718 + OpConsensusTransaction,
            Receipt: TxReceipt,
        > + From<Cip64Storage>
        + Send
        + Sync
        + 'static,
    Spec: OpHardforks + Clone + Send + Sync + 'static,
    CeloTransaction<TxEnv>: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>,
{
    type EvmFactory = CeloEvmFactory;
    type ExecutionCtx<'a> = OpBlockExecutionCtx;
    type Transaction = R::Transaction;
    type Receipt = R::Receipt;
    type TxExecutionResult =
        OpTxResult<OpHaltReason, <R::Transaction as TransactionEnvelope>::TxType>;
    type Executor<'a, DB: StateDB, I: Inspector<CeloContext<DB>>> =
        CeloBlockExecutor<<CeloEvmFactory as EvmFactory>::Evm<DB, I>, R, &'a Spec>;

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
        let builder = R::from(evm.cip64_storage().clone());
        CeloBlockExecutor::new(
            OpBlockExecutor::new(evm, ctx, &self.spec, builder),
            self.upgrade18_time,
            self.upgrade18_overrides.clone(),
        )
    }
}
