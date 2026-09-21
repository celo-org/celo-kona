//! Sequencing payload failure-policy lifecycle.

use alloy_celo_evm::revert_evictions::{PayloadBlock, RevertEvictionAttempt, RevertEvictions};
use alloy_consensus::BlockHeader;
use alloy_primitives::B256;
use reth_evm::execute::{
    BlockBuilder, BlockBuilderOutcome, BlockExecutionError, BlockExecutor, ExecutorTx, GasOutput,
};
use reth_storage_api::StateProvider;
use reth_trie_common::updates::TrieUpdates;

/// Associates attempt-local revert records with the exact block produced by a successful finish.
///
/// Dropping this wrapper, consuming its executor, or returning an error from `finish` leaves the
/// shared queue untouched. That keeps aborted in-progress work out of canonical pool maintenance.
#[derive(Debug)]
pub(crate) struct FailurePolicyBlockBuilder<B> {
    inner: B,
    promotion: Option<(RevertEvictionAttempt, RevertEvictions)>,
}

impl<B> FailurePolicyBlockBuilder<B> {
    pub(crate) const fn new(
        inner: B,
        promotion: Option<(RevertEvictionAttempt, RevertEvictions)>,
    ) -> Self {
        Self { inner, promotion }
    }
}

impl<B: BlockBuilder> BlockBuilder for FailurePolicyBlockBuilder<B> {
    type Primitives = B::Primitives;
    type Executor = B::Executor;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.inner.apply_pre_execution_changes()
    }

    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutorTx<Self::Executor>,
        f: impl FnOnce(&<Self::Executor as BlockExecutor>::Result) -> alloy_evm::block::CommitChanges,
    ) -> Result<Option<GasOutput>, BlockExecutionError> {
        self.inner.execute_transaction_with_commit_condition(tx, f)
    }

    fn finish(
        self,
        state_provider: impl StateProvider,
        state_root_precomputed: Option<(B256, TrieUpdates)>,
    ) -> Result<BlockBuilderOutcome<Self::Primitives>, BlockExecutionError> {
        let outcome = self.inner.finish(state_provider, state_root_precomputed)?;
        if let Some((attempt, evictions)) = self.promotion {
            let block = outcome.block.sealed_block();
            evictions.promote_attempt(
                &attempt,
                PayloadBlock::new(block.header().number(), block.hash()),
            );
        }
        Ok(outcome)
    }

    fn executor_mut(&mut self) -> &mut Self::Executor {
        self.inner.executor_mut()
    }

    fn executor(&self) -> &Self::Executor {
        self.inner.executor()
    }

    fn into_executor(self) -> Self::Executor {
        self.inner.into_executor()
    }
}
