//! Celo block executor.
//!
//! [`CeloBlockExecutor`] wraps the upstream [`OpBlockExecutor`] and extends its
//! pre-execution phase with the Celo-specific Upgrade 18 (CGT v2) irregular state
//! transition (see [`super::upgrade18`]). Everything else — transaction execution,
//! receipts, post-execution — delegates to the inner OP executor unchanged.
//!
//! Both celo-reth (block import / building) and celo-kona (stateless proof execution)
//! obtain their executor through [`CeloBlockExecutorFactory`], so this single wrapper is
//! the one place fork-boundary state injection happens for node and proof client alike.
//!
//! [`CeloBlockExecutorFactory`]: super::CeloBlockExecutorFactory

use crate::block::upgrade18::ensure_cgt_v2_predeploys;
use alloy_consensus::{Transaction, TransactionEnvelope, TxReceipt};
use alloy_eips::Encodable2718;
use alloy_evm::{
    Database, FromRecoveredTx, FromTxWithEncoded,
    block::{
        BlockExecutionError, BlockExecutionResult, BlockExecutor, ExecutableTx, GasOutput,
        OnStateHook, StateDB,
    },
};
use alloy_op_evm::{
    OpBlockExecutor,
    block::{OpTxEnv, OpTxResult, receipt_builder::OpReceiptBuilder},
    post_exec::PostExecEvm,
};
use alloy_op_hardforks::OpHardforks;
use op_alloy_consensus::OpTransaction as OpConsensusTransaction;
use revm::{DatabaseCommit, context_interface::Block};

/// Block executor for Celo: the upstream [`OpBlockExecutor`] plus the Upgrade 18 (CGT v2)
/// irregular state transition at the fork-activation block.
pub struct CeloBlockExecutor<E, R: OpReceiptBuilder, Spec> {
    /// The wrapped OP Stack block executor.
    inner: OpBlockExecutor<E, R, Spec>,
    /// Provisional Upgrade 18 (CGT v2) activation timestamp; `None` = not scheduled.
    upgrade18_time: Option<u64>,
}

impl<E, R: OpReceiptBuilder, Spec> core::fmt::Debug for CeloBlockExecutor<E, R, Spec> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CeloBlockExecutor")
            .field("upgrade18_time", &self.upgrade18_time)
            .finish_non_exhaustive()
    }
}

impl<E, R: OpReceiptBuilder, Spec> CeloBlockExecutor<E, R, Spec> {
    /// Creates a new [`CeloBlockExecutor`] wrapping the given [`OpBlockExecutor`].
    pub const fn new(inner: OpBlockExecutor<E, R, Spec>, upgrade18_time: Option<u64>) -> Self {
        Self { inner, upgrade18_time }
    }

    /// The wrapped [`OpBlockExecutor`].
    pub const fn inner(&self) -> &OpBlockExecutor<E, R, Spec> {
        &self.inner
    }
}

impl<E, R, Spec> BlockExecutor for CeloBlockExecutor<E, R, Spec>
where
    E: PostExecEvm<
            DB: Database + DatabaseCommit + StateDB,
            Tx: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction> + OpTxEnv,
            HaltReason: Send + 'static,
        >,
    R: OpReceiptBuilder<
            Transaction: Transaction + Encodable2718 + OpConsensusTransaction,
            Receipt: TxReceipt,
        >,
    Spec: OpHardforks,
{
    type Transaction = R::Transaction;
    type Receipt = R::Receipt;
    type Evm = E;
    type Result = OpTxResult<E::HaltReason, <R::Transaction as TransactionEnvelope>::TxType>;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.inner.apply_pre_execution_changes()?;

        // Upgrade 18 (CGT v2) irregular state transition. Runs after the upstream
        // pre-execution changes (system calls, Canyon create2deployer) and before any
        // transaction of the block, exactly like the Canyon precedent.
        let timestamp = self.inner.evm.block().timestamp().saturating_to();
        ensure_cgt_v2_predeploys(self.upgrade18_time, timestamp, self.inner.evm.db_mut())
            .map_err(BlockExecutionError::other)
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        self.inner.execute_transaction_without_commit(tx)
    }

    fn commit_transaction(&mut self, output: Self::Result) -> GasOutput {
        self.inner.commit_transaction(output)
    }

    fn finish(
        self,
    ) -> Result<(Self::Evm, BlockExecutionResult<Self::Receipt>), BlockExecutionError> {
        self.inner.finish()
    }

    fn set_state_hook(&mut self, hook: Option<alloc::boxed::Box<dyn OnStateHook>>) {
        self.inner.set_state_hook(hook)
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        self.inner.evm_mut()
    }

    fn evm(&self) -> &Self::Evm {
        self.inner.evm()
    }

    fn receipts(&self) -> &[Self::Receipt] {
        self.inner.receipts()
    }
}
