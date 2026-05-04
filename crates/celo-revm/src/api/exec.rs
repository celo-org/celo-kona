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
    handler::{EthFrame, EvmTr, Handler, PrecompileProvider, SystemCallTx},
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
        let mut h = CeloHandler::<
            CeloEvm<DB, INSP, P>,
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
            CeloEvm<DB, INSP, P>,
            CeloError<CeloContext<DB>>,
            EthFrame<EthInterpreter>,
        >::new();
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
        let mut h = CeloHandler::<
            CeloEvm<DB, INSP, P>,
            CeloError<CeloContext<DB>>,
            EthFrame<EthInterpreter>,
        >::new();
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
        self.inner.ctx().set_tx(
            <CeloContext<DB> as ContextTr>::Tx::new_system_tx_with_caller(
                caller,
                system_contract_address,
                data,
            ),
        );
        let mut h = CeloHandler::<
            CeloEvm<DB, INSP, P>,
            CeloError<CeloContext<DB>>,
            EthFrame<EthInterpreter>,
        >::new();
        h.run_system_call(self)
    }
}

impl<DB, INSP, P> CeloEvm<DB, INSP, P>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
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
            CeloEvm<DB, INSP, P>,
            CeloError<CeloContext<DB>>,
            EthFrame<EthInterpreter>,
        >::new();
        h.run_system_call(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CeloBuilder, DefaultCelo};
    use alloy_primitives::U256;
    use revm::{Context, context::BlockEnv, context_interface::ContextTr, database::EmptyDB};

    /// `ExecuteEvm::set_block` must actually update the EVM's block context.
    /// The `replace ... with ()` mutation makes this a no-op and would surface
    /// here as a stale block number.
    #[test]
    fn set_block_replaces_evm_block_context() {
        let ctx = Context::celo().with_db(EmptyDB::default());
        let mut evm = ctx.build_celo();

        // Pick a block number that isn't 0 (the default), so the mutation is
        // unambiguously observable.
        let custom_block = BlockEnv {
            number: U256::from(123_456u64),
            ..BlockEnv::default()
        };
        evm.set_block(custom_block);
        assert_eq!(evm.ctx().block().number, U256::from(123_456u64));
    }

    /// `ExecuteCommitEvm::commit` writes the committed state back into the
    /// EVM's database. The `replace ... with ()` mutation drops the write —
    /// observable by reading back the committed account balance.
    #[test]
    fn commit_persists_state_to_db() {
        use alloy_primitives::map::HashMap;
        use revm::database::InMemoryDB;
        use revm::primitives::Address;
        use revm::state::{Account, AccountInfo};

        let ctx = Context::celo().with_db(InMemoryDB::default());
        let mut evm = ctx.build_celo();

        let target = Address::with_last_byte(0x42);
        let balance = U256::from(987_654_321u64);
        let mut account = Account::from(AccountInfo {
            balance,
            ..Default::default()
        });
        account.mark_touch();
        let mut state = HashMap::default();
        state.insert(target, account);

        evm.commit(state);
        // After commit, the account must be readable from the underlying DB.
        let db_account = evm.ctx().db_mut().load_account(target).expect("loaded");
        assert_eq!(db_account.info.balance, balance);
    }

    /// `InspectEvm::set_inspector` must overwrite `self.inner.0.inspector`. The
    /// `replace … with ()` mutation drops the assignment — observable by
    /// constructing the EVM with one inspector marker, swapping in a second
    /// distinguishable marker, then consuming the EVM via `into_inspector` and
    /// checking which marker came back out.
    #[test]
    fn set_inspector_replaces_inner_inspector() {
        use revm::Inspector;
        use revm::database::EmptyDB;
        use revm::interpreter::interpreter_types::InterpreterTypes;

        // A distinguishable inspector that carries an integer marker. The
        // marker is the only state we care about, so we leave every Inspector
        // method at its default no-op.
        #[derive(Debug)]
        struct MarkerInspector(u32);
        impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for MarkerInspector {}

        let ctx = Context::celo().with_db(EmptyDB::default());
        let mut evm = ctx.build_celo_with_inspector::<MarkerInspector>(MarkerInspector(1));
        evm.set_inspector(MarkerInspector(2));
        // After the swap, `into_inspector` must yield the new marker. The
        // `replace … with ()` mutation makes set_inspector a no-op, leaving
        // the original marker (1) in place.
        let final_inspector: MarkerInspector = evm.into_inspector();
        assert_eq!(
            final_inspector.0, 2,
            "set_inspector must replace the inner inspector"
        );
    }
}
