use crate::{CeloBlockEnv, CeloContext, CeloEvm, handler::CeloHandler, transaction::CeloTxTr};
use alloy_primitives::{Address, Bytes};
use op_revm::{OpHaltReason, OpSpecId, OpTransactionError};
use revm::{
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
    context::{ContextSetters, JournalOutput},
    context_interface::{
        Cfg, ContextTr, Database, JournalTr,
        result::{EVMError, ExecutionResult, ResultAndState},
    },
    handler::{EthFrame, EvmTr, Handler, SystemCallTx, system_call::SystemCallEvm},
    inspector::{InspectCommitEvm, InspectEvm, Inspector, InspectorHandler},
    interpreter::interpreter::EthInterpreter,
};

// Type alias for Celo context
pub trait CeloContextTr:
    ContextTr<
        Journal: JournalTr<FinalOutput = JournalOutput>,
        Tx: CeloTxTr,
        Cfg: Cfg<Spec = OpSpecId>,
        Chain = CeloBlockEnv,
    >
{
}

impl<T> CeloContextTr for T where
    T: ContextTr<
            Journal: JournalTr<FinalOutput = JournalOutput>,
            Tx: CeloTxTr,
            Cfg: Cfg<Spec = OpSpecId>,
            Chain = CeloBlockEnv,
        >
{
}

/// Type alias for the error type of the CeloEvm.
// TODO: replace with CeloTransactionError
type CeloError<CTX> = EVMError<<<CTX as ContextTr>::Db as Database>::Error, OpTransactionError>;

impl<DB, INSP> ExecuteEvm for CeloEvm<DB, INSP>
where
    DB: Database,
{
    type Output = Result<ResultAndState<OpHaltReason>, CeloError<CeloContext<DB>>>;

    type Tx = <CeloContext<DB> as ContextTr>::Tx;

    type Block = <CeloContext<DB> as ContextTr>::Block;

    fn set_tx(&mut self, tx: Self::Tx) {
        self.0.ctx().set_tx(tx);
    }

    fn set_block(&mut self, block: Self::Block) {
        self.0.ctx().set_block(block);

        // Update the chain environment with fee currencies
        // Warning: If ctx.set_block is called directly, the fee currencies are not updated. I
        // would prefer to place this code in that function, but that would require forking the
        // Context struct.
        match CeloBlockEnv::update_fee_currencies(self) {
            Ok(updated_block_env) => {
                *self.0.ctx().chain() = updated_block_env;
            }
            Err(_e) => {
                // Log and continue. It's better to continue with outdated or unset fee currency
                // information rather than shutting down the node.
                #[cfg(feature = "std")]
                eprintln!("Failed to update fee currencies in set_block: {:?}", _e);
            }
        }
    }

    fn replay(&mut self) -> Self::Output {
        let mut h = CeloHandler::<_, _, EthFrame<_, _, _>>::new();
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
        let mut h = CeloHandler::<_, _, EthFrame<_, _, _>>::new();
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
        let mut h = CeloHandler::<_, _, EthFrame<_, _, _>>::new();
        h.run_system_call(self)
    }
}
