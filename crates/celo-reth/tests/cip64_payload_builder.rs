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
//!
//! The same machinery also drives `CeloPayloadTransactions` over a transaction the base fee
//! prices out, guarding the filter that keeps such a transaction away from the builder's
//! `expect` (celo-org/celo-blockchain-planning#1463).

mod common;

use alloy_celo_evm::blocklist::FeeCurrencyBlocklist;
use alloy_consensus::{Header, Signed, Transaction, TxEip1559};
use alloy_primitives::{Address, B256, Signature, TxKind, U256, address, hex};
use celo_alloy_consensus::{CeloPooledTransaction, CeloTxEnvelope, TxCip64};
use celo_reth::{
    CeloEvmConfig, OpNextBlockEnvAttributes,
    payload::{CeloPayloadTransactions, FeeCurrencyLimits},
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
use reth_evm::{ConfigureEvm, Evm, execute::BlockBuilder};
use reth_node_api::PayloadBuilderError;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_payload_builder::{
    OpBuiltPayload, OpPayloadAttrs, OpPayloadBuilderAttributes,
    builder::{ExecutionInfo, OpPayloadBuilderCtx, RethPayloadTransactions},
    config::OpBuilderConfig,
};
use reth_optimism_txpool::OpPooledTransaction as OpPoolPoolTx;
use reth_payload_util::PayloadTransactions;
use reth_primitives_traits::{Recovered, SealedHeader};
use reth_storage_api::noop::NoopProvider;
use reth_transaction_pool::PoolTransaction;
use revm::{
    database::State,
    state::{AccountInfo, Bytecode},
};
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

/// `PayloadTransactions` impl yielding several `CeloPoolTx` in order, dropping a sender's
/// remaining transactions when it is marked invalid, the way the pool's iterator does. The
/// record of `mark_invalid` calls is shared because the iterator is moved into the filter.
struct QueuedTxs {
    txs: Vec<CeloPoolTx>,
    invalid: Arc<Mutex<Vec<(Address, u64)>>>,
}

impl PayloadTransactions for QueuedTxs {
    type Transaction = CeloPoolTx;
    fn next(&mut self, _ctx: ()) -> Option<Self::Transaction> {
        if self.txs.is_empty() { None } else { Some(self.txs.remove(0)) }
    }
    fn mark_invalid(&mut self, sender: Address, nonce: u64) {
        self.invalid.lock().unwrap().push((sender, nonce));
        self.txs.retain(|tx| tx.sender() != sender);
    }
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

fn test_attempt_config(no_tx_pool: Option<bool>) -> PayloadConfig<OpPayloadAttrs, Header> {
    let config = test_payload_config();
    let attrs = config.attributes;
    PayloadConfig {
        parent_header: config.parent_header,
        parent_block_info: None,
        payload_id: config.payload_id,
        attributes: OpPayloadAttrs(op_alloy_rpc_types_engine::OpPayloadAttributes {
            payload_attributes: alloy_rpc_types_engine::PayloadAttributes {
                timestamp: attrs.timestamp,
                suggested_fee_recipient: attrs.suggested_fee_recipient,
                prev_randao: attrs.prev_randao,
                withdrawals: Some(Vec::new()),
                parent_beacon_block_root: attrs.parent_beacon_block_root,
                ..Default::default()
            },
            no_tx_pool,
            gas_limit: attrs.gas_limit,
            ..Default::default()
        }),
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

/// `balanceOf` returns a high balance, while `debitGasFees` loops until its gas runs out.
fn halt_on_debit_code() -> Bytecode {
    Bytecode::new_raw(hex!("6024361160125760001960005260206000f35b601256").to_vec().into())
}

fn next_block_env() -> OpNextBlockEnvAttributes {
    OpNextBlockEnvAttributes {
        timestamp: 1,
        suggested_fee_recipient: Address::from([0xfe; 20]),
        prev_randao: B256::ZERO,
        gas_limit: 30_000_000,
        parent_beacon_block_root: Some(B256::ZERO),
        extra_data: Default::default(),
    }
}

#[test]
fn pending_builder_does_not_blocklist() {
    let sig = Signature::test_signature();
    let sender = test_sender(sig);
    let blocklist = alloy_celo_evm::blocklist::FeeCurrencyBlocklist::default();
    let config = CeloEvmConfig::celo_with_blocklist(test_chain_spec(), blocklist.clone());
    let parent = test_payload_config().parent_header;

    let mut pending_db = make_celo_test_db(sender, U256::MAX);
    pending_db.insert_account_info(TEST_FC, AccountInfo::from_bytecode(halt_on_debit_code()));
    let mut pending_state = State::builder().with_database(pending_db).with_bundle_update().build();
    let mut pending = config
        .builder_for_next_block(&mut pending_state, &parent, next_block_env())
        .expect("pending builder");
    pending.apply_pre_execution_changes().expect("pending pre-execution");
    let pending_error = pending
        .execute_transaction(test_cip64_pool_tx(sender, sig).into_consensus())
        .expect_err("debit must halt");
    assert!(
        format!("{pending_error}").contains(celo_revm::constants::FEE_CURRENCY_HALT_MARKER),
        "pending failure must come from the debit halt: {pending_error}"
    );
    assert!(!blocklist.is_blocked(TEST_FC), "pending block must not mutate shared blocklist");
}

#[test]
fn payload_failure_policy_requires_a_pool_enabled_attempt() {
    for no_tx_pool in [Some(false), None, Some(true)] {
        let enabled = !no_tx_pool.unwrap_or_default();
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let blocklist = alloy_celo_evm::blocklist::FeeCurrencyBlocklist::default();
        let shared_blocklist = blocklist.clone();
        let work = move || {
            let sig = Signature::test_signature();
            let sender = test_sender(sig);
            let mut db = make_celo_test_db(sender, U256::MAX);
            db.insert_account_info(TEST_FC, AccountInfo::from_bytecode(halt_on_debit_code()));
            let mut state = State::builder().with_database(db).with_bundle_update().build();
            let chain_spec = test_chain_spec();
            let mut config = test_payload_config();
            config.attributes.no_tx_pool = no_tx_pool.unwrap_or_default();
            let ctx = OpPayloadBuilderCtx {
                evm_config: CeloEvmConfig::celo_with_blocklist(
                    chain_spec.clone(),
                    shared_blocklist,
                ),
                builder_config: OpBuilderConfig::default(),
                chain_spec,
                config,
                cancel: Default::default(),
                best_payload: None,
            };
            let mut builder = ctx.block_builder(&mut state).expect("post-exec builder");
            builder.apply_pre_execution_changes().expect("pre-execution");
            let error = builder
                .execute_transaction(test_cip64_pool_tx(sender, sig).into_consensus())
                .expect_err("debit must halt in both sequencing and derivation");
            assert!(format!("{error}").contains(celo_revm::constants::FEE_CURRENCY_HALT_MARKER));
        };
        metrics::with_local_recorder(&recorder, || {
            let builder = PayloadMetricsBuilder::new(SequencingProbe::new(work));
            let args = BuildArguments::new(
                Default::default(),
                None,
                None,
                test_attempt_config(no_tx_pool),
                Default::default(),
                None,
            );
            assert!(matches!(builder.try_build(args), Ok(BuildOutcome::Cancelled)));
        });
        assert_eq!(blocklist.is_blocked(TEST_FC), enabled);
        let snapshot = snapshotter.snapshot().into_vec();
        let failures: Vec<_> = snapshot
            .iter()
            .filter(|(key, _, _, _)| key.key().name() == "celo_payload_fee_currency_failures_total")
            .collect();
        assert_eq!(failures.len(), usize::from(enabled));
        if enabled {
            let (key, _, _, value) = failures[0];
            assert_eq!(
                key.key(),
                &metrics::Key::from_parts(
                    "celo_payload_fee_currency_failures_total",
                    &[("phase", "debit"), ("kind", "halt")],
                )
            );
            assert_eq!(*value, DebugValue::Counter(1));
        }
    }
}

#[test]
fn witness_execution_does_not_update_failure_policy() {
    use alloy_eips::eip2718::Encodable2718;
    use reth_optimism_payload_builder::builder::OpBuilder;
    use reth_primitives_traits::WithEncoded;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};

    // Exercise the same upstream witness method used by debug_executePayload. The RPC can
    // supply either noTxPool value, so neither may opt in to sequencing policy.
    for no_tx_pool in [false, true] {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let blocklist = alloy_celo_evm::blocklist::FeeCurrencyBlocklist::default();
        let sig = Signature::test_signature();
        let sender = test_sender(sig);
        let mut db = make_celo_test_db(sender, U256::MAX);
        let token_code = halt_on_debit_code();
        db.insert_account_info(TEST_FC, AccountInfo::from_bytecode(token_code.clone()));
        let provider = MockEthProvider::default();
        for (address, account) in &db.cache.accounts {
            let code = db
                .cache
                .contracts
                .get(&account.info.code_hash)
                .map(|code| code.original_bytes())
                .unwrap_or_default();
            provider.add_account(
                *address,
                ExtendedAccount::new(account.info.nonce, account.info.balance)
                    .with_bytecode(code)
                    .extend_storage(
                        account
                            .storage
                            .iter()
                            .map(|(key, value)| (B256::from(key.to_be_bytes::<32>()), *value)),
                    ),
            );
        }
        let mut config = test_payload_config();
        config.attributes.no_tx_pool = no_tx_pool;
        let tx = test_cip64_pool_tx(sender, sig).into_consensus().into_inner();
        config.attributes.transactions = vec![WithEncoded::new(tx.encoded_2718().into(), tx)];
        let chain_spec = test_chain_spec();
        let ctx = OpPayloadBuilderCtx {
            evm_config: CeloEvmConfig::celo_with_blocklist(chain_spec.clone(), blocklist.clone()),
            builder_config: OpBuilderConfig::default(),
            chain_spec,
            config,
            cancel: Default::default(),
            best_payload: None,
        };
        let witness = metrics::with_local_recorder(&recorder, || {
            OpBuilder::new(|_| reth_payload_util::NoopPayloadTransactions::<CeloPoolTx>::default())
                .witness(&provider, &provider, &ctx)
                .expect("witness execution")
        });
        assert!(
            witness.codes.contains(&token_code.original_bytes()),
            "the token must execute during witness generation"
        );
        assert!(
            !blocklist.is_blocked(TEST_FC),
            "witness execution must not mutate sequencing policy"
        );
        assert!(snapshotter.snapshot().into_vec().iter().all(|(key, _, _, _)| {
            !matches!(
                key.key().name(),
                "celo_payload_fee_currency_failures_total" | "celo_payload_skipped_total"
            )
        }));
    }
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

/// Sender of the underpriced transaction. It is dropped before execution, so it is never
/// funded and its signature never has to recover to this address.
const UNDERPRICED_SENDER: Address = address!("2222222222222222222222222222222222222222");

/// Native EIP-1559 tx with a 20 Gwei fee cap, below the block's 25 Gwei base fee, so
/// `effective_tip_per_gas` returns `None` for it.
fn underpriced_native_pool_tx(sig: Signature) -> CeloPoolTx {
    let eip1559 = TxEip1559 {
        chain_id: 42220,
        nonce: 0,
        gas_limit: 21_000,
        max_fee_per_gas: 20_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        to: TxKind::Call(Address::ZERO),
        value: U256::ZERO,
        access_list: Default::default(),
        input: Default::default(),
    };
    let envelope = CeloTxEnvelope::Eip1559(Signed::new_unhashed(eip1559, sig));
    let pooled = CeloPooledTransaction::try_from(envelope).unwrap();
    CeloPoolTx::new(OpPoolPoolTx::<CeloTxEnvelope, CeloPooledTransaction>::from_pooled(
        Recovered::new_unchecked(pooled, UNDERPRICED_SENDER),
    ))
}

/// Guard for celo-org/celo-blockchain-planning#1463.
///
/// `execute_best_transactions` unwraps the miner tip of every transaction the iterator yields,
/// before executing it, so one priced below the base fee aborts the payload task. This drives
/// the real builder over the real `CeloPayloadTransactions` filter to prove the two agree on
/// which transactions are underpriced: without the filter's guard the `expect` inside the
/// builder panics and this test fails.
#[test]
fn payload_filter_drops_an_underpriced_tx_before_the_builder_unwraps_its_tip() {
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

    // The builder's own attributes, so the filter reads exactly the base fee the builder
    // compares against a few lines into `execute_best_transactions`.
    let attr = ctx.best_transaction_attributes(builder.evm_mut().block());

    let underpriced = underpriced_native_pool_tx(sig);
    assert!(
        underpriced.max_fee_per_gas() < u128::from(attr.basefee),
        "the tx must be underpriced at this block's base fee. Otherwise the builder drops it \
         down its own LackOfFundForMaxFee path — the sender is deliberately unfunded — and the \
         test would pass with the guard removed"
    );

    let invalid = Arc::new(Mutex::new(Vec::new()));
    let best_txs =
        CeloPayloadTransactions::new(FeeCurrencyLimits::default(), FeeCurrencyBlocklist::default())
            .filter_pool_transactions(
                QueuedTxs {
                    txs: vec![underpriced, test_cip64_pool_tx(sender, sig)],
                    invalid: invalid.clone(),
                },
                30_000_000,
                attr,
            );

    let mut info = ExecutionInfo::new();
    let mut committed = Vec::new();
    // Wrapped as op-reth's build path wraps the pool iterator before executing it.
    ctx.execute_best_transactions(
        &mut info,
        &mut builder,
        RethPayloadTransactions(best_txs),
        None,
        Some(&mut committed),
    )
    .expect("execute_best_transactions");

    assert_eq!(
        *invalid.lock().unwrap(),
        vec![(UNDERPRICED_SENDER, 0)],
        "the underpriced sender must be marked invalid, and nobody else"
    );
    assert_eq!(committed.len(), 1, "only the well-priced CIP-64 tx may be executed");

    // Same tip as `cip64_payload_builder_handles_low_fc_max_fee`: the dropped tx contributes
    // nothing, so the block's fees are still exactly the CIP-64 tx's native-equivalent tip.
    const EXPECTED_TIP_PER_GAS: u128 = 10_000_000_000; // 10 Gwei (native)
    assert!(info.cumulative_gas_used > 0, "the CIP-64 tx must have executed");
    assert_eq!(
        info.total_fees,
        U256::from(EXPECTED_TIP_PER_GAS) * U256::from(info.cumulative_gas_used),
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
    type Attributes = OpPayloadAttrs;
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
            test_attempt_config(None),
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

#[test]
fn cgt_v2_witness_rejects_user_transactions_on_either_side_of_signal() {
    use alloy_consensus::{Sealable, SignableTransaction, TxLegacy};
    use alloy_eips::Encodable2718;
    use celo_alloy_consensus::cgt_v2_group::{
        CGT_V2_DEPOSITOR, CGT_V2_L1_BLOCK, CgtV2UpgradeGroup,
    };
    use reth_optimism_chainspec::OpChainSpecBuilder;
    use reth_optimism_payload_builder::builder::OpBuilder;
    use reth_primitives_traits::WithEncoded;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};

    let mut info_data = alloy_primitives::keccak256(
        b"setL1BlockValues(uint64,uint64,uint256,bytes32,uint64,bytes32,uint256,uint256)",
    )[..4]
        .to_vec();
    info_data.extend_from_slice(&[0; 256]);
    let signed = TxLegacy {
        chain_id: Some(1337),
        gas_limit: 21_000,
        gas_price: 30_000_000_000,
        to: TxKind::Call(Address::with_last_byte(0x88)),
        ..Default::default()
    }
    .into_signed(Signature::test_signature());
    let sender = signed.recover_signer().unwrap();
    let user = CeloTxEnvelope::Legacy(signed);
    let info: CeloTxEnvelope = op_alloy_consensus::TxDeposit {
        from: CGT_V2_DEPOSITOR,
        to: TxKind::Call(CGT_V2_L1_BLOCK),
        gas_limit: 200_000,
        source_hash: B256::repeat_byte(0x22),
        input: info_data.into(),
        ..Default::default()
    }
    .seal_slow()
    .into();
    let signal: CeloTxEnvelope =
        CgtV2UpgradeGroup::deposits(1337, U256::from(600)).unwrap().remove(0).seal_slow().into();
    let provider = MockEthProvider::default();
    provider.add_account(sender, ExtendedAccount::new(0, U256::MAX));
    let prestate: serde_json::Value = serde_json::from_str(include_str!(
        "../../celo-alloy/consensus/testdata/cgt_v2/cgtv2-fixture-prestate.json"
    ))
    .unwrap();
    for (address, account) in prestate.as_object().unwrap() {
        provider.add_account(
            address.parse().unwrap(),
            ExtendedAccount::new(0, account["balance"].as_str().unwrap().parse().unwrap())
                .with_bytecode(account["code"].as_str().unwrap().parse().unwrap())
                .extend_storage(account["storage"].as_object().unwrap().iter().map(
                    |(slot, value)| {
                        (
                            slot.parse::<B256>().unwrap(),
                            value.as_str().unwrap().parse::<U256>().unwrap(),
                        )
                    },
                )),
        );
    }
    let chain_spec = Arc::new(
        OpChainSpecBuilder::default()
            .chain(reth_chainspec::Chain::from_id(1337))
            .genesis(Default::default())
            .granite_activated()
            .build(),
    );
    let witness = |transactions: Vec<CeloTxEnvelope>| {
        let mut config = test_payload_config();
        config.attributes.gas_limit = Some(70_000_000);
        config.attributes.transactions = transactions
            .into_iter()
            .map(|tx| WithEncoded::new(tx.encoded_2718().into(), tx))
            .collect();
        let ctx = OpPayloadBuilderCtx {
            evm_config: CeloEvmConfig::celo(chain_spec.clone()),
            builder_config: OpBuilderConfig::default(),
            chain_spec: chain_spec.clone(),
            config,
            cancel: Default::default(),
            best_payload: None,
        };
        OpBuilder::new(|_| reth_payload_util::NoopPayloadTransactions::<CeloPoolTx>::default())
            .witness(&provider, &provider, &ctx)
    };
    let mut complete = vec![info.clone()];
    complete.extend(
        CgtV2UpgradeGroup::deposits(1337, U256::from(600))
            .unwrap()
            .into_iter()
            .map(|tx| CeloTxEnvelope::Deposit(tx.seal_slow())),
    );
    witness(complete.clone()).expect("complete deposit-only activation witness");
    assert!(witness(vec![info, signal]).is_err(), "truncated activation witness must fail");
    let mut user_before = complete.clone();
    user_before.insert(1, user.clone());
    let mut user_after = complete;
    user_after.push(user);
    for transactions in [user_before, user_after] {
        let error = witness(transactions).expect_err("activation witness must reject user txs");
        assert!(error.to_string().contains("unexpected non-deposit transactions"), "{error}");
    }
}
