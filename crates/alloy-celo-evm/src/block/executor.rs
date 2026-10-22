//! Shared ordered CGTv2 execution for celo-reth and Kona.
//! The protocol activation deposit stages migration at its position in the transaction list.

use crate::block::{
    migration::CgtV2Evm,
    upgrade18::{CELO_GAS_BRIDGE_L2, prepare_cgt_v2_migration},
};
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
use alloy_primitives::{B256, U256};
use celo_alloy_consensus::{
    cgt_v2::{CGT_V2_L1_BLOCK, CgtV2Activation},
    cgt_v2_params::cgt_v2_params,
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
    activates_cgt_v2: bool,
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

/// Block executor for Celo: the upstream [`OpBlockExecutor`] plus the Upgrade 18 (CGT v2)
/// irregular state transition at the activation deposit.
pub struct CeloBlockExecutor<E, R: OpReceiptBuilder, Spec> {
    /// The wrapped OP Stack block executor.
    inner: OpBlockExecutor<E, R, Spec>,
    /// Incremental builders may not know the signal until it is executed.
    has_committed_user_tx: bool,
}

impl<E, R: OpReceiptBuilder, Spec> core::fmt::Debug for CeloBlockExecutor<E, R, Spec> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CeloBlockExecutor").finish_non_exhaustive()
    }
}

impl<E, R: OpReceiptBuilder, Spec> CeloBlockExecutor<E, R, Spec> {
    /// Creates a new [`CeloBlockExecutor`] wrapping the given [`OpBlockExecutor`].
    pub const fn new(inner: OpBlockExecutor<E, R, Spec>) -> Self {
        Self { inner, has_committed_user_tx: false }
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
        > + CgtV2Evm,
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
        let activation = tx
            .tx()
            .as_deposit()
            .map(|deposit| CgtV2Activation::decode_deposit(deposit))
            .transpose()
            .map_err(|error| BlockExecutionError::Validation(BlockValidationError::other(error)))?
            .flatten();
        let Some(activation) = activation else {
            return self
                .inner
                .execute_transaction_without_commit((tx_env, tx))
                .map(|op| CeloTxResult { op, activates_cgt_v2: false });
        };
        if self.has_committed_user_tx {
            return Err(BlockValidationError::other(
                OpBlockExecutionError::UnexpectedNonDepositTxInForkActivationBlock,
            )
            .into());
        }
        if self.inner.receipts.is_empty() {
            return Err(invalid_cgt_signal(
                "CGTv2 activation requires the block's L1-info transaction first",
            ));
        }
        if self
            .inner
            .evm
            .db_mut()
            .basic(CELO_GAS_BRIDGE_L2)
            .map_err(BlockExecutionError::other)?
            .is_some_and(|a| !a.is_empty_code_hash())
        {
            return Err(invalid_cgt_signal("CGTv2 activation cannot repeat"));
        }
        // L1Block.hash occupies slot 2 in the pinned legacy and CGT layouts. It has
        // already been updated by this block's committed L1-info transaction.
        let origin = self
            .inner
            .evm
            .db_mut()
            .storage(CGT_V2_L1_BLOCK, U256::from(2))
            .map_err(BlockExecutionError::other)?;
        if B256::from(origin.to_be_bytes::<32>()) != activation.l1_origin_hash {
            return Err(invalid_cgt_signal("CGTv2 signal does not match the block's L1 origin"));
        }
        let params =
            cgt_v2_params(self.inner.evm.chain_id()).map_err(BlockExecutionError::other)?;
        // Journal staging avoids committing a transition or notifying state hooks for a
        // candidate that can still fail or be declined by the payload builder.
        let journal = self.inner.evm.cgt_v2_journal().inner.clone();
        let outcome = (|| {
            let vault = self.inner.evm.read_cgt_v2_vault_config()?;
            let changes =
                prepare_cgt_v2_migration(self.inner.evm.db_mut(), &params, &activation, vault)?;
            for (address, mut account) in changes.clone() {
                account.mark_cold();
                for slot in account.storage.values_mut() {
                    // EVM gas uses the state immediately after installation. Restore the
                    // actual pre-migration originals in the final diff before commit.
                    slot.original_value = slot.present_value;
                    slot.mark_cold();
                }
                self.inner.evm.cgt_v2_journal().state.insert(address, account);
            }
            let mut output = self.inner.execute_transaction_without_commit((tx_env, tx))?;
            if !output.inner.result.result.is_success() {
                return Err(invalid_cgt_signal("CGTv2 activation setter reverted or halted"));
            }
            for (address, change) in changes {
                let account =
                    output.inner.result.state.entry(address).or_insert_with(|| change.clone());
                for (key, slot) in change.storage {
                    account
                        .storage
                        .entry(key)
                        .and_modify(|final_slot| final_slot.original_value = slot.original_value)
                        .or_insert(slot);
                }
            }
            Ok(output)
        })();
        self.inner.evm.cgt_v2_journal().inner = journal;
        outcome.map(|op| CeloTxResult { op, activates_cgt_v2: true })
    }

    fn commit_transaction(&mut self, output: Self::Result) -> GasOutput {
        self.has_committed_user_tx |= !output.op.is_deposit && !output.op.is_post_exec;
        self.inner.ctx.no_user_tx_activation_block |= output.activates_cgt_v2;
        let gas = self.inner.commit_transaction(output.op);
        // An activation candidate restores the pre-execution journal until accepted.
        // Its cached legacy accounts must not survive the accepted state transition.
        self.inner.evm.cgt_v2_journal().inner.finalize();
        gas
    }

    fn finish(
        self,
    ) -> Result<(Self::Evm, BlockExecutionResult<Self::Receipt>), BlockExecutionError> {
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
// `ConfigurePostExecEvm`, which requires the post-exec extension — delegate it so the
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
