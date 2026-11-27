use super::*;
use alloy_consensus::{SignableTransaction, TxLegacy, transaction::Recovered};
use alloy_evm::block::CommitChanges;
use alloy_op_evm::{
    OpEvm, OpTx, PostExecMode,
    block::{OpAlloyReceiptBuilder, OpBlockExecutionCtx},
    post_exec::{
        PostExecExecutedTx, PostExecRefundInspector, PostExecTxContext, RevertToCheckpointError,
    },
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
// checkpoint, so a missing rollback is observable even when EVM state is not committed. With
// `REVERTS = false` it refuses every revert, like a policy handed a checkpoint it did not issue.
#[derive(Debug, Clone, Default)]
struct ScriptedRefundPolicy<const REVERTS: bool = true> {
    executed_candidates: u64,
}

impl<const REVERTS: bool> PostExecRefundInspector for ScriptedRefundPolicy<REVERTS> {
    type Checkpoint = u64;

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

    fn checkpoint(&mut self) -> Self::Checkpoint {
        self.executed_candidates
    }

    fn revert_to_checkpoint(
        &mut self,
        checkpoint: Self::Checkpoint,
    ) -> Result<(), RevertToCheckpointError> {
        if !REVERTS {
            return Err(RevertToCheckpointError::new("scripted to refuse every revert"));
        }
        self.executed_candidates = checkpoint;
        Ok(())
    }
}

fn legacy_tx(nonce: u64) -> Recovered<OpTxEnvelope> {
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
}

/// Runs `f` against a producing [`CeloBlockExecutor`] whose EVM carries refund policy `P`.
fn with_policy_executor<P, T>(
    f: impl FnOnce(
        &mut CeloBlockExecutor<
            OpEvm<
                &mut State<CacheDB<EmptyDB>>,
                NoOpInspector,
                op_revm::precompiles::OpPrecompiles,
                OpTx,
                P,
            >,
            OpAlloyReceiptBuilder,
            OpChainHardforks,
        >,
    ) -> T,
) -> T
where
    P: PostExecRefundInspector + Default,
{
    let mut db = State::builder().with_database(CacheDB::<EmptyDB>::default()).build();
    let context = Context::mainnet()
        .with_tx(OpTx(OpTransaction::builder().build_fill()))
        .with_cfg(CfgEnv::new_with_spec(OpSpecId::JOVIAN))
        .with_chain(L1BlockInfo::default())
        .with_db(&mut db)
        .with_block(BlockEnv { gas_limit: 500_000, ..Default::default() });
    let evm: OpEvm<_, _, _, OpTx, P> =
        OpEvm::new(context.build_op_with_inspector(NoOpInspector {}), true);
    let mut executor = CeloBlockExecutor::new(OpBlockExecutor::new(
        evm,
        OpBlockExecutionCtx { post_exec_mode: PostExecMode::Produce, ..Default::default() },
        OpChainHardforks::op_mainnet(),
        OpAlloyReceiptBuilder::default(),
    ));
    f(&mut executor)
}

#[test]
fn discarded_candidate_reverts_inner_refund_policy() {
    with_policy_executor::<ScriptedRefundPolicy, _>(|executor| {
        assert!(
            executor
                .execute_transaction_with_commit_condition(&legacy_tx(0), |_| CommitChanges::No)
                .expect("declined candidate executes")
                .is_none()
        );
        assert_eq!(executor.inner.evm.refund_checkpoint(), 0, "decline must revert the policy");

        assert!(
            executor
                .execute_transaction_with_commit_condition(&legacy_tx(1), |_| CommitChanges::Yes)
                .is_err()
        );
        assert_eq!(
            executor.inner.evm.refund_checkpoint(),
            0,
            "execution error must revert the policy"
        );

        executor.execute_transaction(&legacy_tx(0)).expect("committed candidate executes");
        assert_eq!(executor.inner.evm.refund_checkpoint(), 1);
    });
}

#[test]
fn resume_at_tx_index_is_refused() {
    with_policy_executor::<ScriptedRefundPolicy, _>(|executor| {
        let err = PostExecExecutorExt::resume_at_tx_index(executor, 1)
            .expect_err("a Celo executor cannot resume mid-block");
        assert!(err.to_string().contains("CGTv2 group state is lost"), "{err}");
    });
}

fn assert_refund_policy_revert_error(err: &BlockExecutionError) {
    let revert_error = RevertToCheckpointError::from_block_execution_error(err)
        .unwrap_or_else(|| panic!("expected the policy's revert error, got {err}"));
    assert_eq!(revert_error.reason(), "scripted to refuse every revert");
}

#[test]
fn declined_candidate_fails_when_refund_policy_cannot_revert() {
    with_policy_executor::<ScriptedRefundPolicy<false>, _>(|executor| {
        let err = executor
            .execute_transaction_with_commit_condition(&legacy_tx(0), |_| CommitChanges::No)
            .expect_err("a policy that cannot undo the declined candidate fails the call");
        assert_refund_policy_revert_error(&err);
    });
}

// The policy's revert error outranks the candidate's own execution error: a caller that saw only
// the invalid transaction would skip it and keep building on unspecified policy state.
#[test]
fn failed_candidate_reports_refund_policy_revert_error_over_its_own() {
    with_policy_executor::<ScriptedRefundPolicy<false>, _>(|executor| {
        let err = executor
            .execute_transaction(&legacy_tx(7))
            .expect_err("an invalid transaction whose rollback also fails errors");
        assert_refund_policy_revert_error(&err);
    });
}
