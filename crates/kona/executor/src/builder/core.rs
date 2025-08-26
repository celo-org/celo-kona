//! The [CeloStatelessL2Builder] is a block builder that pulls state from a [TrieDB] during
//! execution.

use alloc::{string::ToString, vec::Vec};
use alloy_celo_evm::{CeloEvmFactory, block::CeloAlloyReceiptBuilder};
use alloy_consensus::{Header, Sealed, crypto::RecoveryError};
use alloy_evm::{
    Evm,
    EvmFactory,
    block::{BlockExecutionResult, BlockExecutor, BlockExecutorFactory},
};
use alloy_op_evm::{OpBlockExecutionCtx, OpBlockExecutorFactory};
use celo_alloy_consensus::CeloReceiptEnvelope;
use celo_alloy_rpc_types_engine::CeloPayloadAttributes;
use celo_genesis::CeloRollupConfig;
use kona_executor::{ExecutorError, ExecutorResult, TrieDB, TrieDBError, TrieDBProvider};
use kona_mpt::TrieHinter;
use revm::{database::{states::bundle_state::BundleRetention, State}};
use revm_trace::TxInspector;

/// The [`CeloStatelessL2Builder`] is a Celo block builder that traverses a merkle patricia trie
/// via the [`TrieDB`] during execution.
#[derive(Debug)]
pub struct CeloStatelessL2Builder<'a, P, H>
where
    P: TrieDBProvider,
    H: TrieHinter,
{
    /// The [CeloRollupConfig].
    pub(crate) config: &'a CeloRollupConfig,
    /// The inner trie database.
    pub(crate) trie_db: TrieDB<P, H>,
    #[allow(rustdoc::broken_intra_doc_links)]
    /// The executor factory, used to create new [`celo_revm::CeloEvm`] instances for block
    /// building routines.
    pub(crate) factory:
        OpBlockExecutorFactory<CeloAlloyReceiptBuilder, CeloRollupConfig, CeloEvmFactory>,
}

impl<'a, P, H> CeloStatelessL2Builder<'a, P, H>
where
    P: TrieDBProvider,
    H: TrieHinter,
{
    /// Creates a new [CeloStatelessL2Builder] instance.
    pub fn new(
        config: &'a CeloRollupConfig,
        evm_factory: CeloEvmFactory,
        provider: P,
        hinter: H,
        parent_header: Sealed<Header>,
    ) -> Self {
        let trie_db = TrieDB::new(parent_header, provider, hinter);
        let factory = OpBlockExecutorFactory::new(
            CeloAlloyReceiptBuilder::default(),
            config.clone(),
            evm_factory,
        );
        Self { config, trie_db, factory }
    }

    /// Builds a new block on top of the parent state, using the given [`CeloPayloadAttributes`].
    pub fn build_block(
        &mut self,
        attrs: CeloPayloadAttributes,
    ) -> ExecutorResult<CeloBlockBuildingOutcome> {
        let op_attrs = attrs.op_payload_attributes.clone();

        // Step 1. Set up the execution environment.
        let base_fee_params =
            Self::active_base_fee_params(self.config, self.trie_db.parent_block_header(), &attrs)?;
        let evm_env = self.evm_env(
            self.config.op_rollup_config.spec_id(op_attrs.payload_attributes.timestamp),
            self.trie_db.parent_block_header(),
            &attrs,
            &base_fee_params,
        )?;
        let block_env = evm_env.block_env().clone();
        let parent_hash = self.trie_db.parent_block_header().seal();

        // Attempt to send a payload witness hint to the host. This hint instructs the host to
        // populate its preimage store with the preimages required to statelessly execute
        // this payload. This feature is experimental, so if the hint fails, we continue
        // without it and fall back on on-demand preimage fetching for execution.
        self.trie_db
            .hinter
            .hint_execution_witness(parent_hash, &op_attrs)
            .map_err(|e| TrieDBError::Provider(e.to_string()))?;

        info!(
            target: "block_builder",
            block_number = block_env.number,
            block_timestamp = block_env.timestamp,
            block_gas_limit = block_env.gas_limit,
            transactions = op_attrs.transactions.as_ref().map_or(0, |txs| txs.len()),
            "Beginning block building."
        );

        // Step 2. Create the executor, using the trie database.
        let mut state = State::builder()
            .with_database(&mut self.trie_db)
            .with_bundle_update()
            .without_state_clear()
            .build();
        warn!(target: "build_block", "Creating new evm with spec: {:?}", evm_env.cfg_env.spec);

        let tx_inspector = TxInspector::default();
        info!(target: "build_block", "Created TxInspector for transaction tracing");
        let mut evm = self.factory.evm_factory().create_evm_with_inspector(&mut state, evm_env.clone(), tx_inspector);
        warn!(target: "build_block", "After new evm with inspector");

        // Update the receipt builder to include the fee currency context and CIP-64 storage.
        // We couldn't do this earlier because we need an EVM to populate the fee currency context.
        let fee_currency_context = evm.create_fee_currency_context().unwrap_or_default();
        let cip64_storage = evm.cip64_storage().clone();
        let updated_receipt_builder =
            CeloAlloyReceiptBuilder::new(fee_currency_context, cip64_storage);
        let factory = OpBlockExecutorFactory::<
            CeloAlloyReceiptBuilder,
            CeloRollupConfig,
            CeloEvmFactory,
        >::new(
            updated_receipt_builder, self.config.clone(), *self.factory.evm_factory()
        );

        // Step 3. Execute the block containing the transactions within the payload attributes.
        let transactions = attrs
            .recovered_transactions_with_encoded()
            .collect::<Result<Vec<_>, RecoveryError>>()
            .map_err(ExecutorError::Recovery)?;
        info!(target: "build_block", "Executing block with {} transactions using TxInspector", transactions.len());
        
        // Execute each transaction individually to access traces
        for (i, tx) in transactions.iter().enumerate() {
            info!(target: "tx_trace", "Executing transaction {}/{}", i + 1, transactions.len());
            let tx_result = evm.transact(tx.clone());
            
            match tx_result {
                Ok(_) => {
                    info!(target: "tx_trace", "Transaction {} completed successfully", i + 1);
                }
                Err(e) => {
                    warn!(target: "tx_trace", "Transaction {} failed: {:?}", i + 1, e);
                }
            }
        }
        
        // Print traces from the inspector
        print_tx_inspector(evm.inspector());
        
        // Now create a new EVM without inspector for the actual block execution
        let clean_evm = self.factory.evm_factory().create_evm(&mut state, evm_env);
        let ctx = OpBlockExecutionCtx {
            parent_hash,
            parent_beacon_block_root: op_attrs.payload_attributes.parent_beacon_block_root,
            // This field is unused for individual block building jobs.
            extra_data: Default::default(),
        };
        let executor = factory.create_executor(clean_evm, ctx);
        let ex_result = executor.execute_block(transactions.iter())?;
        
        info!(target: "build_block", "Block execution completed. TxInspector captured {} receipts with call traces.", ex_result.receipts.len());

        info!(
            target: "block_builder",
            gas_used = ex_result.gas_used,
            gas_limit = block_env.gas_limit,
            "Finished block building. Beginning sealing job."
        );

        // Step 4. Merge state transitions and seal the block.
        state.merge_transitions(BundleRetention::Reverts);
        let bundle = state.take_bundle();
        let header = self.seal_block(&attrs, parent_hash, &block_env, &ex_result, bundle)?;

        info!(
            target: "block_builder",
            number = header.number,
            hash = ?header.seal(),
            state_root = ?header.state_root,
            transactions_root = ?header.transactions_root,
            receipts_root = ?header.receipts_root,
            "Sealed new block",
        );

        // Update the parent block hash in the state database, preparing for the next block.
        self.trie_db.set_parent_block_header(header.clone());
        Ok((header, ex_result).into())
    }
}

fn print_tx_inspector(tx_inspector: &TxInspector) {
    let traces = tx_inspector.get_traces();
    for (i, trace) in traces.iter().enumerate() {
        info!(target: "build_block", "  Trace {}: from={:?} to={:?} value={} status={:?}", 
            i, trace.from, trace.to, trace.value, trace.status);
        info!(target: "build_block", "Trace: {:?}", serde_json::to_string_pretty(&trace));
    }
}
/// The outcome of a block building operation, returning the sealed block [`Header`] and the
/// [`BlockExecutionResult`].
#[derive(Debug, Clone)]
pub struct CeloBlockBuildingOutcome {
    /// The block header.
    pub header: Sealed<Header>,
    /// The block execution result.
    pub execution_result: BlockExecutionResult<CeloReceiptEnvelope>,
}

impl From<(Sealed<Header>, BlockExecutionResult<CeloReceiptEnvelope>)>
    for CeloBlockBuildingOutcome
{
    fn from(
        (header, execution_result): (Sealed<Header>, BlockExecutionResult<CeloReceiptEnvelope>),
    ) -> Self {
        Self { header, execution_result }
    }
}

#[cfg(test)]
mod test {
    use crate::test_utils::run_test_fixture;
    use rstest::rstest;
    use std::path::PathBuf;

    #[rstest]
    #[tokio::test]
    async fn test_statelessly_execute_block(
        #[base_dir = "./testdata"]
        #[files("*.tar.gz")]
        path: PathBuf,
    ) {
        run_test_fixture(path).await;
    }
}
