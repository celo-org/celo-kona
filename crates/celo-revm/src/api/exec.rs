use crate::{CeloContext, CeloEvm, handler::CeloHandler};
use alloy_primitives::{Address, Bytes};
use op_revm::{OpHaltReason, OpTransactionError};
use revm::{
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
    context_interface::{
        ContextTr, Database,
        result::{EVMError, ExecutionResult, ResultAndState},
    },
    handler::{EthFrame, EvmTr, Handler, SystemCallTx, system_call::SystemCallEvm},
    inspector::{InspectCommitEvm, InspectEvm, Inspector, InspectorHandler},
    interpreter::interpreter::EthInterpreter,
};
use revm_context::ContextSetters;

/// Type alias for the error type of the CeloEvm.
// TODO: replace with CeloTransactionError
type CeloError<CTX> = EVMError<<<CTX as ContextTr>::Db as Database>::Error, OpTransactionError>;

impl<DB, INSP> ExecuteEvm for CeloEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
{
    type Output = Result<ResultAndState<OpHaltReason>, CeloError<CeloContext<DB>>>;

    type Tx = <CeloContext<DB> as ContextTr>::Tx;

    type Block = <CeloContext<DB> as ContextTr>::Block;

    fn set_tx(&mut self, tx: Self::Tx) {
        self.0.ctx().set_tx(tx);
    }

    fn set_block(&mut self, block: Self::Block) {
        self.0.ctx().set_block(block);
    }

    fn replay(&mut self) -> Self::Output {
        let mut h = CeloHandler::<
            CeloEvm<DB, INSP>,
            CeloError<CeloContext<DB>>,
            EthFrame<CeloEvm<DB, INSP>, CeloError<CeloContext<DB>>, EthInterpreter>,
        >::new();
        h.run(self)
    }
}

impl<DB, INSP> ExecuteCommitEvm for CeloEvm<DB, INSP>
where
    DB: Database + DatabaseCommit,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
{
    type CommitOutput = Result<ExecutionResult<OpHaltReason>, CeloError<CeloContext<DB>>>;

    fn replay_commit(&mut self) -> Self::CommitOutput {
        self.replay().map(|r| {
            self.ctx().db().commit(r.state);
            r.result
        })
    }
}

impl<DB, INSP> InspectEvm for CeloEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.0.0.inspector = inspector;
    }

    fn inspect_replay(&mut self) -> Self::Output {
        let mut h = CeloHandler::<
            CeloEvm<DB, INSP>,
            CeloError<CeloContext<DB>>,
            EthFrame<CeloEvm<DB, INSP>, CeloError<CeloContext<DB>>, EthInterpreter>,
        >::new();
        h.inspect_run(self)
    }
}

impl<DB, INSP> InspectCommitEvm for CeloEvm<DB, INSP>
where
    DB: Database + DatabaseCommit,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
{
    fn inspect_replay_commit(&mut self) -> Self::CommitOutput {
        self.inspect_replay().map(|r| {
            self.ctx().db().commit(r.state);
            r.result
        })
    }
}

impl<DB, INSP> SystemCallEvm for CeloEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
{
    fn transact_system_call(
        &mut self,
        system_contract_address: Address,
        data: Bytes,
    ) -> Self::Output {
        self.set_tx(<CeloContext<DB> as ContextTr>::Tx::new_system_tx(
            data,
            system_contract_address,
        ));
        let mut h = CeloHandler::<
            CeloEvm<DB, INSP>,
            CeloError<CeloContext<DB>>,
            EthFrame<CeloEvm<DB, INSP>, CeloError<CeloContext<DB>>, EthInterpreter>,
        >::new();
        h.run_system_call(self)
    }
}
