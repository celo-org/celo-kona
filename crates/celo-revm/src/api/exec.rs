//! Execution entry points for [`CeloEvm`].
//!
//! # Every *journal-owning* entry point drains on rejection
//!
//! A rejected CIP-64 transaction can reach the caller with state celo-revm cannot unwind. The
//! `creditGasFees` hook runs through the *committing* `core_contracts::call`, whose `commit_tx`
//! folds the fully executed main transaction — nonce bump, native deduction, the fee debit, every
//! storage write — into `journal.state` and empties the shared revert log *before* the failure is
//! classified. There is no revert log left to replay, so `discard_tx` would be a no-op.
//!
//! What keeps that state out of whatever runs next is therefore the journal drain, and it has to
//! happen on the error path. Revm mostly does this: `ExecuteEvm::transact` (revm-handler 18.1.0,
//! `src/api.rs`) calls `finalize()` unconditionally before propagating, and `transact_many` drains
//! via `inspect_err`. The defaulted methods that return on the `?` first are the exception, and
//! every one of them is overridden below, so a caller that uses an entry point which owns the
//! journal lifecycle does not have to know which of them happens to be safe:
//!
//! - [`ExecuteEvm::replay`] — ours, and it had the same gap.
//! - [`InspectEvm::inspect_tx`] / [`InspectEvm::inspect`] (revm-inspector 19.0.0,
//!   `src/inspect.rs`).
//! - The committing entry points [`ExecuteCommitEvm::transact_commit`],
//!   [`InspectCommitEvm::inspect_tx_commit`] and [`InspectCommitEvm::inspect_commit`], whose
//!   defaults propagate before `commit_inner()` can reach its `finalize()`.
//!
//! The committing overrides leave the success arm to revm's own `commit_inner()` and add the
//! drain only on the error arm, so the accepted-transaction path stays byte-identical to
//! upstream. `transact_many_commit` and `replay_commit` need no override — they inherit the drain
//! from `transact_many` and from our `replay`.
//!
//! One divergence is deliberate and worth knowing about: `finalize()` drains everything
//! accumulated since the last drain, not just the transaction that failed. A caller that
//! interleaves `transact_one` with `transact_commit` therefore loses the earlier transactions'
//! state when a later one is rejected, where upstream would leave it in the journal. The journal
//! cannot separate the two, so the choice is drop-all or keep-all, and keep-all is the bug this
//! module exists to prevent. Upstream's own `ExecuteEvm::transact` drops-all for the same reason.
//!
//! `alloy-celo-evm`'s `CeloEvm::transact_raw` relies on this for its inspecting arm.
//!
//! # What the guarantee does *not* cover
//!
//! [`ExecuteEvm::transact_one`] and [`InspectEvm::inspect_one_tx`] are deliberately excluded. They
//! are the non-finalizing primitives — they never drain, on either path, because their whole
//! purpose is to accumulate state across a batch that the caller finalizes once at the end. Adding
//! an error-path drain there would discard the *earlier, accepted* transactions of that batch,
//! which is how `transact_many` and every block builder use them. That trades a rare orphan for
//! routine destruction of valid state, so this crate does not do it.
//!
//! The exclusion has a sharp edge, and callers need to know about it: revm documents on
//! `transact_one` (revm-handler 18.1.0, `src/api.rs`) that "if the transaction fails, the journal
//! will revert all changes of given transaction". **celo-revm does not honour that for CIP-64.**
//! A credit-hook failure has already been committed into `journal.state` by the time it is
//! classified, so there is nothing left to revert. A caller who reads revm's contract and keeps
//! going after an `Err` — safe on plain revm — silently folds the rejected transaction into
//! whatever it finalizes next.
//!
//! So a direct user of these two methods owns the drain: on `Err`, call [`ExecuteEvm::finalize`]
//! and drop what it returns before reusing the EVM, or drop the EVM. Everything above them in this
//! module already does exactly that. The permanent fix is to stop producing unwindable-but-
//! uncommitted state in the first place, which is a change to the credit hook, not to these entry
//! points.

use crate::constants::CELO_SYSTEM_ADDRESS;
use crate::{CeloContext, CeloEvm, handler::CeloHandler};
use alloy_primitives::{Address, Bytes};
use op_revm::{OpHaltReason, OpTransactionError};
use revm::SystemCallEvm;
use revm::{
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
    context::{ContextSetters, result::ExecResultAndState},
    context_interface::{
        ContextTr, Database,
        result::{EVMError, ExecutionResult},
    },
    handler::{EthFrame, EvmTr, Handler, PrecompileProvider, SYSTEM_ADDRESS, SystemCallTx},
    inspector::{InspectCommitEvm, InspectEvm, Inspector, InspectorHandler},
    interpreter::{InterpreterResult, interpreter::EthInterpreter},
    state::EvmState,
};

/// Type alias for the error type of the CeloEvm.
type CeloError<CTX> = EVMError<<<CTX as ContextTr>::Db as Database>::Error, OpTransactionError>;

impl<DB, INSP, P> ExecuteEvm for CeloEvm<DB, INSP, P>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
{
    type Tx = <CeloContext<DB> as ContextTr>::Tx;
    type Block = <CeloContext<DB> as ContextTr>::Block;
    type State = EvmState;
    type Error = CeloError<CeloContext<DB>>;
    type ExecutionResult = ExecutionResult<OpHaltReason>;

    fn set_block(&mut self, block: Self::Block) {
        self.inner.ctx().set_block(block);
    }

    /// # Warning: a failure here does *not* revert the transaction's state
    ///
    /// Revm documents that a failing `transact_one` leaves the journal reverted. celo-revm cannot
    /// honour that for CIP-64: a `creditGasFees` failure is classified *after* the hook's
    /// `commit_tx` has already folded the whole transaction into `journal.state` and emptied the
    /// revert log.
    ///
    /// This method does not drain, by design; see
    /// [the module note](self#what-the-guarantee-does-not-cover). If you call it directly and want
    /// to keep using the EVM after an `Err`, call [`ExecuteEvm::finalize`] yourself and drop the
    /// result. Prefer [`ExecuteEvm::transact`], which does that for you.
    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx().set_tx(tx);
        let mut h =
            CeloHandler::<Self, CeloError<CeloContext<DB>>, EthFrame<EthInterpreter>>::new();
        h.run(self)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.ctx().journal_mut().finalize()
    }

    /// Drains the journal on the error path as well as the success path.
    ///
    /// See [the module note](self) — a rejected CIP-64 transaction can reach this point with
    /// state that celo-revm cannot unwind, so the drain is what keeps it from reaching whatever
    /// runs next on this EVM.
    fn replay(
        &mut self,
    ) -> Result<ExecResultAndState<Self::ExecutionResult, Self::State>, Self::Error> {
        let mut h =
            CeloHandler::<Self, CeloError<CeloContext<DB>>, EthFrame<EthInterpreter>>::new();
        let result = h.run(self);
        let state = self.finalize();
        result.map(|result| ExecResultAndState::new(result, state))
    }
}

impl<DB, INSP, P> ExecuteCommitEvm for CeloEvm<DB, INSP, P>
where
    DB: Database + DatabaseCommit,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
{
    fn commit(&mut self, state: Self::State) {
        self.inner.ctx().db_mut().commit(state);
    }

    /// Drains the journal on the error path, where revm's default does not.
    ///
    /// See [the module note](self). Revm's default propagates the `transact_one` error before
    /// `commit_inner()` runs, leaving the rejected transaction's state in the journal for the
    /// next transaction on this EVM to finalize as its own.
    ///
    /// The success arm calls [`ExecuteCommitEvm::commit_inner`] rather than reproducing it, so
    /// only the error arm diverges from upstream and a future revm that adds a step to
    /// `commit_inner` is inherited rather than silently skipped.
    fn transact_commit(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        match self.transact_one(tx) {
            Ok(output) => {
                self.commit_inner();
                Ok(output)
            }
            Err(err) => {
                // The drain takes everything accumulated since the last one, not just this
                // transaction — the journal cannot separate the two. Dropping it is the
                // deliberate choice; see the module note.
                let _ = self.finalize();
                Err(err)
            }
        }
    }
}

impl<DB, INSP, P> InspectEvm for CeloEvm<DB, INSP, P>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.inner.0.inspector = inspector;
    }

    /// # Warning: a failure here does *not* revert the transaction's state
    ///
    /// The inspecting twin of [`ExecuteEvm::transact_one`], and it carries the same caveat: no
    /// drain on either path, and a CIP-64 credit failure leaves state the journal can no longer
    /// unwind. See there and [the module note](self#what-the-guarantee-does-not-cover).
    /// Prefer [`InspectEvm::inspect_tx`], which drains for you.
    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx().set_tx(tx);
        let mut h =
            CeloHandler::<Self, CeloError<CeloContext<DB>>, EthFrame<EthInterpreter>>::new();
        h.inspect_run(self)
    }

    /// Drains the journal on the error path as well as the success path; see
    /// [the module note](self) and [`ExecuteEvm::replay`].
    fn inspect_tx(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ExecResultAndState<Self::ExecutionResult, Self::State>, Self::Error> {
        let output = self.inspect_one_tx(tx);
        let state = self.finalize();
        output.map(|output| ExecResultAndState::new(output, state))
    }

    /// Routes through the overridden [`InspectEvm::inspect_tx`] so this entry point drains too.
    fn inspect(
        &mut self,
        tx: Self::Tx,
        inspector: Self::Inspector,
    ) -> Result<ExecResultAndState<Self::ExecutionResult, Self::State>, Self::Error> {
        self.set_inspector(inspector);
        self.inspect_tx(tx)
    }
}

impl<DB, INSP, P> InspectCommitEvm for CeloEvm<DB, INSP, P>
where
    DB: Database + DatabaseCommit,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
{
    /// Committing counterpart of the overridden [`InspectEvm::inspect_tx`]: drains the journal on
    /// the error path, where revm's default does not. Shaped like
    /// [`ExecuteCommitEvm::transact_commit`] — see there and [the module note](self).
    fn inspect_tx_commit(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        match self.inspect_one_tx(tx) {
            Ok(output) => {
                self.commit_inner();
                Ok(output)
            }
            Err(err) => {
                let _ = self.finalize();
                Err(err)
            }
        }
    }

    /// Routes through the overridden [`InspectCommitEvm::inspect_tx_commit`] so this entry point
    /// drains too.
    fn inspect_commit(
        &mut self,
        tx: Self::Tx,
        inspector: Self::Inspector,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.set_inspector(inspector);
        self.inspect_tx_commit(tx)
    }
}

impl<DB, INSP, P> SystemCallEvm for CeloEvm<DB, INSP, P>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
{
    fn system_call_one_with_caller(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.run_system_tx(
            <CeloContext<DB> as ContextTr>::Tx::new_system_tx_with_caller(
                caller,
                system_contract_address,
                data,
            ),
            true,
        )
    }
}

impl<DB, INSP, P> CeloEvm<DB, INSP, P>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
{
    /// Set `tx` as the current system transaction and run it through a fresh
    /// [`CeloHandler`], either committing the journal (`commit == true`, via
    /// [`Handler::run_system_call`]) or leaving the revert log intact for an enclosing
    /// `checkpoint` / `checkpoint_revert` (`commit == false`, via
    /// [`CeloHandler::run_system_call_no_commit`]). Shared by the committing and
    /// non-committing system-call entry points below, which differ only in the tx they build
    /// and this flag.
    fn run_system_tx(
        &mut self,
        tx: <CeloContext<DB> as ContextTr>::Tx,
        commit: bool,
    ) -> Result<ExecutionResult<OpHaltReason>, CeloError<CeloContext<DB>>> {
        self.inner.ctx().set_tx(tx);
        let mut h =
            CeloHandler::<Self, CeloError<CeloContext<DB>>, EthFrame<EthInterpreter>>::new();
        if commit {
            h.run_system_call(self)
        } else {
            h.run_system_call_no_commit(self)
        }
    }

    /// Execute a system call with a custom gas limit
    pub fn transact_system_call_with_gas_limit(
        &mut self,
        system_contract_address: Address,
        data: Bytes,
        gas_limit: u64,
    ) -> Result<ExecutionResult<OpHaltReason>, CeloError<CeloContext<DB>>> {
        self.run_system_tx(
            <CeloContext<DB> as ContextTr>::Tx::new_system_tx_with_gas_limit(
                CELO_SYSTEM_ADDRESS,
                system_contract_address,
                data,
                gas_limit,
            ),
            true,
        )
    }

    /// Non-committing counterpart of [`SystemCallEvm::system_call_one`].
    ///
    /// Runs the system call through `CeloHandler::run_system_call_no_commit` so the
    /// journal's revert log survives, letting the caller undo every state change with
    /// a surrounding `checkpoint` / `checkpoint_revert`. Behaves identically to
    /// `system_call_one` (same [`SYSTEM_ADDRESS`] caller and 30M default gas limit)
    /// except it does not `commit_tx`. Used only by
    /// [`call_read_only`](crate::contracts::core_contracts::call_read_only).
    pub(crate) fn system_call_one_no_commit(
        &mut self,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<ExecutionResult<OpHaltReason>, CeloError<CeloContext<DB>>> {
        self.run_system_tx(
            <CeloContext<DB> as ContextTr>::Tx::new_system_tx_with_caller(
                SYSTEM_ADDRESS,
                system_contract_address,
                data,
            ),
            false,
        )
    }

    /// Non-committing counterpart of [`Self::transact_system_call_with_gas_limit`].
    ///
    /// See [`Self::system_call_one_no_commit`]; this variant uses the
    /// [`CELO_SYSTEM_ADDRESS`] caller and a caller-supplied gas limit.
    pub(crate) fn transact_system_call_no_commit_with_gas_limit(
        &mut self,
        system_contract_address: Address,
        data: Bytes,
        gas_limit: u64,
    ) -> Result<ExecutionResult<OpHaltReason>, CeloError<CeloContext<DB>>> {
        self.run_system_tx(
            <CeloContext<DB> as ContextTr>::Tx::new_system_tx_with_gas_limit(
                CELO_SYSTEM_ADDRESS,
                system_contract_address,
                data,
                gas_limit,
            ),
            false,
        )
    }
}
