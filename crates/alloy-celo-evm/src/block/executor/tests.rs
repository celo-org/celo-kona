use super::*;
use alloy_consensus::{SignableTransaction, TxLegacy, transaction::Recovered};
use alloy_evm::block::CommitChanges;
use alloy_op_evm::{
    OpEvm, OpTx, PostExecMode,
    block::{OpAlloyReceiptBuilder, OpBlockExecutionCtx},
    post_exec::{PostExecExecutedTx, PostExecRefundInspector, PostExecTxContext},
};
use alloy_op_hardforks::OpChainHardforks;
use alloy_primitives::{Address, Signature, TxKind, U256};
use op_alloy_consensus::OpTxEnvelope;
use op_revm::{L1BlockInfo, OpBuilder, OpSpecId, OpTransaction};
use revm::{
    Context, MainContext,
    context::{BlockEnv, CfgEnv},
    context_interface::ContextTr,
    database::{CacheDB, EmptyDB, State},
    inspector::{JournalExt, NoOpInspector},
    interpreter::{CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter},
};

// Match the pinned upstream regression's policy: each attempted execution changes the
// snapshot, so a missing rollback is observable even when EVM state is not committed.
#[derive(Debug, Clone, Default)]
struct ScriptedRefundPolicy {
    executed_candidates: u64,
}

impl PostExecRefundInspector for ScriptedRefundPolicy {
    type Snapshot = u64;

    fn begin_tx(&mut self, _ctx: PostExecTxContext) {}

    fn note_account_touch(&mut self, _address: Address) {}

    fn finish_tx(&mut self) -> PostExecExecutedTx {
        self.executed_candidates += 1;
        PostExecExecutedTx { refund_total: self.executed_candidates, refund_events: Vec::new() }
    }

    fn inspect_step<CTX>(&mut self, _interp: &mut Interpreter, _context: &mut CTX)
    where
        CTX: ContextTr<Journal: JournalExt>,
    {
    }

    fn inspect_call<CTX>(&mut self, _context: &mut CTX, _inputs: &mut CallInputs)
    where
        CTX: ContextTr<Journal: JournalExt>,
    {
    }

    fn inspect_call_end<CTX>(
        &mut self,
        _context: &mut CTX,
        _inputs: &CallInputs,
        _outcome: &CallOutcome,
    ) where
        CTX: ContextTr<Journal: JournalExt>,
    {
    }

    fn inspect_create<CTX>(&mut self, _context: &mut CTX, _inputs: &mut CreateInputs)
    where
        CTX: ContextTr<Journal: JournalExt>,
    {
    }

    fn inspect_create_end<CTX>(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        _outcome: &CreateOutcome,
    ) where
        CTX: ContextTr<Journal: JournalExt>,
    {
    }

    fn inspect_selfdestruct(&mut self, _contract: Address, _target: Address, _value: U256) {}

    fn snapshot(&self) -> Self::Snapshot {
        self.executed_candidates
    }

    fn restore(&mut self, snapshot: Self::Snapshot) {
        self.executed_candidates = snapshot;
    }
}

#[test]
fn discarded_candidate_restores_inner_refund_policy() {
    let mut db = State::builder().with_database(CacheDB::<EmptyDB>::default()).build();
    let context = Context::mainnet()
        .with_tx(OpTx(OpTransaction::builder().build_fill()))
        .with_cfg(CfgEnv::new_with_spec(OpSpecId::JOVIAN))
        .with_chain(L1BlockInfo::default())
        .with_db(&mut db)
        .with_block(BlockEnv { gas_limit: 500_000, ..Default::default() });
    let evm: OpEvm<_, _, _, OpTx, ScriptedRefundPolicy> =
        OpEvm::new(context.build_op_with_inspector(NoOpInspector {}), true);
    let mut executor = CeloBlockExecutor::new(OpBlockExecutor::new(
        evm,
        OpBlockExecutionCtx { post_exec_mode: PostExecMode::Produce, ..Default::default() },
        OpChainHardforks::op_mainnet(),
        OpAlloyReceiptBuilder::default(),
    ));
    let tx = |nonce| {
        Recovered::new_unchecked(
            OpTxEnvelope::Legacy(
                TxLegacy {
                    nonce,
                    to: TxKind::Call(Address::with_last_byte(0x42)),
                    gas_limit: 200_000,
                    ..Default::default()
                }
                .into_signed(Signature::new(U256::ZERO, U256::ZERO, false)),
            ),
            Address::ZERO,
        )
    };

    assert!(
        executor
            .execute_transaction_with_commit_condition(&tx(0), |_| CommitChanges::No)
            .expect("declined candidate executes")
            .is_none()
    );
    assert_eq!(executor.refund_snapshot(), 0, "decline must restore the policy");

    assert!(
        executor.execute_transaction_with_commit_condition(&tx(1), |_| CommitChanges::Yes).is_err()
    );
    assert_eq!(executor.refund_snapshot(), 0, "execution error must restore the policy");

    executor.execute_transaction(&tx(0)).expect("committed candidate executes");
    assert_eq!(executor.refund_snapshot(), 1);
}

impl super::super::migration::CgtV2Evm
    for OpEvm<
        &mut State<CacheDB<EmptyDB>>,
        NoOpInspector,
        op_revm::precompiles::OpPrecompiles,
        OpTx,
        ScriptedRefundPolicy,
    >
{
    fn cgt_v2_journal(&mut self) -> &mut revm::context::Journal<Self::DB> {
        &mut self.ctx_mut().journaled_state
    }
    fn read_cgt_v2_vault_config(
        &mut self,
    ) -> Result<super::super::upgrade18::FeeVaultConfig, BlockExecutionError> {
        Err(BlockExecutionError::msg("this refund-policy test does not activate CGTv2"))
    }
}
