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
    handler::{EthFrame, EvmTr, Handler, SystemCallTx},
    inspector::{InspectCommitEvm, InspectEvm, Inspector, InspectorHandler},
    interpreter::interpreter::EthInterpreter,
    state::EvmState,
};

/// Type alias for the error type of the CeloEvm.
type CeloError<CTX> = EVMError<<<CTX as ContextTr>::Db as Database>::Error, OpTransactionError>;

impl<DB, INSP> ExecuteEvm for CeloEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
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
        let mut h = CeloHandler::<
            CeloEvm<DB, INSP>,
            CeloError<CeloContext<DB>>,
            EthFrame<EthInterpreter>,
        >::new();
        h.run(self)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.ctx().journal_mut().finalize()
    }

    fn replay(
        &mut self,
    ) -> Result<ExecResultAndState<Self::ExecutionResult, Self::State>, Self::Error> {
        let mut h = CeloHandler::<
            CeloEvm<DB, INSP>,
            CeloError<CeloContext<DB>>,
            EthFrame<EthInterpreter>,
        >::new();
        h.run(self).map(|result| {
            let state = self.finalize();
            ExecResultAndState::new(result, state)
        })
    }
}

impl<DB, INSP> ExecuteCommitEvm for CeloEvm<DB, INSP>
where
    DB: Database + DatabaseCommit,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
{
    fn commit(&mut self, state: Self::State) {
        self.inner.ctx().db_mut().commit(state);
    }
}

impl<DB, INSP> InspectEvm for CeloEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.inner.0.inspector = inspector;
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx().set_tx(tx);
        let mut h = CeloHandler::<
            CeloEvm<DB, INSP>,
            CeloError<CeloContext<DB>>,
            EthFrame<EthInterpreter>,
        >::new();
        h.inspect_run(self)
    }
}

impl<DB, INSP> InspectCommitEvm for CeloEvm<DB, INSP>
where
    DB: Database + DatabaseCommit,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
{
}

impl<DB, INSP> SystemCallEvm for CeloEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
{
    fn system_call_one_with_caller(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx().set_tx(
            <CeloContext<DB> as ContextTr>::Tx::new_system_tx_with_caller(
                caller,
                system_contract_address,
                data,
            ),
        );
        let mut h = CeloHandler::<
            CeloEvm<DB, INSP>,
            CeloError<CeloContext<DB>>,
            EthFrame<EthInterpreter>,
        >::new();
        h.run_system_call(self)
    }
}

impl<DB, INSP> CeloEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
{
    /// Execute a system call with a custom gas limit
    pub fn transact_system_call_with_gas_limit(
        &mut self,
        system_contract_address: Address,
        data: Bytes,
        gas_limit: u64,
    ) -> Result<ExecutionResult<OpHaltReason>, CeloError<CeloContext<DB>>> {
        self.inner.ctx().set_tx(
            <CeloContext<DB> as ContextTr>::Tx::new_system_tx_with_gas_limit(
                CELO_SYSTEM_ADDRESS,
                system_contract_address,
                data,
                gas_limit,
            ),
        );
        let mut h = CeloHandler::<
            CeloEvm<DB, INSP>,
            CeloError<CeloContext<DB>>,
            EthFrame<EthInterpreter>,
        >::new();
        h.run_system_call(self)
    }
}
