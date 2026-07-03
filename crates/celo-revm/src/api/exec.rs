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

    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx().set_tx(tx);
        let mut h =
            CeloHandler::<Self, CeloError<CeloContext<DB>>, EthFrame<EthInterpreter>>::new();
        h.run(self)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.ctx().journal_mut().finalize()
    }

    fn replay(
        &mut self,
    ) -> Result<ExecResultAndState<Self::ExecutionResult, Self::State>, Self::Error> {
        let mut h =
            CeloHandler::<Self, CeloError<CeloContext<DB>>, EthFrame<EthInterpreter>>::new();
        h.run(self).map(|result| {
            let state = self.finalize();
            ExecResultAndState::new(result, state)
        })
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

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx().set_tx(tx);
        let mut h =
            CeloHandler::<Self, CeloError<CeloContext<DB>>, EthFrame<EthInterpreter>>::new();
        h.inspect_run(self)
    }
}

impl<DB, INSP, P> InspectCommitEvm for CeloEvm<DB, INSP, P>
where
    DB: Database + DatabaseCommit,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
{
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
