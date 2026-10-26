//! Shared ordered CGTv2 execution for celo-reth and Kona.
//! Required upgrade deposits perform migration through normal EVM execution.

use alloy_consensus::{Transaction, TransactionEnvelope, TxReceipt};
use alloy_eips::Encodable2718;
use alloy_evm::{
    Database, FromRecoveredTx, FromTxWithEncoded, RecoveredTx,
    block::{
        BlockExecutionError, BlockExecutionResult, BlockExecutor, BlockValidationError,
        CommitChanges, ExecutableTx, GasOutput, StateDB, TxResult,
    },
};
use alloy_op_evm::{
    OpBlockExecutor, PreRefundGasUsed,
    block::{OpBlockExecutionError, OpTxEnv, OpTxResult, receipt_builder::OpReceiptBuilder},
    post_exec::{PostExecEvm, PostExecExecutorExt, PostExecRefundEvent},
};
use alloy_op_hardforks::OpHardforks;
use alloy_primitives::U256;
use celo_alloy_consensus::cgt_v2_group::{
    CGT_V2_FLAG_SLOT, CGT_V2_L1_BLOCK, CGT_V2_STEPS, CgtV2UpgradeGroup,
};
use op_alloy_consensus::{OpTransaction as OpConsensusTransaction, post_exec::SDMGasEntry};
use revm::{Database as _, DatabaseCommit};

fn invalid_cgt_signal(message: &'static str) -> BlockExecutionError {
    BlockValidationError::msg(message).into()
}

/// OP execution result with the CGTv2 activation status of this candidate.
/// Keeping the marker in the result makes it take effect only when the candidate is committed.
#[derive(Debug)]
pub struct CeloTxResult<H, T> {
    /// The upstream execution result.
    pub op: OpTxResult<H, T>,
    cgt_v2_step: Option<usize>,
}

impl<H, T> core::ops::Deref for CeloTxResult<H, T> {
    type Target = OpTxResult<H, T>;

    fn deref(&self) -> &Self::Target {
        &self.op
    }
}

impl<H: Send + 'static, T: Send + 'static> TxResult for CeloTxResult<H, T> {
    type HaltReason = H;

    fn result(&self) -> &revm::context::result::ResultAndState<H> {
        self.op.result()
    }

    fn into_result(self) -> revm::context::result::ResultAndState<H> {
        self.op.into_result()
    }
}

impl<H, T> PreRefundGasUsed for CeloTxResult<H, T> {
    fn evm_gas_used(&self) -> u64 {
        self.op.evm_gas_used()
    }
}

/// Celo executor with strict ordering, completeness and success checks for CGTv2 deposits.
pub struct CeloBlockExecutor<E, R: OpReceiptBuilder, Spec> {
    /// The wrapped OP Stack block executor.
    inner: OpBlockExecutor<E, R, Spec>,
    /// Incremental builders may not know the signal until it is executed.
    has_committed_non_deposit_tx: bool,
    /// Only committed required deposits advance the group.
    cgt_v2_next_step: usize,
}

impl<E, R: OpReceiptBuilder, Spec> core::fmt::Debug for CeloBlockExecutor<E, R, Spec> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CeloBlockExecutor").finish_non_exhaustive()
    }
}

impl<E, R: OpReceiptBuilder, Spec> CeloBlockExecutor<E, R, Spec> {
    /// Creates a new [`CeloBlockExecutor`] wrapping the given [`OpBlockExecutor`].
    pub const fn new(inner: OpBlockExecutor<E, R, Spec>) -> Self {
        Self { inner, has_committed_non_deposit_tx: false, cgt_v2_next_step: 0 }
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
    type Result = CeloTxResult<E::HaltReason, <R::Transaction as TransactionEnvelope>::TxType>;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.inner.apply_pre_execution_changes()
    }

    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutableTx<Self>,
        f: impl FnOnce(&Self::Result) -> CommitChanges,
    ) -> Result<Option<GasOutput>, BlockExecutionError> {
        let refund_snapshot =
            matches!(self.inner.ctx.post_exec_mode, alloy_op_evm::PostExecMode::Produce)
                .then(|| self.inner.evm.refund_snapshot());
        let output = match self.execute_transaction_without_commit(tx) {
            Ok(output) => output,
            Err(error) => {
                if let Some(snapshot) = refund_snapshot {
                    self.inner.evm.seed_refund_snapshot(snapshot);
                }
                return Err(error);
            }
        };
        if !f(&output).should_commit() {
            if let Some(snapshot) = refund_snapshot {
                self.inner.evm.seed_refund_snapshot(snapshot);
            }
            return Ok(None);
        }
        Ok(Some(self.commit_transaction(output)))
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        let (tx_env, tx) = tx.into_parts();
        let step = tx
            .tx()
            .as_deposit()
            .map(|deposit| CgtV2UpgradeGroup::validate(self.inner.evm.chain_id(), deposit))
            .transpose()
            .map_err(|error| BlockExecutionError::Validation(BlockValidationError::other(error)))?
            .flatten();
        if let Some(step) = step {
            if step != self.cgt_v2_next_step {
                return Err(invalid_cgt_signal(
                    "CGTv2 upgrade deposits are missing, repeated or reordered",
                ));
            }
            if self.has_committed_non_deposit_tx {
                return Err(BlockValidationError::other(
                    OpBlockExecutionError::UnexpectedNonDepositTxInForkActivationBlock,
                )
                .into());
            }
            if step == 0 {
                if self.inner.receipts.is_empty() {
                    return Err(invalid_cgt_signal("CGTv2 activation requires L1-info first"));
                }
                let flag = self
                    .inner
                    .evm
                    .db_mut()
                    .storage(CGT_V2_L1_BLOCK, U256::from_be_bytes(CGT_V2_FLAG_SLOT.0))
                    .map_err(BlockExecutionError::other)?;
                if !flag.is_zero() {
                    return Err(invalid_cgt_signal("CGTv2 activation cannot repeat"));
                }
            }
        } else if self.cgt_v2_next_step > 0 {
            if self.cgt_v2_next_step < CGT_V2_STEPS {
                return Err(invalid_cgt_signal("CGTv2 upgrade group cannot be interrupted"));
            }
            if tx.tx().is_deposit() {
                return Err(invalid_cgt_signal("origin deposits must precede CGTv2 upgrades"));
            }
            return Err(BlockValidationError::other(
                OpBlockExecutionError::UnexpectedNonDepositTxInForkActivationBlock,
            )
            .into());
        }
        let op = self.inner.execute_transaction_without_commit((tx_env, tx))?;
        if step.is_some() && !op.inner.result.result.is_success() {
            return Err(invalid_cgt_signal("required CGTv2 upgrade deposit reverted or halted"));
        }
        Ok(CeloTxResult { op, cgt_v2_step: step })
    }

    fn commit_transaction(&mut self, output: Self::Result) -> GasOutput {
        self.has_committed_non_deposit_tx |= !output.op.is_deposit;
        if let Some(step) = output.cgt_v2_step {
            self.cgt_v2_next_step = step + 1;
            self.inner.ctx.no_user_tx_activation_block = true;
        }
        self.inner.commit_transaction(output.op)
    }

    fn finish(
        mut self,
    ) -> Result<(Self::Evm, BlockExecutionResult<Self::Receipt>), BlockExecutionError> {
        if self.cgt_v2_next_step > 0 {
            if self.cgt_v2_next_step != CGT_V2_STEPS {
                return Err(invalid_cgt_signal("incomplete CGTv2 upgrade group"));
            }
            let flag = self
                .inner
                .evm
                .db_mut()
                .storage(CGT_V2_L1_BLOCK, U256::from_be_bytes(CGT_V2_FLAG_SLOT.0))
                .map_err(BlockExecutionError::other)?;
            if flag != U256::from(1) {
                return Err(invalid_cgt_signal("CGTv2 completion flag is not set"));
            }
        }
        self.inner.finish()
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

// The sequencing path (op-reth's payload builder) obtains its executor via
// `ConfigurePostExecEvm`, which requires the post-exec extension. Delegate it so the
// wrapper is usable there too. Without this, sequencing would silently fall back to a
// raw `OpBlockExecutor` and skip the Upgrade 18 transition.
impl<E, R, Spec> PostExecExecutorExt for CeloBlockExecutor<E, R, Spec>
where
    E: PostExecEvm,
    R: OpReceiptBuilder,
    Spec: OpHardforks + Clone,
{
    type Snapshot = E::Snapshot;

    fn post_exec_entries(&self) -> &[SDMGasEntry] {
        self.inner.post_exec_entries()
    }

    fn take_post_exec_entries(&mut self) -> alloc::vec::Vec<SDMGasEntry> {
        self.inner.take_post_exec_entries()
    }

    fn take_refund_events_by_tx(
        &mut self,
    ) -> alloc::vec::Vec<alloc::vec::Vec<PostExecRefundEvent>> {
        self.inner.take_refund_events_by_tx()
    }

    fn refund_snapshot(&self) -> Self::Snapshot {
        PostExecExecutorExt::refund_snapshot(&self.inner)
    }

    fn seed_refund_snapshot(&mut self, state: Self::Snapshot) {
        PostExecExecutorExt::seed_refund_snapshot(&mut self.inner, state)
    }
}

#[cfg(test)]
mod tests;
