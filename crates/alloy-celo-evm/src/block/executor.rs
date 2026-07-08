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

use crate::block::upgrade18::{Upgrade18Overrides, cgt_v2_state_changes};
use alloc::{boxed::Box, sync::Arc};
use alloy_consensus::{Transaction, TransactionEnvelope, TxReceipt};
use alloy_eips::Encodable2718;
use alloy_evm::{
    Database, FromRecoveredTx, FromTxWithEncoded,
    block::{
        BlockExecutionError, BlockExecutionResult, BlockExecutor, ExecutableTx, GasOutput,
        OnStateHook, StateChangePreBlockSource, StateChangeSource, StateDB,
    },
};
use alloy_op_evm::{
    OpBlockExecutor,
    block::{OpTxEnv, OpTxResult, receipt_builder::OpReceiptBuilder},
    post_exec::{PostExecEvm, PostExecExecutorExt, WarmingRefundEvent, WarmingState},
};
use alloy_op_hardforks::OpHardforks;
use op_alloy_consensus::{OpTransaction as OpConsensusTransaction, post_exec::SDMGasEntry};
use revm::{DatabaseCommit, context_interface::Block, state::EvmState};

/// [`StateChangeSource`] reported for the Upgrade 18 irregular state transition.
const UPGRADE18_STATE_CHANGE: StateChangeSource =
    StateChangeSource::PreBlock(StateChangePreBlockSource::Other("celo-upgrade18-cgt-v2"));

/// An [`OnStateHook`] handle that can be shared between this wrapper and the inner
/// [`OpBlockExecutor`], so both can report state changes to the same consumer.
struct SharedHook(Arc<spin::Mutex<Box<dyn OnStateHook>>>);

impl OnStateHook for SharedHook {
    fn on_state(&mut self, source: StateChangeSource, state: &EvmState) {
        self.0.lock().on_state(source, state);
    }
}

/// Block executor for Celo: the upstream [`OpBlockExecutor`] plus the Upgrade 18 (CGT v2)
/// irregular state transition at the fork-activation block.
pub struct CeloBlockExecutor<E, R: OpReceiptBuilder, Spec> {
    /// The wrapped OP Stack block executor.
    inner: OpBlockExecutor<E, R, Spec>,
    /// Provisional Upgrade 18 (CGT v2) activation timestamp; `None` = not scheduled.
    upgrade18_time: Option<u64>,
    /// Caller-supplied values for the activation artifact's `param:` placeholders.
    upgrade18_overrides: Upgrade18Overrides,
    /// Our handle on the block's state hook (shared with the inner executor). The
    /// Upgrade 18 transition must report its changes here: reth's live engine derives
    /// the state root from hook-streamed updates, so unreported direct commits would be
    /// missing from the sealed root.
    state_hook: Option<SharedHook>,
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
    pub const fn new(
        inner: OpBlockExecutor<E, R, Spec>,
        upgrade18_time: Option<u64>,
        upgrade18_overrides: Upgrade18Overrides,
    ) -> Self {
        Self { inner, upgrade18_time, upgrade18_overrides, state_hook: None }
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
        // transaction of the block. Report to the state hook first, then commit — the
        // same order `SystemCaller` uses for pre-block system calls.
        let timestamp = self.inner.evm.block().timestamp().saturating_to();
        let chain_id = self.inner.evm.chain_id();
        if let Some(changes) = cgt_v2_state_changes(
            self.upgrade18_time,
            &self.upgrade18_overrides,
            chain_id,
            timestamp,
            self.inner.evm.db_mut(),
        )? {
            if let Some(hook) = self.state_hook.as_mut() {
                hook.on_state(UPGRADE18_STATE_CHANGE, &changes);
            }
            self.inner.evm.db_mut().commit(changes.into_iter().collect());
        }
        Ok(())
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
        match hook {
            Some(hook) => {
                // Share the hook between this wrapper (Upgrade 18 reporting) and the
                // inner executor (system calls + per-transaction reporting).
                let shared = Arc::new(spin::Mutex::new(hook));
                self.state_hook = Some(SharedHook(Arc::clone(&shared)));
                self.inner.set_state_hook(Some(Box::new(SharedHook(shared))));
            }
            None => {
                self.state_hook = None;
                self.inner.set_state_hook(None);
            }
        }
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
    fn post_exec_entries(&self) -> &[SDMGasEntry] {
        self.inner.post_exec_entries()
    }

    fn take_post_exec_entries(&mut self) -> alloc::vec::Vec<SDMGasEntry> {
        self.inner.take_post_exec_entries()
    }

    fn take_warming_events_by_tx(
        &mut self,
    ) -> alloc::vec::Vec<alloc::vec::Vec<WarmingRefundEvent>> {
        self.inner.take_warming_events_by_tx()
    }

    fn warming_state(&self) -> WarmingState {
        PostExecExecutorExt::warming_state(&self.inner)
    }

    fn seed_warming_state(&mut self, state: WarmingState) {
        PostExecExecutorExt::seed_warming_state(&mut self.inner, state)
    }
}
