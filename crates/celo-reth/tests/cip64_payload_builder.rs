//! Regression test for celo-org/celo-kona#152.
//!
//! Drives `OpPayloadBuilderCtx::execute_best_transactions` end-to-end with a
//! single CIP-64 transaction whose fee-currency-denominated `max_fee_per_gas`
//! is numerically less than the native `base_fee`. Before
//! ethereum-optimism/optimism#20382 (first shipped in op-reth/v2.3.1), the
//! payload builder called `effective_tip_per_gas` on the consensus tx (a plain
//! `CeloTxEnvelope`), which returns `None` for this case and panicked at the
//! `.expect()` site. The fix moves the call before `into_consensus()` so the
//! pool-layer override returns the correct native tip.
//!
//! The test is intentionally heavy (deploys mock FeeCurrencyDirectory / Oracle /
//! ERC20 into an in-memory state); it guards against a future op-reth bump
//! reintroducing a consensus-layer `effective_tip_per_gas` call, which would
//! silently break CIP-64 payload building again.

mod common;

use alloy_consensus::{Header, Signed};
use alloy_primitives::{Address, B256, Signature, TxKind, U256};
use celo_alloy_consensus::{CeloPooledTransaction, CeloTxEnvelope, TxCip64};
use celo_reth::{
    CeloEvmConfig,
    payload_metrics::PayloadMetricsBuilder,
    pool::{CeloPoolTx, ExchangeRate},
    primitives::CeloPrimitives,
};
use metrics::{SharedString, Unit};
use metrics_util::{
    CompositeKey,
    debugging::{DebugValue, DebuggingRecorder},
};
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, HeaderForPayload, MissingPayloadBehaviour, PayloadBuilder,
    PayloadConfig,
};
use reth_evm::execute::BlockBuilder;
use reth_node_api::PayloadBuilderError;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_payload_builder::{
    OpBuiltPayload, OpPayloadBuilderAttributes,
    builder::{ExecutionInfo, OpPayloadBuilderCtx, RethPayloadTransactions},
    config::OpBuilderConfig,
};
use reth_optimism_txpool::OpPooledTransaction as OpPoolPoolTx;
use reth_payload_util::PayloadTransactions;
use reth_primitives_traits::{Recovered, SealedHeader};
use reth_storage_api::noop::NoopProvider;
use reth_transaction_pool::PoolTransaction;
use revm::database::State;
use std::sync::{Arc, Mutex};

use common::{TEST_FC, make_celo_test_db, test_chain_spec};

/// `PayloadTransactions` impl yielding a single `CeloPoolTx`.
struct OneTx(Option<CeloPoolTx>);
impl PayloadTransactions for OneTx {
    type Transaction = CeloPoolTx;
    fn next(&mut self, _ctx: ()) -> Option<Self::Transaction> {
        self.0.take()
    }
    fn mark_invalid(&mut self, _sender: Address, _nonce: u64) {}
}

/// Sender address behind `Signature::test_signature`, derived from a dummy CIP-64 tx.
fn test_sender(sig: Signature) -> Address {
    let dummy = TxCip64 {
        chain_id: 42220,
        nonce: 0,
        gas_limit: 21_000,
        max_fee_per_gas: 1,
        max_priority_fee_per_gas: 1,
        to: TxKind::Call(Address::ZERO),
        value: U256::ZERO,
        access_list: Default::default(),
        input: Default::default(),
        fee_currency: Some(TEST_FC),
    };
    Signed::new_unhashed(dummy, sig).recover_signer().unwrap()
}

/// Parent at 25 Gwei base fee and 50% utilization, so the next block is also 25 Gwei.
/// Cancun-active fields are populated so EIP-4788 pre-execution doesn't reject the build.
fn test_payload_config() -> PayloadConfig<OpPayloadBuilderAttributes<CeloTxEnvelope>, Header> {
    let parent = SealedHeader::seal_slow(Header {
        base_fee_per_gas: Some(25_000_000_000),
        gas_limit: 30_000_000,
        gas_used: 15_000_000,
        timestamp: 0,
        number: 0,
        excess_blob_gas: Some(0),
        blob_gas_used: Some(0),
        parent_beacon_block_root: Some(B256::ZERO),
        ..Default::default()
    });

    // Builder attributes (mostly default; timestamp must exceed parent's).
    let attributes = OpPayloadBuilderAttributes::<CeloTxEnvelope> {
        timestamp: 1,
        suggested_fee_recipient: Address::from([0xfe; 20]),
        parent: parent.hash(),
        parent_beacon_block_root: Some(B256::ZERO),
        gas_limit: Some(30_000_000),
        ..Default::default()
    };

    PayloadConfig {
        parent_header: Arc::new(parent),
        parent_block_info: None,
        attributes,
        payload_id: Default::default(),
    }
}

/// CIP-64 tx with FC `max_fee` = 10 Gwei: well below the native 25 Gwei base fee, but above the
/// FC base fee of 25 Gwei * 1/10 = 2.5 Gwei, so fee-currency validation passes.
fn test_cip64_pool_tx(sender: Address, sig: Signature) -> CeloPoolTx {
    let cip64 = TxCip64 {
        chain_id: 42220,
        nonce: 0,
        gas_limit: 200_000, // generous to cover 50k FC intrinsic + execution
        max_fee_per_gas: 10_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        to: TxKind::Call(Address::ZERO),
        value: U256::ZERO,
        access_list: Default::default(),
        input: Default::default(),
        fee_currency: Some(TEST_FC),
    };
    let envelope = CeloTxEnvelope::Cip64(Signed::new_unhashed(cip64, sig));
    let pooled = CeloPooledTransaction::try_from(envelope).unwrap();
    let inner_pool_tx = OpPoolPoolTx::<CeloTxEnvelope, CeloPooledTransaction>::from_pooled(
        Recovered::new_unchecked(pooled, sender),
    );
    let mut pool_tx = CeloPoolTx::new(inner_pool_tx);
    // 1 FC = 10 native (numerator=1, denominator=10): native_max_fee = 10 Gwei * 10 = 100 Gwei.
    pool_tx.apply_exchange_rate(ExchangeRate { numerator: 1, denominator: 10 });
    pool_tx
}

#[test]
fn cip64_payload_builder_handles_low_fc_max_fee() {
    // Sender with deterministic test signature.
    let sig = Signature::test_signature();
    let sender = test_sender(sig);

    // ── State: FC infrastructure + funded sender.
    let inner_db = make_celo_test_db(sender, U256::from(1_000_000_000_000_000_000u128));
    let mut state = State::builder().with_database(inner_db).with_bundle_update().build();

    let chain_spec = test_chain_spec();
    let evm_config = CeloEvmConfig::celo(chain_spec.clone());

    // This struct literal pins op-reth's private `OpPayloadBuilderCtx` shape, so an
    // op-reth bump that adds/renames a field breaks *compilation* here. That is
    // expected churn — fix the literal and move on. It is categorically different
    // from this test *failing* at runtime: a failure means the #20382 behavior
    // (miner tip computed on the pool tx before `into_consensus`) regressed and
    // CIP-64 payload building is broken again. Don't paper over a runtime failure
    // by adjusting the assertion below.
    let ctx: OpPayloadBuilderCtx<_, OpChainSpec, _> = OpPayloadBuilderCtx {
        evm_config,
        builder_config: OpBuilderConfig::default(),
        chain_spec,
        config: test_payload_config(),
        cancel: Default::default(),
        best_payload: None,
    };

    let best_txs = RethPayloadTransactions(OneTx(Some(test_cip64_pool_tx(sender, sig))));

    // ── Drive execute_best_transactions. Pre-#20382 op-reth panicked inside the
    //    loop because consensus_tx.effective_tip_per_gas(25 Gwei) returns None for
    //    max_fee=10 Gwei, and the .expect() unwrapped it.
    let mut builder = ctx.block_builder(&mut state).expect("block_builder");
    builder.apply_pre_execution_changes().expect("pre-execution");
    let mut info = ExecutionInfo::new();
    ctx.execute_best_transactions(&mut info, &mut builder, best_txs, None, None)
        .expect("execute_best_transactions");

    // Assert the *exact* per-gas tip, not just `total_fees > 0`. A regression
    // that computed a wrong-but-positive tip — FC units leaking through, or the
    // exchange rate never applied — would sail past a `> 0` check; pinning the
    // value catches it. With one tx in the block, `cumulative_gas_used` is this
    // tx's gas and `total_fees == tip_per_gas * gas_used`.
    //
    //   native tip = min(native_max_fee - base_fee, native_max_priority_fee)
    //              = min(100 Gwei - 25 Gwei, 1 Gwei * 10) = 10 Gwei
    const EXPECTED_TIP_PER_GAS: u128 = 10_000_000_000; // 10 Gwei (native)
    assert!(info.cumulative_gas_used > 0, "tx must have executed and consumed gas");
    assert_eq!(
        info.total_fees,
        U256::from(EXPECTED_TIP_PER_GAS) * U256::from(info.cumulative_gas_used),
        "miner fee must equal the native-equivalent tip (10 Gwei) times gas used, \
         not merely be positive"
    );
}

/// A `PayloadBuilder` whose `try_build` runs the given closure. Wrapping it in
/// `PayloadMetricsBuilder` reproduces the production call shape: the attempt scope is opened by
/// the real decorator, and everything the closure does runs inside it.
/// `PayloadBuilder` requires `Send + Sync + Clone` and a `&self` receiver, so the one-shot
/// closure lives behind a shared mutex.
type ProbeWork = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

#[derive(Clone)]
struct SequencingProbe(ProbeWork);

impl SequencingProbe {
    fn new(work: impl FnOnce() + Send + 'static) -> Self {
        Self(Arc::new(Mutex::new(Some(Box::new(work)))))
    }
}

impl PayloadBuilder for SequencingProbe {
    type Attributes = OpPayloadBuilderAttributes<CeloTxEnvelope>;
    type BuiltPayload = OpBuiltPayload<CeloPrimitives>;

    fn try_build(
        &self,
        _args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> Result<BuildOutcome<Self::BuiltPayload>, PayloadBuilderError> {
        (self.0.lock().unwrap().take().expect("probe runs once"))();
        Ok(BuildOutcome::Cancelled)
    }

    fn on_missing_payload(
        &self,
        _args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> MissingPayloadBehaviour<Self::BuiltPayload> {
        MissingPayloadBehaviour::AwaitInProgress
    }

    fn build_empty_payload(
        &self,
        _config: PayloadConfig<Self::Attributes, HeaderForPayload<Self::BuiltPayload>>,
    ) -> Result<Self::BuiltPayload, PayloadBuilderError> {
        unimplemented!("the probe never builds an empty payload")
    }
}

/// One drained snapshot: `Snapshotter::snapshot` empties recorded histogram samples, so a test
/// must take it exactly once and query the result.
type MetricSnapshot = [(CompositeKey, Option<Unit>, Option<SharedString>, DebugValue)];

fn histogram_samples(snapshot: &MetricSnapshot, name: &str, labels: &[(&str, &str)]) -> Vec<f64> {
    let value = snapshot
        .iter()
        .find(|(key, _, _, _)| {
            key.key().name() == name &&
                labels.iter().all(|(wanted_key, wanted_value)| {
                    key.key()
                        .labels()
                        .any(|label| label.key() == *wanted_key && label.value() == *wanted_value)
                })
        })
        .map(|(_, _, _, value)| value)
        .unwrap_or_else(|| panic!("missing metric {name} with labels {labels:?}"));

    match value {
        DebugValue::Histogram(values) => values.iter().map(|value| value.into_inner()).collect(),
        other => panic!("{name} is not a histogram: {other:?}"),
    }
}

/// End-to-end wiring check for celo-org/celo-blockchain-planning#1453.
///
/// Every decorator in `celo_reth::payload_metrics` is transparent, so a unit test can prove the
/// metric contract but not that the instrumentation is actually reached during sequencing. This
/// drives the real chain — `PayloadMetricsBuilder::try_build` opens the attempt,
/// `OpPayloadBuilderCtx::block_builder` goes through `CeloEvmConfig`'s wrapped
/// `post_exec_builder_for_next_block`, and `finish` wraps the state provider — and asserts that
/// one CIP-64 transaction produces a sample in every phase histogram.
#[test]
fn payload_metrics_cover_the_whole_sequencing_path() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();

    let work = || {
        let sig = Signature::test_signature();
        let sender = test_sender(sig);
        let inner_db = make_celo_test_db(sender, U256::from(1_000_000_000_000_000_000u128));
        let mut state = State::builder().with_database(inner_db).with_bundle_update().build();

        let chain_spec = test_chain_spec();
        let ctx: OpPayloadBuilderCtx<_, OpChainSpec, _> = OpPayloadBuilderCtx {
            evm_config: CeloEvmConfig::celo(chain_spec.clone()),
            builder_config: OpBuilderConfig::default(),
            chain_spec,
            config: test_payload_config(),
            cancel: Default::default(),
            best_payload: None,
        };

        let mut builder = ctx.block_builder(&mut state).expect("block_builder");
        builder.apply_pre_execution_changes().expect("pre-execution");
        let mut info = ExecutionInfo::new();
        ctx.execute_best_transactions(
            &mut info,
            &mut builder,
            RethPayloadTransactions(OneTx(Some(test_cip64_pool_tx(sender, sig)))),
            None,
            None,
        )
        .expect("execute_best_transactions");
        assert!(info.cumulative_gas_used > 0, "the CIP-64 tx must have executed");

        // `NoopProvider` returns an empty post state and a zero root, which is enough to reach
        // and measure the real `hashed_post_state` / `state_root_with_updates` calls. Block
        // assembly on top of that empty state may fail; the finalization sample is recorded
        // either way, so the assertions below do not pin its `result` label.
        drop(builder.finish(NoopProvider::default(), None));
    };

    metrics::with_local_recorder(&recorder, || {
        let builder = PayloadMetricsBuilder::new(SequencingProbe::new(work));
        let args = BuildArguments::new(
            Default::default(),
            None,
            None,
            test_payload_config(),
            Default::default(),
            None,
        );
        assert!(matches!(builder.try_build(args), Ok(BuildOutcome::Cancelled)));
    });

    let snapshot = snapshotter.snapshot().into_vec();
    let no_incumbent = [("has_best_payload", "false")];

    assert_eq!(
        histogram_samples(&snapshot, "celo_payload_transaction_execution_calls", &no_incumbent),
        vec![1.0],
        "the single CIP-64 tx must be counted exactly once",
    );
    for name in [
        "celo_payload_build_duration_seconds",
        "celo_payload_transaction_execution_duration_seconds",
        "celo_payload_hashed_post_state_duration_seconds",
        "celo_payload_hashed_post_state_size",
        "celo_payload_trie_updates_size",
    ] {
        assert_eq!(
            histogram_samples(&snapshot, name, &no_incumbent).len(),
            1,
            "missing sequencing sample for {name}",
        );
    }
    // The exact blocking root call is the metric this whole change exists for.
    assert_eq!(
        histogram_samples(
            &snapshot,
            "celo_payload_state_root_duration_seconds",
            &[("has_best_payload", "false"), ("result", "success"),]
        )
        .len(),
        1,
    );
    assert_eq!(
        histogram_samples(
            &snapshot,
            "celo_payload_finalization_duration_seconds",
            &[("has_best_payload", "false"), ("root_source", "blocking"),]
        )
        .len(),
        1,
    );
}
