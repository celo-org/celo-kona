//! Sequencing payload-builder observability.

use alloy_evm::block::CommitChanges;
use alloy_primitives::B256;
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, HeaderForPayload, MissingPayloadBehaviour, PayloadBuilder,
    PayloadConfig,
};
use reth_evm::execute::{
    BlockBuilder, BlockBuilderOutcome, BlockExecutionError, BlockExecutor, ExecutorTx, GasOutput,
};
use reth_node_api::{PayloadAttributes, PayloadBuilderError};
use reth_node_builder::{BuilderContext, FullNodeTypes, components::PayloadBuilderBuilder};
use reth_primitives_traits::{Account, Bytecode};
use reth_storage_api::{
    AccountReader, BlockHashReader, BytecodeReader, HashedPostStateProvider, StateProofProvider,
    StateProvider, StateRootProvider, StorageRootProvider,
};
use reth_storage_errors::provider::ProviderResult;
use reth_transaction_pool::TransactionPool;
use reth_trie_common::{
    AccountProof, ExecutionWitnessMode, HashedPostState, HashedStorage, MultiProof,
    MultiProofTargets, StorageMultiProof, StorageProof, TrieInput, updates::TrieUpdates,
};
use std::{
    cell::Cell,
    time::{Duration, Instant},
};

thread_local! {
    static ATTEMPT_CONTEXT: Cell<Option<bool>> = const { Cell::new(None) };
}

fn current_attempt_context() -> Option<bool> {
    ATTEMPT_CONTEXT.get()
}

fn with_attempt_context<T>(has_best_payload: bool, f: impl FnOnce() -> T) -> T {
    struct RestoreAttemptContext(Option<bool>);

    impl Drop for RestoreAttemptContext {
        fn drop(&mut self) {
            ATTEMPT_CONTEXT.set(self.0);
        }
    }

    let previous = ATTEMPT_CONTEXT.replace(Some(has_best_payload));
    let _restore = RestoreAttemptContext(previous);
    f()
}

/// Adds Celo payload metrics around the node's payload-builder factory.
///
/// This is the only place production code enters the instrumented path: it hands
/// [`BasicPayloadServiceBuilder`](reth_node_builder::components::BasicPayloadServiceBuilder) a
/// [`PayloadMetricsBuilder`] instead of the bare OP payload builder.
#[derive(Debug, Clone, Default)]
pub struct PayloadMetricsBuilderBuilder<PB> {
    inner: PB,
}

impl<PB> PayloadMetricsBuilderBuilder<PB> {
    /// Wraps an existing payload-builder factory.
    pub const fn new(inner: PB) -> Self {
        Self { inner }
    }
}

impl<Node, Pool, EvmConfig, PB> PayloadBuilderBuilder<Node, Pool, EvmConfig>
    for PayloadMetricsBuilderBuilder<PB>
where
    Node: FullNodeTypes,
    Pool: TransactionPool,
    // `BasicPayloadServiceBuilder` already requires this; repeating it here keeps the wrapper's
    // `async fn` future `Send`, as the trait demands.
    EvmConfig: Send,
    PB: PayloadBuilderBuilder<Node, Pool, EvmConfig>,
{
    type PayloadBuilder = PayloadMetricsBuilder<PB::PayloadBuilder>;

    async fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: EvmConfig,
    ) -> eyre::Result<Self::PayloadBuilder> {
        Ok(PayloadMetricsBuilder::new(
            self.inner.build_payload_builder(ctx, pool, evm_config).await?,
        ))
    }
}

/// Adds Celo payload metrics around an existing synchronous payload builder.
#[derive(Debug, Clone)]
pub struct PayloadMetricsBuilder<B> {
    inner: B,
}

impl<B> PayloadMetricsBuilder<B> {
    /// Wraps an existing payload builder.
    pub const fn new(inner: B) -> Self {
        Self { inner }
    }
}

impl<B: PayloadBuilder> PayloadBuilder for PayloadMetricsBuilder<B> {
    type Attributes = B::Attributes;
    type BuiltPayload = B::BuiltPayload;

    fn try_build(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> Result<BuildOutcome<Self::BuiltPayload>, PayloadBuilderError> {
        struct ActiveGauge(metrics::Gauge);

        impl Drop for ActiveGauge {
            fn drop(&mut self) {
                self.0.decrement(1.0);
            }
        }

        let has_best_payload = args.best_payload.is_some();
        let has_best_label = bool_label(has_best_payload);
        let payload_id = args.config.payload_id();
        let timestamp = args.config.attributes.timestamp();
        let active = metrics::gauge!(
            "celo_payload_builds_active",
            "has_best_payload" => has_best_label,
        );
        active.increment(1.0);
        let _active = ActiveGauge(active);
        let started = Instant::now();

        let result = with_attempt_context(has_best_payload, || self.inner.try_build(args));
        let duration = started.elapsed();
        let outcome = match &result {
            Ok(BuildOutcome::Better { .. }) => "better",
            Ok(BuildOutcome::Aborted { .. }) => "aborted",
            Ok(BuildOutcome::Cancelled) => "cancelled",
            Ok(BuildOutcome::Freeze(_)) => "freeze",
            Err(_) => "error",
        };

        metrics::counter!(
            "celo_payload_build_attempts_total",
            "has_best_payload" => has_best_label,
            "outcome" => outcome,
        )
        .increment(1);
        metrics::histogram!(
            "celo_payload_build_duration_seconds",
            "has_best_payload" => has_best_label,
            "outcome" => outcome,
        )
        .record(duration.as_secs_f64());
        tracing::debug!(
            target: "payload_builder",
            %payload_id,
            timestamp,
            has_best_payload,
            outcome,
            duration_seconds = duration.as_secs_f64(),
            "completed Celo payload build attempt"
        );

        result
    }

    fn on_missing_payload(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> MissingPayloadBehaviour<Self::BuiltPayload> {
        metrics::counter!("celo_payload_get_payload_without_completed_build_total").increment(1);
        self.inner.on_missing_payload(args)
    }

    /// Deliberately left outside the attempt scope, so a block builder created here captures no
    /// context and emits nothing.
    ///
    /// `OpPayloadBuilder::on_missing_payload` is hardcoded to `AwaitInProgress`, which makes
    /// reth's `RaceEmptyPayload` branch unreachable on an OP Stack chain. The only remaining
    /// caller is `PayloadJob::best_payload`, and op-reth documents this method as test-only
    /// because the payload it produces has no L1 system transactions: if it ever fires in
    /// production that is a correctness problem, not a latency budget item, and reth already
    /// counts it via `inc_requested_empty_payload`. Timing a near-zero-work build into the
    /// sequencing histograms would only make those distributions bimodal.
    fn build_empty_payload(
        &self,
        config: PayloadConfig<Self::Attributes, HeaderForPayload<Self::BuiltPayload>>,
    ) -> Result<Self::BuiltPayload, PayloadBuilderError> {
        self.inner.build_empty_payload(config)
    }
}

const fn bool_label(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

const fn result_label(succeeded: bool) -> &'static str {
    if succeeded { "success" } else { "error" }
}

/// Panic message for the block-builder decorator's `Option<B>` slot. `BlockBuilder::finish` and
/// `into_executor` take `self` by value, so the slot can only be emptied once.
const CONSUMED: &str = "block builder already consumed";

/// Adds Celo payload metrics around the block builder used by the sequencing path.
///
/// Transaction execution samples are accumulated in memory and emitted once per builder
/// instance on drop, so an attempt that aborts before `finish` is still measured. Finalization
/// is measured around the inner `finish`, and the supplied [`StateProvider`] is wrapped so the
/// exact `state_root_with_updates` call is timed separately.
#[derive(Debug)]
pub struct PayloadMetricsBlockBuilder<B> {
    inner: Option<B>,
    /// Incumbent-payload flag captured from the enclosing payload attempt. `None` means the
    /// builder was created outside an observed attempt (RPC, derivation, tests), and no payload
    /// metric is emitted.
    has_best_payload: Option<bool>,
    execution_duration: Duration,
    execution_calls: u64,
}

impl<B> PayloadMetricsBlockBuilder<B> {
    /// Wraps a block builder, binding it to the payload attempt active on this thread.
    pub fn new(inner: B) -> Self {
        Self {
            inner: Some(inner),
            has_best_payload: current_attempt_context(),
            execution_duration: Duration::ZERO,
            execution_calls: 0,
        }
    }

    const fn inner(&self) -> &B {
        self.inner.as_ref().expect(CONSUMED)
    }

    const fn inner_mut(&mut self) -> &mut B {
        self.inner.as_mut().expect(CONSUMED)
    }

    const fn take_inner(&mut self) -> B {
        self.inner.take().expect(CONSUMED)
    }

    /// Accumulates one transaction execution. Aggregates are emitted once, on drop.
    fn record_execution(&mut self, elapsed: Duration) {
        self.execution_duration += elapsed;
        self.execution_calls += 1;
    }

    fn record_finalization(&self, root_source: &'static str, succeeded: bool, elapsed: Duration) {
        let Some(has_best_payload) = self.has_best_payload else { return };
        metrics::histogram!(
            "celo_payload_finalization_duration_seconds",
            "has_best_payload" => bool_label(has_best_payload),
            "root_source" => root_source,
            "result" => result_label(succeeded),
        )
        .record(elapsed.as_secs_f64());
    }
}

impl<B> Drop for PayloadMetricsBlockBuilder<B> {
    fn drop(&mut self) {
        let Some(has_best_payload) = self.has_best_payload else { return };
        let has_best_label = bool_label(has_best_payload);
        metrics::histogram!(
            "celo_payload_transaction_execution_duration_seconds",
            "has_best_payload" => has_best_label,
        )
        .record(self.execution_duration.as_secs_f64());
        metrics::histogram!(
            "celo_payload_transaction_execution_calls",
            "has_best_payload" => has_best_label,
        )
        .record(self.execution_calls as f64);
    }
}

impl<B: BlockBuilder> BlockBuilder for PayloadMetricsBlockBuilder<B> {
    type Primitives = B::Primitives;
    type Executor = B::Executor;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.inner_mut().apply_pre_execution_changes()
    }

    // Only this method is instrumented: `execute_transaction` and
    // `execute_transaction_with_result_closure` are left to their trait defaults, which route
    // back through here. Delegating them to the inner builder instead would bypass the timer.
    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutorTx<Self::Executor>,
        f: impl FnOnce(&<Self::Executor as BlockExecutor>::Result) -> CommitChanges,
    ) -> Result<Option<GasOutput>, BlockExecutionError> {
        let started = Instant::now();
        let result = self.inner_mut().execute_transaction_with_commit_condition(tx, f);
        self.record_execution(started.elapsed());
        result
    }

    fn finish(
        mut self,
        state_provider: impl StateProvider,
        state_root_precomputed: Option<(B256, TrieUpdates)>,
    ) -> Result<BlockBuilderOutcome<Self::Primitives>, BlockExecutionError> {
        // Labelled before the call so a future sparse-trie integration is visible as a shift from
        // `blocking` to `precomputed` rather than as a silent disappearance of the root histogram.
        let root_source = if state_root_precomputed.is_some() { "precomputed" } else { "blocking" };
        let inner = self.take_inner();
        let started = Instant::now();
        let outcome = inner.finish(
            PayloadMetricsStateProvider::with_context(state_provider, self.has_best_payload),
            state_root_precomputed,
        );
        self.record_finalization(root_source, outcome.is_ok(), started.elapsed());
        outcome
    }

    fn executor_mut(&mut self) -> &mut Self::Executor {
        self.inner_mut().executor_mut()
    }

    fn executor(&self) -> &Self::Executor {
        self.inner().executor()
    }

    fn into_executor(mut self) -> Self::Executor {
        self.take_inner().into_executor()
    }
}

/// Transparent [`StateProvider`] that times the two calls `BasicBlockBuilder::finish` makes
/// against it. Every other method delegates unchanged.
struct PayloadMetricsStateProvider<P> {
    inner: P,
    has_best_payload: Option<bool>,
}

impl<P> PayloadMetricsStateProvider<P> {
    /// The attempt context is passed in rather than read from the thread-local so the provider
    /// always agrees with the block builder that created it.
    const fn with_context(inner: P, has_best_payload: Option<bool>) -> Self {
        Self { inner, has_best_payload }
    }
}

impl<P: AccountReader> AccountReader for PayloadMetricsStateProvider<P> {
    fn basic_account(
        &self,
        address: &alloy_primitives::Address,
    ) -> ProviderResult<Option<Account>> {
        self.inner.basic_account(address)
    }
}

impl<P: BlockHashReader> BlockHashReader for PayloadMetricsStateProvider<P> {
    fn block_hash(&self, number: u64) -> ProviderResult<Option<alloy_primitives::B256>> {
        self.inner.block_hash(number)
    }

    fn canonical_hashes_range(
        &self,
        start: u64,
        end: u64,
    ) -> ProviderResult<Vec<alloy_primitives::B256>> {
        self.inner.canonical_hashes_range(start, end)
    }
}

impl<P: BytecodeReader> BytecodeReader for PayloadMetricsStateProvider<P> {
    fn bytecode_by_hash(
        &self,
        code_hash: &alloy_primitives::B256,
    ) -> ProviderResult<Option<Bytecode>> {
        self.inner.bytecode_by_hash(code_hash)
    }
}

impl<P: StorageRootProvider> StorageRootProvider for PayloadMetricsStateProvider<P> {
    fn storage_root(
        &self,
        address: alloy_primitives::Address,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<alloy_primitives::B256> {
        self.inner.storage_root(address, hashed_storage)
    }

    fn storage_proof(
        &self,
        address: alloy_primitives::Address,
        slot: alloy_primitives::B256,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        self.inner.storage_proof(address, slot, hashed_storage)
    }

    fn storage_multiproof(
        &self,
        address: alloy_primitives::Address,
        slots: &[alloy_primitives::B256],
        hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        self.inner.storage_multiproof(address, slots, hashed_storage)
    }
}

impl<P: StateProofProvider> StateProofProvider for PayloadMetricsStateProvider<P> {
    fn proof(
        &self,
        input: TrieInput,
        address: alloy_primitives::Address,
        slots: &[alloy_primitives::B256],
    ) -> ProviderResult<AccountProof> {
        self.inner.proof(input, address, slots)
    }

    fn multiproof(
        &self,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        self.inner.multiproof(input, targets)
    }

    fn witness(
        &self,
        input: TrieInput,
        target: HashedPostState,
        mode: ExecutionWitnessMode,
    ) -> ProviderResult<Vec<alloy_primitives::Bytes>> {
        self.inner.witness(input, target, mode)
    }
}

impl<P: HashedPostStateProvider> HashedPostStateProvider for PayloadMetricsStateProvider<P> {
    fn hashed_post_state(&self, bundle_state: &revm::database::BundleState) -> HashedPostState {
        let started = Instant::now();
        let hashed_state = self.inner.hashed_post_state(bundle_state);
        if let Some(has_best_payload) = self.has_best_payload {
            let has_best_label = bool_label(has_best_payload);
            metrics::histogram!(
                "celo_payload_hashed_post_state_duration_seconds",
                "has_best_payload" => has_best_label,
            )
            .record(started.elapsed().as_secs_f64());
            metrics::histogram!(
                "celo_payload_hashed_post_state_size",
                "has_best_payload" => has_best_label,
            )
            // `chunking_length` is reth's own measure of the work the trie walk has to do:
            // changed accounts plus changed slots, counting a wiped storage as one entry.
            .record(hashed_state.chunking_length() as f64);
        }
        hashed_state
    }
}

impl<P: StateRootProvider> StateRootProvider for PayloadMetricsStateProvider<P> {
    fn state_root(&self, state: HashedPostState) -> ProviderResult<alloy_primitives::B256> {
        self.inner.state_root(state)
    }

    fn state_root_from_nodes(&self, input: TrieInput) -> ProviderResult<alloy_primitives::B256> {
        self.inner.state_root_from_nodes(input)
    }

    fn state_root_with_updates(
        &self,
        state: HashedPostState,
    ) -> ProviderResult<(alloy_primitives::B256, TrieUpdates)> {
        let started = Instant::now();
        let result = self.inner.state_root_with_updates(state);
        if let Some(has_best_payload) = self.has_best_payload {
            let has_best_label = bool_label(has_best_payload);
            metrics::histogram!(
                "celo_payload_state_root_duration_seconds",
                "has_best_payload" => has_best_label,
                "result" => result_label(result.is_ok()),
            )
            .record(started.elapsed().as_secs_f64());
            if let Ok((_, updates)) = &result {
                metrics::histogram!(
                    "celo_payload_trie_updates_size",
                    "has_best_payload" => has_best_label,
                )
                .record(trie_updates_size(updates) as f64);
            }
        }
        result
    }

    fn state_root_from_nodes_with_updates(
        &self,
        input: TrieInput,
    ) -> ProviderResult<(alloy_primitives::B256, TrieUpdates)> {
        self.inner.state_root_from_nodes_with_updates(input)
    }
}

impl<P: StateProvider> StateProvider for PayloadMetricsStateProvider<P> {
    fn storage(
        &self,
        account: alloy_primitives::Address,
        storage_key: alloy_primitives::StorageKey,
    ) -> ProviderResult<Option<alloy_primitives::StorageValue>> {
        self.inner.storage(account, storage_key)
    }
}

/// Total trie nodes a successful blocking root computation reported as changed.
///
/// `TrieUpdates` has no `len()`; `StorageTrieUpdates::len()` already folds in the wiped-trie
/// marker, so summing the three collections counts each node exactly once.
fn trie_updates_size(updates: &TrieUpdates) -> usize {
    updates.account_nodes_ref().len() +
        updates.removed_nodes_ref().len() +
        updates.storage_tries_ref().values().map(|storage| storage.len()).sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CeloBlock, CeloTransactionSigned, primitives::CeloPrimitives};
    use alloy_eips::eip4895::Withdrawals;
    use alloy_primitives::{Address, B256, U256};
    use alloy_rpc_types_engine::PayloadId;
    use metrics::{SharedString, Unit};
    use metrics_util::{
        CompositeKey,
        debugging::{DebugValue, DebuggingRecorder, Snapshotter},
    };
    use reth_optimism_payload_builder::{OpBuiltPayload, OpPayloadBuilderAttributes};
    use reth_primitives_traits::{SealedBlock, SealedHeader};
    use reth_storage_api::{HashedPostStateProvider, StateRootProvider, noop::NoopProvider};
    use reth_trie_common::HashedPostState;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    type TestPayload = OpBuiltPayload<CeloPrimitives>;
    type TestAttributes = OpPayloadBuilderAttributes<CeloTransactionSigned>;

    #[derive(Clone, Copy)]
    enum FakeOutcome {
        Better,
        Aborted,
        Cancelled,
        Freeze,
        Error,
    }

    #[derive(Clone)]
    struct FakePayloadBuilder {
        outcome: FakeOutcome,
        missing_called: Arc<AtomicBool>,
    }

    impl FakePayloadBuilder {
        fn new(outcome: FakeOutcome) -> Self {
            Self { outcome, missing_called: Arc::new(AtomicBool::new(false)) }
        }
    }

    impl PayloadBuilder for FakePayloadBuilder {
        type Attributes = TestAttributes;
        type BuiltPayload = TestPayload;

        fn try_build(
            &self,
            args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
        ) -> Result<BuildOutcome<Self::BuiltPayload>, PayloadBuilderError> {
            let payload = test_payload(args.config.payload_id());
            match self.outcome {
                FakeOutcome::Better => {
                    Ok(BuildOutcome::Better { payload, cached_reads: args.cached_reads })
                }
                FakeOutcome::Aborted => {
                    Ok(BuildOutcome::Aborted { fees: U256::ZERO, cached_reads: args.cached_reads })
                }
                FakeOutcome::Cancelled => Ok(BuildOutcome::Cancelled),
                FakeOutcome::Freeze => Ok(BuildOutcome::Freeze(payload)),
                FakeOutcome::Error => Err(PayloadBuilderError::MissingPayload),
            }
        }

        fn on_missing_payload(
            &self,
            _args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
        ) -> MissingPayloadBehaviour<Self::BuiltPayload> {
            self.missing_called.store(true, Ordering::Relaxed);
            MissingPayloadBehaviour::AwaitInProgress
        }

        fn build_empty_payload(
            &self,
            config: PayloadConfig<Self::Attributes, HeaderForPayload<Self::BuiltPayload>>,
        ) -> Result<Self::BuiltPayload, PayloadBuilderError> {
            Ok(test_payload(config.payload_id()))
        }
    }

    fn test_payload(payload_id: PayloadId) -> TestPayload {
        OpBuiltPayload::new(
            payload_id,
            Arc::new(SealedBlock::seal_slow(CeloBlock::default())),
            U256::ZERO,
            None,
        )
    }

    fn test_args(has_best_payload: bool) -> BuildArguments<TestAttributes, TestPayload> {
        let payload_id = PayloadId::default();
        let attributes = OpPayloadBuilderAttributes {
            id: payload_id,
            parent: B256::ZERO,
            timestamp: 123,
            suggested_fee_recipient: Address::ZERO,
            prev_randao: B256::ZERO,
            withdrawals: Withdrawals::default(),
            parent_beacon_block_root: None,
            no_tx_pool: false,
            transactions: Vec::new(),
            gas_limit: None,
            eip_1559_params: None,
            min_base_fee: None,
        };
        BuildArguments::new(
            Default::default(),
            None,
            None,
            PayloadConfig::new(
                Arc::new(SealedHeader::seal_slow(Default::default())),
                attributes,
                payload_id,
            ),
            Default::default(),
            has_best_payload.then(|| test_payload(payload_id)),
        )
    }

    /// One drained snapshot. `Snapshotter::snapshot` empties the recorded histogram samples, so
    /// a test must take it exactly once and then query the result.
    type Snapshot = Vec<(CompositeKey, Option<Unit>, Option<SharedString>, DebugValue)>;

    fn snapshot(snapshotter: &Snapshotter) -> Snapshot {
        snapshotter.snapshot().into_vec()
    }

    /// Builds a state provider bound to the attempt active on this thread, the way
    /// `PayloadMetricsBlockBuilder::finish` does.
    fn observed_provider<P>(inner: P) -> PayloadMetricsStateProvider<P> {
        PayloadMetricsStateProvider::with_context(inner, current_attempt_context())
    }

    fn metric_value<'a>(
        snapshot: &'a Snapshot,
        name: &str,
        labels: &[(&str, &str)],
    ) -> &'a DebugValue {
        snapshot
            .iter()
            .find(|(key, _, _, _)| {
                key.key().name() == name &&
                    labels.iter().all(|(wanted_key, wanted_value)| {
                        key.key().labels().any(|label| {
                            label.key() == *wanted_key && label.value() == *wanted_value
                        })
                    })
            })
            .map(|(_, _, _, value)| value)
            .unwrap_or_else(|| panic!("missing metric {name} with labels {labels:?}"))
    }

    fn histogram_samples(snapshot: &Snapshot, name: &str, labels: &[(&str, &str)]) -> Vec<f64> {
        match metric_value(snapshot, name, labels) {
            DebugValue::Histogram(values) => {
                values.iter().map(|value| value.into_inner()).collect()
            }
            other => panic!("{name} is not a histogram: {other:?}"),
        }
    }

    #[test]
    fn attempt_context_is_scoped() {
        assert_eq!(current_attempt_context(), None);
        with_attempt_context(true, || {
            assert_eq!(current_attempt_context(), Some(true));
        });
        assert_eq!(current_attempt_context(), None);
    }

    #[test]
    fn attempt_metrics_record_all_outcomes_and_cleanup_active_gauge() {
        for (outcome, label) in [
            (FakeOutcome::Better, "better"),
            (FakeOutcome::Aborted, "aborted"),
            (FakeOutcome::Cancelled, "cancelled"),
            (FakeOutcome::Freeze, "freeze"),
            (FakeOutcome::Error, "error"),
        ] {
            let recorder = DebuggingRecorder::new();
            let snapshotter = recorder.snapshotter();
            let builder = PayloadMetricsBuilder::new(FakePayloadBuilder::new(outcome));

            metrics::with_local_recorder(&recorder, || {
                let _ = builder.try_build(test_args(true));
            });

            assert_eq!(
                metric_value(
                    &snapshot(&snapshotter),
                    "celo_payload_build_attempts_total",
                    &[("has_best_payload", "true"), ("outcome", label)],
                ),
                &DebugValue::Counter(1),
            );
        }
    }

    #[test]
    fn failed_attempt_cleans_up_active_gauge_and_records_duration() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let builder = PayloadMetricsBuilder::new(FakePayloadBuilder::new(FakeOutcome::Error));

        metrics::with_local_recorder(&recorder, || {
            assert!(builder.try_build(test_args(false)).is_err());
        });

        let snapshot = snapshot(&snapshotter);
        assert_eq!(
            histogram_samples(
                &snapshot,
                "celo_payload_build_duration_seconds",
                &[("has_best_payload", "false"), ("outcome", "error")],
            )
            .len(),
            1,
        );
        assert_eq!(
            metric_value(&snapshot, "celo_payload_builds_active", &[("has_best_payload", "false")],),
            &DebugValue::Gauge(0.0.into()),
        );
    }

    #[test]
    fn missing_payload_is_metered_and_delegated() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let inner = FakePayloadBuilder::new(FakeOutcome::Cancelled);
        let missing_called = inner.missing_called.clone();
        let builder = PayloadMetricsBuilder::new(inner);

        let behaviour = metrics::with_local_recorder(&recorder, || {
            builder.on_missing_payload(test_args(false))
        });

        assert!(matches!(behaviour, MissingPayloadBehaviour::AwaitInProgress));
        assert!(missing_called.load(Ordering::Relaxed));
        assert_eq!(
            metric_value(
                &snapshot(&snapshotter),
                "celo_payload_get_payload_without_completed_build_total",
                &[],
            ),
            &DebugValue::Counter(1),
        );
    }

    #[test]
    fn block_builder_binds_to_the_enclosing_attempt() {
        assert_eq!(PayloadMetricsBlockBuilder::new(()).has_best_payload, None);
        with_attempt_context(true, || {
            assert_eq!(PayloadMetricsBlockBuilder::new(()).has_best_payload, Some(true));
        });
    }

    #[test]
    fn execution_aggregates_are_emitted_once_per_builder_instance() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            with_attempt_context(true, || {
                let mut builder = PayloadMetricsBlockBuilder::new(());
                builder.record_execution(Duration::from_millis(10));
                builder.record_execution(Duration::from_millis(30));
            });
        });

        let snapshot = snapshot(&snapshotter);
        assert_eq!(
            histogram_samples(
                &snapshot,
                "celo_payload_transaction_execution_duration_seconds",
                &[("has_best_payload", "true")],
            ),
            vec![0.04],
            "the two executions must be summed into a single sample",
        );
        assert_eq!(
            histogram_samples(
                &snapshot,
                "celo_payload_transaction_execution_calls",
                &[("has_best_payload", "true")],
            ),
            vec![2.0],
        );
    }

    #[test]
    fn finalization_records_root_source_and_result() {
        for (precomputed, root_source, succeeded, result) in
            [(false, "blocking", true, "success"), (true, "precomputed", false, "error")]
        {
            let recorder = DebuggingRecorder::new();
            let snapshotter = recorder.snapshotter();

            metrics::with_local_recorder(&recorder, || {
                with_attempt_context(false, || {
                    let builder = PayloadMetricsBlockBuilder::new(());
                    let source = if precomputed { "precomputed" } else { "blocking" };
                    builder.record_finalization(source, succeeded, Duration::from_millis(5));
                });
            });

            assert_eq!(
                histogram_samples(
                    &snapshot(&snapshotter),
                    "celo_payload_finalization_duration_seconds",
                    &[
                        ("has_best_payload", "false"),
                        ("root_source", root_source),
                        ("result", result),
                    ],
                ),
                vec![0.005],
            );
        }
    }

    /// Pending-block RPC, derivation re-execution and debug tracing all reach the same EVM
    /// config. Only a real payload attempt may contribute to the sequencing histograms.
    #[test]
    fn no_payload_metrics_are_emitted_outside_an_attempt() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            let mut builder = PayloadMetricsBlockBuilder::new(());
            builder.record_execution(Duration::from_millis(10));
            builder.record_finalization("blocking", true, Duration::from_millis(5));
            drop(builder);

            let provider = observed_provider(NoopProvider::default());
            let hashed = provider.hashed_post_state(&Default::default());
            let _ = provider.state_root_with_updates(hashed);
        });

        let recorded: Vec<_> = snapshot(&snapshotter)
            .into_iter()
            .map(|(key, _, _, _)| key.key().name().to_string())
            .filter(|name| name.starts_with("celo_payload_"))
            .collect();
        assert!(recorded.is_empty(), "unexpected payload metrics without an attempt: {recorded:?}");
    }

    #[test]
    fn state_provider_records_hash_and_exact_root_samples() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            with_attempt_context(false, || {
                let provider = observed_provider(NoopProvider::default());
                let hashed = provider.hashed_post_state(&Default::default());
                assert_eq!(hashed, HashedPostState::default());
                assert_eq!(
                    provider.state_root_with_updates(hashed).unwrap(),
                    (B256::ZERO, Default::default()),
                );
            });
        });

        let snapshot = snapshot(&snapshotter);
        for name in [
            "celo_payload_hashed_post_state_duration_seconds",
            "celo_payload_hashed_post_state_size",
            "celo_payload_trie_updates_size",
        ] {
            assert_eq!(
                histogram_samples(&snapshot, name, &[("has_best_payload", "false")]).len(),
                1,
                "missing histogram sample for {name}",
            );
        }
        // The exact blocking root call is the metric this whole module exists for, so pin its
        // `result` label rather than accepting any sample.
        assert_eq!(
            histogram_samples(
                &snapshot,
                "celo_payload_state_root_duration_seconds",
                &[("has_best_payload", "false"), ("result", "success"),],
            )
            .len(),
            1,
        );
    }
}
