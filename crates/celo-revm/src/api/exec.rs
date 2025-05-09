use crate::{CeloEvm, transaction::CeloTxTr};
use op_revm::{L1BlockInfo, OpHaltReason, OpSpecId, OpTransactionError, handler::OpHandler};
use revm::{
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
    context::{ContextSetters, JournalOutput},
    context_interface::{
        Cfg, ContextTr, Database, JournalTr,
        result::{EVMError, ExecutionResult, ResultAndState},
    },
    handler::{
        EthFrame, EvmTr, Handler, PrecompileProvider, SystemCallTx, instructions::EthInstructions,
        system_call::SystemCallEvm,
    },
    inspector::{InspectCommitEvm, InspectEvm, Inspector, InspectorHandler, JournalExt},
    interpreter::{InterpreterResult, interpreter::EthInterpreter},
};

// Type alias for Celo context
pub trait CeloContextTr:
    ContextTr<
        Journal: JournalTr<FinalOutput = JournalOutput>,
        Tx: CeloTxTr,
        Cfg: Cfg<Spec = OpSpecId>,
        Chain = L1BlockInfo,
    >
{
}

impl<T> CeloContextTr for T where
    T: ContextTr<
            Journal: JournalTr<FinalOutput = JournalOutput>,
            Tx: CeloTxTr,
            Cfg: Cfg<Spec = OpSpecId>,
            Chain = L1BlockInfo,
        >
{
}

/// Type alias for the error type of the CeloEvm.
// TODO: replace with CeloTransactionError
type CeloError<CTX> = EVMError<<<CTX as ContextTr>::Db as Database>::Error, OpTransactionError>;

impl<CTX, INSP, PRECOMPILE> ExecuteEvm
    for CeloEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: CeloContextTr + ContextSetters,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    type Output = Result<ResultAndState<OpHaltReason>, CeloError<CTX>>;

    type Tx = <CTX as ContextTr>::Tx;

    type Block = <CTX as ContextTr>::Block;

    fn set_tx(&mut self, tx: Self::Tx) {
        self.0.ctx().set_tx(tx);
    }

    fn set_block(&mut self, block: Self::Block) {
        self.0.ctx().set_block(block);
    }

    fn replay(&mut self) -> Self::Output {
        // TODO: replace with CeloHandler
        let mut h = OpHandler::<_, _, EthFrame<_, _, _>>::new();
        h.run(self)
    }
}

impl<CTX, INSP, PRECOMPILE> ExecuteCommitEvm
    for CeloEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: CeloContextTr<Db: DatabaseCommit> + ContextSetters,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    type CommitOutput = Result<ExecutionResult<OpHaltReason>, CeloError<CTX>>;

    fn replay_commit(&mut self) -> Self::CommitOutput {
        self.replay().map(|r| {
            self.ctx().db().commit(r.state);
            r.result
        })
    }
}

impl<CTX, INSP, PRECOMPILE> InspectEvm
    for CeloEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: CeloContextTr<Journal: JournalExt> + ContextSetters,
    INSP: Inspector<CTX, EthInterpreter>,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.0.0.data.inspector = inspector;
    }

    fn inspect_replay(&mut self) -> Self::Output {
        // TODO: replace with CeloHandler
        let mut h = OpHandler::<_, _, EthFrame<_, _, _>>::new();
        h.inspect_run(self)
    }
}

impl<CTX, INSP, PRECOMPILE> InspectCommitEvm
    for CeloEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: CeloContextTr<Journal: JournalExt, Db: DatabaseCommit> + ContextSetters,
    INSP: Inspector<CTX, EthInterpreter>,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    fn inspect_replay_commit(&mut self) -> Self::CommitOutput {
        self.inspect_replay().map(|r| {
            self.ctx().db().commit(r.state);
            r.result
        })
    }
}

impl<CTX, INSP, PRECOMPILE> SystemCallEvm
    for CeloEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: CeloContextTr<Tx: SystemCallTx> + ContextSetters,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    fn transact_system_call(
        &mut self,
        data: revm::primitives::Bytes,
        system_contract_address: revm::primitives::Address,
    ) -> Self::Output {
        self.set_tx(CTX::Tx::new_system_tx(data, system_contract_address));
        // TODO: replace with CeloHandler
        let mut h = OpHandler::<_, _, EthFrame<_, _, _>>::new();
        h.run_system_call(self)
    }
}
