//! Unit coverage for the Celo half of the reth fork's block-replay context hook.
//!
//! Since celo-org/celo-kona#248 the RPC replay paths run against a *forked* reth
//! (`celo-org/reth`) that captures a per-block replay context before replaying a
//! block prefix and seeds it into every call EVM it builds over mid-block state
//! (`debug_traceCall` with a `txIndex`, `debug_traceCallMany`, `eth_callMany`).
//! The fork tests its own side of that hook with an Ethereum `ConfigureEvm`; the
//! Celo implementations — `CeloEvmConfig::capture_block_replay_ctx`,
//! `CeloEvmConfig::seed_block_replay_ctx` and the `CeloEvm::set_fee_currency_context`
//! they delegate to — had none, so a regression there would be invisible to both
//! upstream CI and ours.
//!
//! What the hook is for: fee-currency exchange rates are block-scoped consensus
//! state. Every transaction of a block settles at the rates read from block-start
//! state, so a call simulated at a mid-block position must price against those
//! rates and not against whatever the oracle holds after the prefix has been
//! replayed. The tests below drive a real `FeeCurrencyDirectory` / MockOracle in
//! state, move the rate the way a mid-block oracle update would, and read the
//! effective gas price back out of the EVM through a `GASPRICE` probe contract.
//!
//! The four properties pinned here are the ones the hook's correctness rests on:
//! capture reads *live* block-start state, seeding overrides the lazy per-block
//! load, a foreign context is rejected without disturbing the one already seeded,
//! and a seeded context stops applying the moment the EVM's block environment no
//! longer matches the block it was captured for.

mod common;

use alloy_consensus::Header;
use alloy_evm::Evm;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use celo_alloy_consensus::CeloTxType;
use celo_reth::CeloEvmConfig;
use celo_revm::{CeloTransaction, FeeCurrencyContext};
use op_revm::OpTransaction;
use reth_chainspec::Chain;
use reth_evm::{ConfigureEvm, Database, EvmFor};
use reth_optimism_chainspec::{OpChainSpec, OpChainSpecBuilder};
use revm::{
    context::TxEnv,
    database::InMemoryDB,
    state::{AccountInfo, Bytecode},
};
use std::{any::Any, sync::Arc};

use common::{TEST_FC, make_celo_test_db, set_oracle_rate};

/// Sender of every simulated call; funded with native CELO and FC by the fixture.
const SENDER: Address = Address::with_last_byte(0x01);

/// Callee returning `GASPRICE` as a single ABI word:
/// `GASPRICE; PUSH0; MSTORE; PUSH1 0x20; PUSH0; RETURN`.
///
/// Deliberately not a low address — those are shadowed by precompiles.
const GAS_PRICE_PROBE: Address = Address::with_last_byte(0xC0);

/// Native base fee of every block below: Celo's 25 Gwei floor.
const BASE_FEE: u64 = 25_000_000_000;

/// Block-start oracle rate, as in [`make_celo_test_db`]: 1 FC per 10 CELO.
const BLOCK_START_RATE: (u64, u64) = (1, 10);
/// Rate the oracle is moved to mid-block: 3 FC per 10 CELO.
const MID_BLOCK_RATE: (u64, u64) = (3, 10);

/// `BASE_FEE` converted at [`BLOCK_START_RATE`] — 2.5 Gwei.
const BASE_FEE_AT_BLOCK_START_RATE: u64 = BASE_FEE * BLOCK_START_RATE.0 / BLOCK_START_RATE.1;
/// `BASE_FEE` converted at [`MID_BLOCK_RATE`] — 7.5 Gwei.
const BASE_FEE_AT_MID_BLOCK_RATE: u64 = BASE_FEE * MID_BLOCK_RATE.0 / MID_BLOCK_RATE.1;

/// Block the replay context is captured for.
const CAPTURED_BLOCK: u64 = 7;

fn chain_spec() -> Arc<OpChainSpec> {
    Arc::new(
        OpChainSpecBuilder::default()
            .chain(Chain::from_id(42220))
            .genesis(Default::default())
            .granite_activated()
            .build(),
    )
}

/// Header of the block a call is simulated against. Cancun fields are populated
/// so the EVM env is well-formed for the Granite spec.
fn header(number: u64) -> Header {
    Header {
        number,
        base_fee_per_gas: Some(BASE_FEE),
        gas_limit: 30_000_000,
        timestamp: number,
        excess_blob_gas: Some(0),
        blob_gas_used: Some(0),
        parent_beacon_block_root: Some(B256::ZERO),
        ..Default::default()
    }
}

/// Fixture state: fee-currency infrastructure at [`BLOCK_START_RATE`] plus the
/// `GASPRICE` probe.
fn test_db() -> InMemoryDB {
    let mut db = make_celo_test_db(SENDER, U256::from(1_000_000_000_000_000_000u128));
    let code = Bytecode::new_raw(Bytes::from_static(&[0x3a, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3]));
    db.insert_account_info(
        GAS_PRICE_PROBE,
        AccountInfo { code_hash: code.hash_slow(), code: Some(code), ..Default::default() },
    );
    db
}

/// A CIP-64 call to the `GASPRICE` probe, paying in [`TEST_FC`].
///
/// The priority fee is zero and the fee cap is far above either candidate base
/// fee, so the observed `GASPRICE` is exactly the fee-currency-denominated base
/// fee — i.e. the exchange rate the EVM priced with, isolated from every other
/// input.
fn cip64_probe_call() -> CeloTransaction<TxEnv> {
    CeloTransaction {
        op_tx: OpTransaction {
            base: TxEnv {
                caller: SENDER,
                kind: TxKind::Call(GAS_PRICE_PROBE),
                nonce: 0,
                // Covers the 21k base plus the currency's 50k extra intrinsic gas.
                gas_limit: 200_000,
                value: U256::ZERO,
                data: Bytes::new(),
                gas_price: 100 * u128::from(BASE_FEE),
                chain_id: Some(42220),
                gas_priority_fee: Some(0),
                access_list: Default::default(),
                blob_hashes: Vec::new(),
                max_fee_per_blob_gas: 0,
                tx_type: CeloTxType::Cip64 as u8,
                authorization_list: Default::default(),
            },
            enveloped_tx: Some(Bytes::default()),
            deposit: Default::default(),
        },
        fee_currency: Some(TEST_FC),
        cip64_tx_info: None,
        effective_gas_price: None,
    }
}

/// Simulate [`cip64_probe_call`] on `evm` and return the `GASPRICE` it observed.
///
/// Disabling the base-fee check is what makes this the `eth_call` /
/// `debug_traceCall` shape rather than the block-execution one: the ERC20 debit
/// is skipped, and the handler denominates `effective_gas_price` from the
/// fee-currency context alone.
fn observed_gas_price<DB: Database>(evm: &mut EvmFor<CeloEvmConfig, DB>) -> U256 {
    evm.ctx_mut().cfg.disable_base_fee = true;
    let outcome = evm.transact_raw(cip64_probe_call()).expect("CIP-64 probe call must not error");
    let output = outcome.result.output().expect("probe returns GASPRICE");
    U256::from_be_slice(output)
}

/// The captured context must be read out of *live* block-start state — the whole
/// point of capturing is that the rate it carries is the one the block settles
/// at. Pins the full `FeeCurrencyInfo` (rate and intrinsic gas), plus the
/// `updated_at_block` stamp that later decides whether a seed still applies.
#[test]
fn capture_reads_the_fee_currency_directory_from_block_start_state() {
    let mut db = test_db();
    let evm_config = CeloEvmConfig::celo(chain_spec());
    let evm_env = evm_config.evm_env(&header(CAPTURED_BLOCK)).expect("evm env");

    let captured = evm_config
        .capture_block_replay_ctx(&mut db, &evm_env)
        .expect("Celo must capture a replay context");
    let ctx = captured
        .downcast_ref::<FeeCurrencyContext>()
        .expect("the captured context is a FeeCurrencyContext");

    assert_eq!(
        ctx.currency_exchange_rate(Some(TEST_FC)).expect("TEST_FC is registered"),
        (U256::from(BLOCK_START_RATE.0), U256::from(BLOCK_START_RATE.1)),
        "capture must carry the directory's exchange rate"
    );
    assert_eq!(
        ctx.currency_intrinsic_gas_cost(Some(TEST_FC)).expect("TEST_FC is registered"),
        50_000,
        "capture must carry the currency's intrinsic gas, not just its rate"
    );
    assert_eq!(
        ctx.updated_at_block,
        Some(U256::from(CAPTURED_BLOCK)),
        "the stamp decides whether a seeded context still applies, so it must be the \
         captured block"
    );

    // Sensitivity: the assertions above would also hold for a context read once and
    // cached somewhere. Move the oracle and re-capture — the rate must follow.
    set_oracle_rate(&mut db, MID_BLOCK_RATE.0, MID_BLOCK_RATE.1);
    let recaptured = evm_config
        .capture_block_replay_ctx(&mut db, &evm_env)
        .expect("Celo must capture a replay context");
    assert_eq!(
        recaptured
            .downcast_ref::<FeeCurrencyContext>()
            .expect("the captured context is a FeeCurrencyContext")
            .currency_exchange_rate(Some(TEST_FC))
            .expect("TEST_FC is registered"),
        (U256::from(MID_BLOCK_RATE.0), U256::from(MID_BLOCK_RATE.1)),
        "capture must read state at call time, not a cached rate"
    );
}

/// The regression the hook exists to prevent: a call simulated at a mid-block
/// position must price against the *block-start* rate. Without seeding, the
/// handler's lazy per-block load reads the oracle as the replayed prefix left it
/// and the call prices at the wrong rate — a successful response with wrong
/// numbers in it, which is exactly the failure mode this whole surface keeps
/// producing.
#[test]
fn seeded_context_prices_a_cip64_call_at_the_captured_rate() {
    let mut db = test_db();
    let evm_config = CeloEvmConfig::celo(chain_spec());
    let evm_env = evm_config.evm_env(&header(CAPTURED_BLOCK)).expect("evm env");

    let captured = evm_config
        .capture_block_replay_ctx(&mut db, &evm_env)
        .expect("Celo must capture a replay context");

    // A transaction in the replayed prefix updates the oracle.
    set_oracle_rate(&mut db, MID_BLOCK_RATE.0, MID_BLOCK_RATE.1);

    let unseeded = {
        let mut evm = evm_config.evm_with_env(&mut db, evm_env.clone());
        observed_gas_price(&mut evm)
    };
    assert_eq!(
        unseeded,
        U256::from(BASE_FEE_AT_MID_BLOCK_RATE),
        "without a seed the EVM lazily loads the rate from mid-block state — this is the \
         behaviour the hook overrides, asserted so the test below proves an override rather \
         than a coincidence"
    );

    let seeded = {
        let mut evm = evm_config.evm_with_env(&mut db, evm_env);
        evm_config.seed_block_replay_ctx(&mut evm, &*captured);
        observed_gas_price(&mut evm)
    };
    assert_eq!(
        seeded,
        U256::from(BASE_FEE_AT_BLOCK_START_RATE),
        "a seeded call must price at the captured block-start rate, not at the rate the \
         replayed prefix left in state"
    );
}

/// The seed is handed over as `&dyn Any`, so `CeloEvmConfig` must tolerate a
/// context captured by some other `ConfigureEvm` — the downcast is the only
/// thing standing between a foreign payload and a panic inside an RPC handler.
/// A wrong-typed seed must be a no-op: no panic, and no clearing of the context
/// already in place (which would make every currency read as unregistered).
///
/// The foreign seed lands on an EVM that already holds the captured context,
/// because that is what discriminates the two failure modes. On a *fresh* EVM a
/// clobber to an empty context is indistinguishable from a no-op: an empty
/// context carries no `updated_at_block`, so the handler reloads it from state
/// either way and both price at the mid-block rate.
#[test]
fn seeding_a_foreign_context_is_a_no_op() {
    let mut db = test_db();
    let evm_config = CeloEvmConfig::celo(chain_spec());
    let evm_env = evm_config.evm_env(&header(CAPTURED_BLOCK)).expect("evm env");

    let captured = evm_config
        .capture_block_replay_ctx(&mut db, &evm_env)
        .expect("Celo must capture a replay context");
    set_oracle_rate(&mut db, MID_BLOCK_RATE.0, MID_BLOCK_RATE.1);

    let foreign: Box<dyn Any + Send> = Box::new(String::from("some other node's replay context"));
    let mut evm = evm_config.evm_with_env(&mut db, evm_env);
    evm_config.seed_block_replay_ctx(&mut evm, &*captured);
    evm_config.seed_block_replay_ctx(&mut evm, &*foreign);

    assert_eq!(
        observed_gas_price(&mut evm),
        U256::from(BASE_FEE_AT_BLOCK_START_RATE),
        "a context of the wrong concrete type must leave the EVM's context exactly as it found \
         it — neither replaced nor cleared"
    );
}

/// The `updated_at_block` stamp is what bounds a seeded context to its own
/// block: the debug API re-captures per simulated block (e.g. per
/// `debug_traceCallMany` bundle), so a stale context reaching an EVM configured
/// for a later block must be reloaded rather than silently applied. Without that
/// bound, seeding would be a way to price a call at another block's rates.
#[test]
fn seeded_context_expires_when_the_block_environment_moves_on() {
    let mut db = test_db();
    let evm_config = CeloEvmConfig::celo(chain_spec());

    let captured_env = evm_config.evm_env(&header(CAPTURED_BLOCK)).expect("evm env");
    let captured = evm_config
        .capture_block_replay_ctx(&mut db, &captured_env)
        .expect("Celo must capture a replay context");

    set_oracle_rate(&mut db, MID_BLOCK_RATE.0, MID_BLOCK_RATE.1);

    // Same context, but an EVM configured for the next block.
    let next_env = evm_config.evm_env(&header(CAPTURED_BLOCK + 1)).expect("evm env");
    let mut evm = evm_config.evm_with_env(&mut db, next_env);
    evm_config.seed_block_replay_ctx(&mut evm, &*captured);

    assert_eq!(
        observed_gas_price(&mut evm),
        U256::from(BASE_FEE_AT_MID_BLOCK_RATE),
        "a context captured for an earlier block must be reloaded, not applied"
    );
}
