use crate::{CeloContext, CeloEvm, handler::CeloHandler};
use op_revm::OpTransactionError;
use revm::{
    ExecuteCommitEvm, ExecuteEvm, Inspector,
    context::{ContextTr, Database, result::EVMError, TxEnv},
    handler::{EthFrame, Handler, EvmTr},
    interpreter::interpreter::EthInterpreter,
    primitives::{address, Address, Bytes, TxKind},
};

/// Type alias for the error type of the CeloEvm.
type CeloError<CTX> = EVMError<<<CTX as ContextTr>::Db as Database>::Error, OpTransactionError>;

pub const CELO_SYSTEM_ADDRESS: Address = address!("0x0000000000000000000000000000000000000000");

/// Creates the system transaction with default values and set data and tx call target to system contract address
/// that is going to be called.
///
/// The caller is set to be [`CELO_SYSTEM_ADDRESS`].
///
/// It is used inside [`CeloSystemCallEvm`] and [`SystemCallCommitEvm`] traits to prepare EVM for system call execution.
pub trait CeloSystemCallTx {
    /// Creates new transaction for system call.
    fn new_system_tx(data: Bytes, system_contract_address: Address) -> Self;
}

impl CeloSystemCallTx for TxEnv {
    fn new_system_tx(data: Bytes, system_contract_address: Address) -> Self {
        TxEnv {
            caller: CELO_SYSTEM_ADDRESS,
            data,
            kind: TxKind::Call(system_contract_address),
            gas_limit: 30_000_000,
            ..Default::default()
        }
    }
}

/// API for executing the system calls. System calls dont deduct the caller or reward the
/// beneficiary. They are used before and after block execution to insert or obtain blockchain state.
///
/// It act similar to `transact` function and sets default Tx with data and system contract as a target.
pub trait CeloSystemCallEvm: ExecuteEvm {
    /// System call is a special transaction call that is used to call a system contract.
    ///
    /// Transaction fields are reset and set in [`SystemCallTx`] and data and target are set to
    /// given values.
    ///
    /// Block values are taken into account and will determent how system call will be executed.
    fn transact_system_call(
        &mut self,
        system_contract_address: Address,
        data: Bytes,
    ) -> Self::Output;
}

/// Extension of the [`CeloSystemCallEvm`] trait that adds a method that commits the state after execution.
pub trait CeloSystemCallCommitEvm: CeloSystemCallEvm + ExecuteCommitEvm {
    /// Transact the system call and commit to the state.
    fn transact_system_call_commit(
        &mut self,
        system_contract_address: Address,
        data: Bytes,
    ) -> Self::CommitOutput;
}

impl<DB, INSP> CeloSystemCallEvm for CeloEvm<DB, INSP>
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

impl<DB, INSP> CeloSystemCallCommitEvm for CeloEvm<DB, INSP>
where
    DB: Database + revm::DatabaseCommit,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
{
    fn transact_system_call_commit(
        &mut self,
        system_contract_address: Address,
        data: Bytes,
    ) -> Self::CommitOutput {
        self.transact_system_call(system_contract_address, data)
            .map(|r| {
                self.ctx().db().commit(r.state);
                r.result
            })
    }
}
