//! The [CeloStatelessL2Builder] is a block builder that pulls state from a [TrieDB] during
//! execution.

use alloc::{string::ToString, vec::Vec};
use alloy_celo_evm::{
    CeloEvmFactory,
    block::{CeloAlloyReceiptBuilder, CeloBlockExecutorFactory, Upgrade18Overrides},
    cip64_storage::Cip64Storage,
};
use alloy_consensus::{Header, Sealed, crypto::RecoveryError};
use alloy_evm::{
    EvmFactory, RecoveredTx,
    block::{BlockExecutionResult, BlockExecutor, BlockExecutorFactory},
};
use alloy_op_evm::{OpBlockExecutionCtx, PostExecMode};
use celo_alloy_consensus::CeloReceiptEnvelope;
use celo_alloy_rpc_types_engine::CeloPayloadAttributesExt;
use celo_genesis::CeloRollupConfig;
use core::fmt::Debug;
use kona_executor::{ExecutorError, ExecutorResult, TrieDB, TrieDBError, TrieDBProvider};
use kona_mpt::TrieHinter;
use op_alloy_consensus::parse_post_exec_payload_from_transactions;
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use revm::database::{State, states::bundle_state::BundleRetention};

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
    /// building routines. Each `create_executor` call constructs a fresh
    /// [`CeloAlloyReceiptBuilder`] bound to that EVM's own [`Cip64Storage`].
    pub(crate) factory: CeloBlockExecutorFactory<CeloAlloyReceiptBuilder, CeloRollupConfig>,
}

impl<'a, P, H> CeloStatelessL2Builder<'a, P, H>
where
    P: TrieDBProvider + Debug,
    H: TrieHinter + Debug,
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
        let factory = CeloBlockExecutorFactory::new(config.clone(), evm_factory)
            .with_upgrade18_time(config.upgrade18_time)
            .with_upgrade18_overrides(Upgrade18Overrides {
                liquidity_controller_owner: config.upgrade18_liquidity_controller_owner,
                celo_token_l1: config.upgrade18_celo_token_l1,
                celo_gas_bridge_l1: config.upgrade18_celo_gas_bridge_l1,
                native_asset_liquidity_amount: config.upgrade18_native_asset_liquidity_amount,
            });
        Self { config, trie_db, factory }
    }

    /// Builds a new block on top of the parent state, using the given [`OpPayloadAttributes`].
    pub fn build_block(
        &mut self,
        attrs: OpPayloadAttributes,
    ) -> ExecutorResult<CeloBlockBuildingOutcome> {
        // Step 1. Set up the execution environment.
        let (base_fee_params, min_base_fee) = Self::active_base_fee_params(
            self.config,
            self.trie_db.parent_block_header(),
            attrs.payload_attributes.timestamp,
        )?;
        let evm_env = self.evm_env(
            self.config.spec_id(attrs.payload_attributes.timestamp),
            self.trie_db.parent_block_header(),
            &attrs,
            &base_fee_params,
            min_base_fee,
        )?;
        let block_env = evm_env.block_env().clone();
        let parent_hash = self.trie_db.parent_block_header().seal();

        // Attempt to send a payload witness hint to the host. This hint instructs the host to
        // populate its preimage store with the preimages required to statelessly execute
        // this payload. This feature is experimental, so if the hint fails, we continue
        // without it and fall back on on-demand preimage fetching for execution.
        self.trie_db
            .hinter
            .hint_execution_witness(parent_hash, &attrs)
            .map_err(|e| TrieDBError::Provider(e.to_string()))?;

        info!(
            target: "block_builder",
            block_number = %block_env.number,
            block_timestamp = %block_env.timestamp,
            block_gas_limit = block_env.gas_limit,
            transactions = attrs.transactions.as_ref().map_or(0, |txs| txs.len()),
            "Beginning block building."
        );

        // Step 2. Create the executor, using the trie database.
        let mut state =
            State::builder().with_database(&mut self.trie_db).with_bundle_update().build();
        let evm = self.factory.evm_factory().create_evm(&mut state, evm_env);

        // Grab the EVM's own CIP-64 storage before handing the EVM to the executor — the
        // factory binds a fresh receipt builder to it inside `create_executor`, and we
        // also need a handle to return in `CeloBlockBuildingOutcome` so callers (e.g.
        // CIP-64 gas-accounting tests) can read post-execution entries.
        let cip64_storage = evm.cip64_storage().clone();

        // Step 3. Decode and validate the block transactions within the payload attributes.
        let transactions = attrs
            .celo_recovered_transactions_with_encoded()
            .collect::<Result<Vec<_>, RecoveryError>>()
            .map_err(ExecutorError::Recovery)?;
        let sdm_active = self.config.is_sdm_active(block_env.timestamp.saturating_to());
        let post_exec_mode = parse_post_exec_payload_from_transactions(
            transactions.iter().map(RecoveredTx::tx),
            block_env.number.saturating_to(),
            sdm_active,
        )
        .map_err(|err| ExecutorError::InvalidPostExecPayload(err.into_string()))?
        .map(|parsed| PostExecMode::Verify(parsed.payload))
        .unwrap_or_default();

        let ctx = OpBlockExecutionCtx {
            parent_hash,
            parent_beacon_block_root: attrs.payload_attributes.parent_beacon_block_root,
            // This field is unused for individual block building jobs.
            extra_data: Default::default(),
            post_exec_mode,
        };
        let executor = self.factory.create_executor(evm, ctx);

        let ex_result = executor.execute_block(transactions.iter())?;

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
        Ok(CeloBlockBuildingOutcome { header, execution_result: ex_result, cip64_storage })
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
    /// Storage containing CIP-64 transaction execution metrics (gas used, refunds, etc.)
    pub cip64_storage: Cip64Storage,
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

/// Tests that the Upgrade 18 (CGT v2) fixtures actually *constrain* the irregular state
/// transition in the stateless (fault-proof) executor.
///
/// `test_statelessly_execute_block` above already replays both fixtures and checks the block
/// hash. On its own that is weak evidence: a fixture whose expected hash simply never depended
/// on the transition would pass just as green. These tests perturb one transition input at a
/// time and require the block hash to move — and, for the block *after* the boundary, require
/// that it does not.
#[cfg(test)]
mod upgrade18_fixture_tests {
    use crate::test_utils::execute_fixture_with;
    use alloy_primitives::{U256, address};
    use std::path::PathBuf;

    /// The activation block: the transition installs the CGT v2 predeploys.
    const BOUNDARY: &str = "devnet-upgrade18-boundary_block-2.tar.gz";
    /// The block right after it: the completion marker is in the pre-state.
    const POST_BOUNDARY: &str = "devnet-upgrade18-post-boundary_block-3.tar.gz";

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name)
    }

    /// Bump the reserve seed by one wei; the boundary block's state root — and therefore its
    /// hash — must move. This is what proves the fixture pins the transition's state writes,
    /// rather than merely happening to replay.
    #[tokio::test]
    async fn boundary_fixture_pins_the_reserve_seed() {
        let (outcome, fixture) = execute_fixture_with(fixture(BOUNDARY), |config| {
            let seed = config
                .upgrade18_native_asset_liquidity_amount
                .expect("the boundary fixture seeds the reserve");
            config.upgrade18_native_asset_liquidity_amount = Some(seed + U256::from(1));
        })
        .await;

        assert_ne!(
            outcome.expect("Failed to execute block").header.hash(),
            fixture.expected_block_hash,
            "a one-wei change to the reserve seed must break the boundary block hash"
        );
    }

    /// The same, one layer down: `celoGasBridgeL1` resolves into a *storage* word of the
    /// `CeloGasBridgeL2` proxy, where the seed above is a *balance*. Perturbing it must also
    /// break the boundary block hash, so the fixture pins the artifact's storage writes and not
    /// just the mint.
    ///
    /// (Unscheduling the fork outright would not be a clean probe here: this dev genesis ships
    /// none of the CGT predeploys, so a boundary block without the transition has no
    /// `L2ToL1MessagePasser` account to read the Isthmus withdrawals root from, and fails for
    /// that unrelated reason.)
    #[tokio::test]
    async fn boundary_fixture_pins_the_artifact_storage_writes() {
        let (outcome, fixture) = execute_fixture_with(fixture(BOUNDARY), |config| {
            let bridge = config
                .upgrade18_celo_gas_bridge_l1
                .expect("the boundary fixture overrides the L1 bridge address");
            let perturbed = address!("00000000000000000000000000000000000000dd");
            assert_ne!(bridge, perturbed);
            config.upgrade18_celo_gas_bridge_l1 = Some(perturbed);
        })
        .await;

        assert_ne!(
            outcome.expect("Failed to execute block").header.hash(),
            fixture.expected_block_hash,
            "a different `celoGasBridgeL1` must break the boundary block hash"
        );
    }

    /// A scheduled fork whose artifact params cannot be resolved halts the boundary block
    /// rather than silently skipping the migration or seeding a zero reserve. The dev chain is
    /// unknown to the artifact, so clearing the overrides leaves every param unresolvable.
    #[tokio::test]
    async fn boundary_block_halts_when_the_params_are_unresolvable() {
        let (outcome, _) = execute_fixture_with(fixture(BOUNDARY), |config| {
            config.upgrade18_liquidity_controller_owner = None;
            config.upgrade18_celo_token_l1 = None;
            config.upgrade18_celo_gas_bridge_l1 = None;
            config.upgrade18_native_asset_liquidity_amount = None;
        })
        .await;

        assert!(
            outcome.is_err(),
            "an unresolvable artifact param must fail the boundary block, not skip the migration"
        );
    }

    /// Exactly-once, proven through the witness: the marker account read at the start of the
    /// post-boundary block comes from the trie witness, and it short-circuits the transition
    /// *before* parameter resolution. So the block must still reach its expected hash with all
    /// four params cleared — a re-application would instead fail to resolve them (as
    /// [`boundary_block_halts_when_the_params_are_unresolvable`] shows) and, if they were
    /// supplied, would mint the reserve seed a second time.
    #[tokio::test]
    async fn post_boundary_block_does_not_reapply_the_transition() {
        let (outcome, fixture) = execute_fixture_with(fixture(POST_BOUNDARY), |config| {
            assert!(config.upgrade18_time.is_some(), "the fork stays scheduled");
            config.upgrade18_liquidity_controller_owner = None;
            config.upgrade18_celo_token_l1 = None;
            config.upgrade18_celo_gas_bridge_l1 = None;
            config.upgrade18_native_asset_liquidity_amount = None;
        })
        .await;

        assert_eq!(
            outcome.expect("Failed to execute block").header.hash(),
            fixture.expected_block_hash,
            "the transition must not re-apply once its completion marker is in the pre-state"
        );
    }
}

/// Tests for CIP-64 gas calculation verification.
///
/// These tests verify that the gas costs for CIP-64 debit/credit operations
/// match the expected values from op-geth for the same transactions.
#[cfg(test)]
mod cip64_gas_tests {
    use crate::test_utils::load_and_execute_fixture;
    use celo_revm::Cip64Info;
    use std::path::PathBuf;

    /// Runs a test fixture by name and returns the CIP-64 gas metrics directly from execution.
    async fn run_fixture_and_get_cip64_info(fixture_name: &str) -> Option<Cip64Info> {
        // Find the fixture path
        let testdata_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata");
        let fixture_path = testdata_dir
            .read_dir()
            .ok()?
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().contains(fixture_name))?
            .path();

        let (outcome, _fixture) = load_and_execute_fixture(fixture_path).await;

        // Get the first CIP-64 entry from the accumulated storage
        outcome.cip64_storage.all_entries().into_iter().next().map(|data| data.cip64_info)
    }

    /// Test that verifies the CIP-64 gas calculation matches op-geth for the
    /// sepolia-cip64-erc20-transfer fixture.
    ///
    /// Expected values from op-geth (gas_used + gas_refunded):
    /// - Debit: 47756
    /// - Credit: 22997
    /// These values are taken from the call_tracer/celo-kona-comparison.json test in op-geth.
    ///
    /// op-geth calculates: `gasUsed = maxIntrinsicGasCost - leftoverGas`
    /// which equals `gas_used + gas_refunded` in revm terminology.
    #[tokio::test]
    async fn test_cip64_gas_matches_opgeth_sepolia_erc20_transfer() {
        let cip64_info = run_fixture_and_get_cip64_info("sepolia-cip64-erc20-transfer")
            .await
            .expect("Should get CIP-64 info from sepolia-cip64-erc20-transfer fixture");

        // These values match op-geth's gas calculation for the same transaction
        // See: https://github.com/celo-org/op-geth/blob/main/contracts/fee_currencies.go
        //
        // op-geth calculates: gasUsed = maxIntrinsicGasCost - leftoverGas
        // which equals gas_used + gas_refunded in revm terminology
        const EXPECTED_DEBIT_RAW_GAS: u64 = 47756;
        const EXPECTED_CREDIT_RAW_GAS: u64 = 22997;

        let debit_raw_gas = cip64_info.debit_gas_used + cip64_info.debit_gas_refunded;
        let credit_raw_gas = cip64_info.credit_gas_used + cip64_info.credit_gas_refunded;

        assert_eq!(
            debit_raw_gas, EXPECTED_DEBIT_RAW_GAS,
            "Debit raw gas (gas_used + gas_refunded) should match op-geth"
        );

        assert_eq!(
            credit_raw_gas, EXPECTED_CREDIT_RAW_GAS,
            "Credit raw gas (gas_used + gas_refunded) should match op-geth"
        );
    }
}
