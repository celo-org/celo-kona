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
    pool::{CeloPoolTx, ExchangeRate},
};
use reth_basic_payload_builder::PayloadConfig;
use reth_chainspec::Chain;
use reth_evm::execute::BlockBuilder;
use reth_optimism_chainspec::{OpChainSpec, OpChainSpecBuilder};
use reth_optimism_payload_builder::{
    OpPayloadBuilderAttributes,
    builder::{ExecutionInfo, OpPayloadBuilderCtx},
    config::OpBuilderConfig,
};
use reth_optimism_txpool::OpPooledTransaction as OpPoolPoolTx;
use reth_payload_util::PayloadTransactions;
use reth_primitives_traits::{Recovered, SealedHeader};
use reth_transaction_pool::PoolTransaction;
use revm::database::State;
use std::sync::Arc;

use common::{TEST_FC, make_celo_test_db};

/// `PayloadTransactions` impl yielding a single `CeloPoolTx`.
struct OneTx(Option<CeloPoolTx>);
impl PayloadTransactions for OneTx {
    type Transaction = CeloPoolTx;
    fn next(&mut self, _ctx: ()) -> Option<Self::Transaction> {
        self.0.take()
    }
    fn mark_invalid(&mut self, _sender: Address, _nonce: u64) {}
}

#[test]
fn cip64_payload_builder_handles_low_fc_max_fee() {
    // Sender with deterministic test signature.
    let sig = Signature::test_signature();
    // The test_signature recovers to a specific address; derive it from a dummy tx.
    let sender = {
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
        let signed = Signed::new_unhashed(dummy, sig);
        signed.recover_signer().unwrap()
    };

    // ── State: FC infrastructure + funded sender.
    let inner_db = make_celo_test_db(sender, U256::from(1_000_000_000_000_000_000u128));
    let mut state = State::builder().with_database(inner_db).with_bundle_update().build();

    // ── Chain spec: Granite-active (pre-Holocene, no extra_data ceremony).
    let chain_spec: Arc<OpChainSpec> = Arc::new(
        OpChainSpecBuilder::default()
            .chain(Chain::from_id(42220))
            .genesis(Default::default())
            .granite_activated()
            .build(),
    );

    // ── Parent: 25 Gwei base fee, 50% utilization (next block also 25 Gwei).
    //    Cancun-active fields (excess_blob_gas, parent_beacon_block_root) are populated
    //    so EIP-4788 pre-execution doesn't reject the build.
    let parent_header = Header {
        base_fee_per_gas: Some(25_000_000_000),
        gas_limit: 30_000_000,
        gas_used: 15_000_000,
        timestamp: 0,
        number: 0,
        excess_blob_gas: Some(0),
        blob_gas_used: Some(0),
        parent_beacon_block_root: Some(B256::ZERO),
        ..Default::default()
    };
    let parent = SealedHeader::seal_slow(parent_header);

    // ── Builder attributes (mostly default; timestamp must exceed parent's).
    let attributes = OpPayloadBuilderAttributes::<CeloTxEnvelope> {
        timestamp: 1,
        suggested_fee_recipient: Address::from([0xfe; 20]),
        parent: parent.hash(),
        parent_beacon_block_root: Some(B256::ZERO),
        gas_limit: Some(30_000_000),
        ..Default::default()
    };

    let config = PayloadConfig {
        parent_header: Arc::new(parent),
        attributes,
        payload_id: Default::default(),
    };

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
        config,
        cancel: Default::default(),
        best_payload: None,
    };

    // ── CIP-64 tx with FC max_fee = 10 Gwei (well below native 25 Gwei base fee, but
    //    above FC base fee of 25 Gwei * 1/10 = 2.5 Gwei so FC validation passes).
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

    let best_txs = OneTx(Some(pool_tx));

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
