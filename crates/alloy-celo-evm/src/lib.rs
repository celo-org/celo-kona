#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::{borrow::Cow, format};
use alloy_evm::{
    Database, Evm, EvmEnv, EvmFactory,
    precompiles::{DynPrecompile, PrecompilesMap},
};
use alloy_op_evm::{
    OpTxError, map_op_err,
    post_exec::{PostExecEvm, PostExecExecutedTx, PostExecTxContext},
};
use alloy_primitives::{Address, Bytes, U256};
use celo_revm::{
    CeloBuilder, CeloContext, CeloPrecompiles, CeloTransaction, DefaultCelo, constants,
    constants::{FEE_CREDIT_ERROR_PREFIX, FEE_DEBIT_ERROR_PREFIX},
    precompiles::transfer::{TRANSFER_ADDRESS, TRANSFER_GAS_COST},
};
use core::{
    fmt::Debug,
    ops::{Deref, DerefMut},
};
use op_revm::{L1BlockInfo, OpHaltReason, OpSpecId, precompiles::OpPrecompiles};
use revm::{
    Context, ExecuteEvm, InspectEvm, Inspector, SystemCallEvm,
    context::{BlockEnv, TxEnv},
    context_interface::{
        Cfg,
        result::{EVMError, ResultAndState},
    },
    handler::PrecompileProvider,
    inspector::NoOpInspector,
    interpreter::InterpreterResult,
    precompile::{PrecompileHalt, PrecompileOutput},
};

pub mod block;
pub mod blocklist;
pub mod cip64_storage;

use blocklist::FeeCurrencyBlocklist;
use cip64_storage::Cip64Storage;

/// Creates a default [`L1BlockInfo`] with zeroed operator fee fields for specs that require
/// them. Without this, `eth_call` panics on Isthmus+ because
/// `operator_fee_scalar`/`operator_fee_constant` are `None`.
fn default_l1_block_info(spec_id: OpSpecId) -> L1BlockInfo {
    let mut info = L1BlockInfo::default();
    if spec_id.is_enabled_in(OpSpecId::ISTHMUS) {
        info.operator_fee_scalar = Some(U256::ZERO);
        info.operator_fee_constant = Some(U256::ZERO);
    }
    info
}

/// Creates a [`PrecompilesMap`] containing the standard OP Stack precompiles plus the Celo
/// transfer precompile for the given spec.
pub fn celo_precompiles_map(spec_id: OpSpecId) -> PrecompilesMap {
    let mut map = PrecompilesMap::from_static(OpPrecompiles::new_with_spec(spec_id).precompiles());
    map.extend_precompiles([(TRANSFER_ADDRESS, make_transfer_precompile(spec_id))]);
    map
}

/// Creates the Celo transfer [`DynPrecompile`] for the given spec.
fn make_transfer_precompile(spec_id: OpSpecId) -> DynPrecompile {
    const fn coerce<
        F: Fn(alloy_evm::precompiles::PrecompileInput<'_>) -> revm::precompile::PrecompileResult
            + Send
            + Sync
            + 'static,
    >(
        f: F,
    ) -> F {
        f
    }
    DynPrecompile::from(coerce(move |input| transfer_precompile(spec_id, input))).stateful()
}

/// Transfer precompile implementation for use as a [`DynPrecompile`].
///
/// This duplicates the logic in `celo_revm::precompiles::transfer::transfer_run` because the two
/// dispatch models are incompatible: `celo-revm`'s version operates on a full `ContextTr` (used
/// by the handler-based precompile pipeline), while this version targets `alloy-evm`'s stateless
/// `DynPrecompile` interface (balance changes go through `PrecompileInput::internals`). Both
/// implementations must be kept in sync.
fn transfer_precompile(
    spec_id: OpSpecId,
    mut input: alloy_evm::precompiles::PrecompileInput<'_>,
) -> revm::precompile::PrecompileResult {
    if input.is_static {
        return Ok(PrecompileOutput::halt(
            PrecompileHalt::Other(Cow::Borrowed(
                "transfer precompile cannot be called in static context",
            )),
            0,
        ));
    }

    if input.gas < TRANSFER_GAS_COST {
        return Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, 0));
    }

    let chain_id = input.internals.chain_id();
    if input.caller != constants::get_addresses(chain_id).celo_token {
        return Ok(PrecompileOutput::halt(
            PrecompileHalt::Other(Cow::Borrowed("invalid caller for transfer precompile")),
            0,
        ));
    }

    if input.data.len() != 96 {
        return Ok(PrecompileOutput::halt(
            PrecompileHalt::Other(Cow::Borrowed("invalid input length")),
            0,
        ));
    }

    let from = Address::from_slice(&input.data[12..32]);
    let to = Address::from_slice(&input.data[44..64]);
    let value = U256::from_be_slice(&input.data[64..96]);

    let revert_cold_status = !spec_id.is_enabled_in(OpSpecId::JOVIAN);
    let revert_from_cold =
        revert_cold_status && input.internals.load_account(from).map(|a| a.is_cold).unwrap_or(true);
    let revert_to_cold =
        revert_cold_status && input.internals.load_account(to).map(|a| a.is_cold).unwrap_or(true);

    let result = input.internals.transfer(from, to, value);

    if revert_from_cold && let Ok(mut account) = input.internals.load_account_mut(from) {
        account.data.unsafe_mark_cold();
    }
    if revert_to_cold && let Ok(mut account) = input.internals.load_account_mut(to) {
        account.data.unsafe_mark_cold();
    }

    match result {
        Ok(None) => Ok(PrecompileOutput::new(TRANSFER_GAS_COST, Bytes::new(), 0)),
        Ok(Some(transfer_err)) => Ok(PrecompileOutput::halt(
            PrecompileHalt::Other(Cow::Owned(format!("transfer error occurred: {transfer_err:?}"))),
            0,
        )),
        Err(db_err) => Ok(PrecompileOutput::halt(
            PrecompileHalt::Other(Cow::Owned(format!("database error occurred: {db_err:?}"))),
            0,
        )),
    }
}

/// Celo EVM implementation.
///
/// This is a wrapper type around the `revm` evm with optional [`Inspector`] (tracing)
/// support. [`Inspector`] support is configurable at runtime because it's part of the underlying
/// [`CeloEvm`](celo_revm::CeloEvm) type.
#[allow(missing_debug_implementations)] // missing celo_revm::CeloContext Debug impl
pub struct CeloEvm<DB: Database, I, P = CeloPrecompiles> {
    inner: celo_revm::CeloEvm<DB, I, P>,
    inspect: bool,
    cip64_storage: Cip64Storage,
    blocklist: FeeCurrencyBlocklist,
    /// Block timestamp of the last eviction call, used to avoid redundant eviction
    /// on every `transact_raw` call within the same block.
    last_evicted_timestamp: u64,
}

impl<DB: Database, I, P> CeloEvm<DB, I, P> {
    /// Provides a reference to the EVM context.
    pub const fn ctx(&self) -> &CeloContext<DB> {
        &self.inner.inner.0.ctx
    }

    /// Provides a mutable reference to the EVM context.
    pub const fn ctx_mut(&mut self) -> &mut CeloContext<DB> {
        &mut self.inner.inner.0.ctx
    }

    /// Provides a mutable reference to the EVM inspector.
    pub const fn inspector_mut(&mut self) -> &mut I {
        &mut self.inner.inner.0.inspector
    }

    /// Creates a FeeCurrencyContext from the current EVM state.
    pub fn create_fee_currency_context(&mut self) -> celo_revm::FeeCurrencyContext
    where
        I: Inspector<CeloContext<DB>>,
        P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
    {
        celo_revm::FeeCurrencyContext::new_from_evm(&mut self.inner)
    }

    /// Provides a reference to the CIP-64 storage.
    pub const fn cip64_storage(&self) -> &Cip64Storage {
        &self.cip64_storage
    }
}

impl<DB: Database, I, P> CeloEvm<DB, I, P> {
    /// Creates a new Celo EVM instance.
    ///
    /// The `inspect` argument determines whether the configured [`Inspector`] of the given
    /// [`CeloEvm`](celo_revm::CeloEvm) should be invoked on [`Evm::transact`].
    pub fn new(evm: celo_revm::CeloEvm<DB, I, P>, inspect: bool) -> Self {
        Self {
            inner: evm,
            inspect,
            cip64_storage: Cip64Storage::default(),
            blocklist: FeeCurrencyBlocklist::default(),
            last_evicted_timestamp: 0,
        }
    }
}

impl<DB: Database, I, P> Deref for CeloEvm<DB, I, P> {
    type Target = CeloContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I, P> DerefMut for CeloEvm<DB, I, P> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, I, P> Evm for CeloEvm<DB, I, P>
where
    DB: Database,
    I: Inspector<CeloContext<DB>>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
{
    type DB = DB;
    type Tx = CeloTransaction<TxEnv>;
    type Error = EVMError<DB::Error, OpTxError>;
    type HaltReason = OpHaltReason;
    type Spec = OpSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = P;
    type Inspector = I;

    fn block(&self) -> &BlockEnv {
        &self.block
    }

    fn cfg_env(&self) -> &revm::context::CfgEnv<OpSpecId> {
        &self.cfg
    }

    fn chain_id(&self) -> u64 {
        self.cfg.chain_id
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        // Capture fee_currency before execution (it's consumed by transact)
        let fee_currency = tx.fee_currency;

        // The base-fee check is enabled during real state transitions — both sequencing AND
        // block import / derivation re-execution — and disabled during RPC simulation (eth_call,
        // eth_estimateGas, tracing). We use it to gate blocklist *bookkeeping* (eviction and
        // population), never rejection: since it cannot distinguish sequencing from import,
        // rejecting on it would reject valid canonical blocks during import (see NOTE below).
        let base_fee_check_enabled = !self.ctx().cfg.is_base_fee_check_disabled();

        // Evict stale blocklist entries using the current block timestamp,
        // but only once per block to avoid redundant work on every transaction.
        if base_fee_check_enabled {
            let block_timestamp: u64 = self.ctx().block.timestamp.to();
            if block_timestamp != self.last_evicted_timestamp {
                self.blocklist.evict(block_timestamp);
                self.last_evicted_timestamp = block_timestamp;
            }
        }

        // NOTE: blocklist *rejection* is intentionally NOT performed here. `base_fee_check_enabled`
        // is true during both sequencing AND block import / derivation re-execution, so rejecting
        // here would let a node's locally-accumulated blocklist reject a valid canonical block
        // built by another sequencer — a cross-node disagreement on block validity. Rejection is a
        // sequencing-only policy and is enforced upstream in `CeloFeeCurrencyFilter` (see
        // `celo-reth`'s `payload.rs`), which derivation deliberately bypasses. Here we only
        // *populate* the blocklist on debit/credit failures below. That population can also fire
        // during import, but it is moot there: a debit/credit failure is an `InvalidTransaction`
        // error (not a revert), so it aborts the entire block import — the node already cannot
        // follow the chain, and the resulting blocklist entry never affects a valid block.

        let result = if self.inspect { self.inner.inspect_tx(tx) } else { self.inner.transact(tx) }
            .map_err(map_op_err);

        match &result {
            Ok(_) => {
                // CIP64 NOTE:
                // Extract and store the cip64 info to a shared storage to be able to add the
                // credit/debit logs when building the receipt in the receipts_builder.
                // Only store during real execution (sequencing and import), not RPC
                // simulation/tracing: storage is per-EVM (so cross-EVM pollution is
                // impossible), but multi-tx RPC tracing (`debug_traceBlock`,
                // `debug_traceTransaction`) re-executes multiple txs through one EVM
                // without calling `build_receipt`. Skipping the store here keeps the
                // slot-occupied panic in `store_cip64_info` a signal of a real executor
                // bug instead of firing on legitimate RPC paths.
                let cip64_info = self.inner.inner.0.ctx.tx.cip64_tx_info.take();
                if base_fee_check_enabled && let Some(cip64_info) = cip64_info {
                    self.cip64_storage.store_cip64_info(fee_currency, cip64_info);
                }
            }
            Err(e) if base_fee_check_enabled && fee_currency.is_some() => {
                // Only blocklist when the error is a fee-currency debit/credit failure,
                // not for unrelated validation errors (nonce, gas limit, etc.) that
                // happen to involve a CIP-64 tx.
                let err_msg = alloc::format!("{e}");
                if err_msg.contains(FEE_DEBIT_ERROR_PREFIX)
                    || err_msg.contains(FEE_CREDIT_ERROR_PREFIX)
                {
                    let fc = fee_currency.unwrap();
                    tracing::warn!(
                        target: "celo",
                        "fee-currency debit/credit failed for {fc}: {e} — blocklisting"
                    );
                    let block_timestamp: u64 = self.ctx().block.timestamp.to();
                    self.blocklist.block_currency(fc, block_timestamp);
                }
            }
            _ => {}
        }

        result
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        self.inner.system_call_with_caller(caller, contract, data).map_err(map_op_err)
    }

    fn db_mut(&mut self) -> &mut Self::DB {
        &mut self.journaled_state.database
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec>) {
        let Context { block: block_env, cfg: cfg_env, journaled_state, .. } =
            self.inner.inner.0.ctx;

        (journaled_state.database, EvmEnv { block_env, cfg_env })
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspect = enabled;
    }

    fn precompiles(&self) -> &Self::Precompiles {
        &self.inner.inner.0.precompiles
    }

    fn precompiles_mut(&mut self) -> &mut Self::Precompiles {
        &mut self.inner.inner.0.precompiles
    }

    fn inspector(&self) -> &Self::Inspector {
        &self.inner.inner.0.inspector
    }

    fn inspector_mut(&mut self) -> &mut Self::Inspector {
        &mut self.inner.inner.0.inspector
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        let inner_evm = &self.inner.inner.0;
        (&inner_evm.ctx.journaled_state.database, &inner_evm.inspector, &inner_evm.precompiles)
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        let inner_evm = &mut self.inner.inner.0;
        (
            &mut inner_evm.ctx.journaled_state.database,
            &mut inner_evm.inspector,
            &mut inner_evm.precompiles,
        )
    }
}

/// Factory producing [`CeloEvm`]s.
///
/// Each EVM produced by this factory carries its own fresh [`Cip64Storage`]: the storage
/// is owned by the EVM instance, not the factory, so two consumers (e.g. the main-chain
/// executor and a re-executing ExEx) running through the same factory get independent
/// slots and never overwrite each other's pending CIP-64 receipt data.
#[derive(Debug, Default, Clone)]
pub struct CeloEvmFactory {
    /// Shared fee currency blocklist. EVMs created by this factory *populate* this blocklist
    /// when a CIP-64 fee-currency debit/credit fails during execution; the sequencing-time
    /// payload filter (`CeloFeeCurrencyFilter` in `celo-reth`) reads it to skip such currencies.
    /// `transact_raw` itself never rejects blocklisted currencies. Defaults to empty.
    pub blocklist: FeeCurrencyBlocklist,
}

impl CeloEvmFactory {
    /// Sets the shared fee currency blocklist.
    pub fn with_blocklist(mut self, blocklist: FeeCurrencyBlocklist) -> Self {
        self.blocklist = blocklist;
        self
    }
}

/// Creates a [`CeloEvm`] for testing with an in-memory database.
#[cfg(test)]
fn make_test_evm(
    blocklist: FeeCurrencyBlocklist,
) -> CeloEvm<revm::database::InMemoryDB, revm::inspector::NoOpInspector> {
    let spec_id = OpSpecId::FJORD;
    let db = revm::database::InMemoryDB::default();
    let mut cfg = revm::context::CfgEnv::<OpSpecId>::default();
    cfg.chain_id = 42220;
    CeloEvm {
        inner: Context::celo()
            .with_db(db)
            .with_cfg(cfg)
            .with_chain(default_l1_block_info(spec_id))
            .build_celo_with_inspector(revm::inspector::NoOpInspector {})
            .with_precompiles(CeloPrecompiles::new_with_spec(spec_id)),
        inspect: false,
        cip64_storage: Cip64Storage::default(),
        blocklist,
        last_evicted_timestamp: 0,
    }
}

impl CeloEvmFactory {
    /// Shared initialization for both `create_evm` and `create_evm_with_inspector`.
    fn build_evm<DB: Database, I: Inspector<CeloContext<DB>>>(
        &self,
        db: DB,
        mut input: EvmEnv<OpSpecId>,
        inspector: I,
        inspect: bool,
    ) -> CeloEvm<DB, I, PrecompilesMap> {
        input.cfg_env.limit_contract_code_size = Some(constants::CELO_MAX_CODE_SIZE);
        let spec_id = input.cfg_env.spec;
        CeloEvm {
            inner: Context::celo()
                .with_db(db)
                .with_block(input.block_env)
                .with_cfg(input.cfg_env)
                .with_chain(default_l1_block_info(spec_id))
                .build_celo_with_inspector(inspector)
                .with_precompiles(celo_precompiles_map(spec_id)),
            inspect,
            cip64_storage: Cip64Storage::default(),
            blocklist: self.blocklist.clone(),
            last_evicted_timestamp: 0,
        }
    }
}

impl EvmFactory for CeloEvmFactory {
    type Evm<DB: Database, I: Inspector<CeloContext<DB>>> = CeloEvm<DB, I, Self::Precompiles>;
    type Context<DB: Database> = CeloContext<DB>;
    type Tx = CeloTransaction<TxEnv>;
    type Error<DBError: core::error::Error + Send + Sync + 'static> = EVMError<DBError, OpTxError>;
    type HaltReason = OpHaltReason;
    type Spec = OpSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<OpSpecId>,
    ) -> Self::Evm<DB, NoOpInspector> {
        self.build_evm(db, input, NoOpInspector {}, false)
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<OpSpecId>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        self.build_evm(db, input, inspector, true)
    }
}

// SDM/post-exec is unscheduled on Celo: `RollupConfig::is_sdm_active` is hard-wired to `false`
// upstream, and Celo has no plans to activate it. This impl exists only so `CeloEvm` satisfies
// the `PostExecEvm` bound that `OpBlockExecutor: BlockExecutor` requires (mirroring the direct
// `PostExecEvm for OpEvm` impl in alloy-op-evm).
//
// Both methods panic: if SDM is ever activated on Celo (e.g. via an upstream rebase), the
// panic surfaces the gap immediately rather than silently returning a default value.
impl<DB, I, P> PostExecEvm for CeloEvm<DB, I, P>
where
    DB: Database,
    Self: Evm,
{
    fn begin_post_exec_tx(&mut self, _ctx: PostExecTxContext) {
        panic!("SDM unscheduled on Celo — `RollupConfig::is_sdm_active` must remain false");
    }

    fn take_last_post_exec_tx_result(&mut self) -> PostExecExecutedTx {
        panic!("SDM unscheduled on Celo — `RollupConfig::is_sdm_active` must remain false");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use alloy_evm::Evm;
    use alloy_primitives::TxKind;
    use celo_alloy_consensus::CeloTxType;
    use op_revm::OpTransaction;

    /// Build a CIP-64 `CeloTransaction<TxEnv>` for testing.
    fn make_cip64_tx(fee_currency: Address) -> CeloTransaction<TxEnv> {
        CeloTransaction {
            op_tx: OpTransaction {
                base: TxEnv {
                    caller: Address::with_last_byte(0x01),
                    kind: TxKind::Call(Address::with_last_byte(0x02)),
                    nonce: 0,
                    gas_limit: 21_000,
                    value: U256::ZERO,
                    data: Bytes::new(),
                    gas_price: 1_000_000_000,
                    chain_id: Some(42220),
                    gas_priority_fee: Some(100),
                    access_list: Default::default(),
                    blob_hashes: Vec::new(),
                    max_fee_per_blob_gas: 0,
                    tx_type: CeloTxType::Cip64 as u8,
                    authorization_list: Default::default(),
                },
                enveloped_tx: Some(Bytes::default()),
                deposit: Default::default(),
            },
            fee_currency: Some(fee_currency),
            cip64_tx_info: None,
            effective_gas_price: None,
        }
    }

    /// `transact_raw` must NOT reject a blocklisted currency: `base_fee_check_enabled`
    /// is also true during block import / derivation re-execution, so rejecting here
    /// would let a node's locally-accumulated blocklist reject a valid canonical block.
    /// Sequencing-time rejection lives in `CeloFeeCurrencyFilter` (see `celo-reth`'s
    /// `payload.rs`, `filter_skips_blocklisted_currency`), which derivation deliberately
    /// bypasses.
    #[test]
    fn test_blocklist_does_not_reject_in_transact_raw() {
        let fc = Address::with_last_byte(0xAA);

        // Run the identical tx through two EVMs — one with `fc` blocklisted, one without —
        // and assert the outcomes are byte-for-byte equal. This proves the blocklist had
        // zero effect on `transact_raw` (i.e. the tx executed rather than being
        // short-circuited), which a bare "no blocklisted error" check cannot distinguish
        // from the tx simply succeeding.
        let outcome = |blocklist: FeeCurrencyBlocklist| {
            let mut evm = make_test_evm(blocklist);
            format!("{:?}", evm.transact_raw(make_cip64_tx(fc)))
        };

        let blocked = FeeCurrencyBlocklist::default();
        blocked.block_currency(fc, 1000);

        let with_blocklist = outcome(blocked);
        let without_blocklist = outcome(FeeCurrencyBlocklist::default());

        assert_eq!(
            with_blocklist, without_blocklist,
            "blocklisting a fee currency must not change the transact_raw outcome (import safety)"
        );
        assert!(
            !with_blocklist.contains("blocklisted"),
            "transact_raw must not reject blocklisted currencies, got: {with_blocklist}"
        );
    }

    #[test]
    fn test_blocklist_allows_unblocked_currency() {
        let blocked_fc = Address::with_last_byte(0xAA);
        let other_fc = Address::with_last_byte(0xBB);
        let blocklist = FeeCurrencyBlocklist::default();
        blocklist.block_currency(blocked_fc, 1000);

        let mut evm = make_test_evm(blocklist);

        // A different fee currency should not be blocked (it may fail later
        // during execution for other reasons, but not at the blocklist check)
        let tx = make_cip64_tx(other_fc);
        let result = evm.transact_raw(tx);
        // If it fails, it should NOT be a blocklist error
        if let Err(e) = &result {
            let msg = format!("{e}");
            assert!(
                !msg.contains("blocklisted"),
                "Non-blocked currency should not get blocklist error, got: {msg}"
            );
        }
    }

    #[test]
    fn test_blocklist_does_not_block_native_tx() {
        let fc = Address::with_last_byte(0xAA);
        let blocklist = FeeCurrencyBlocklist::default();
        blocklist.block_currency(fc, 1000);

        let mut evm = make_test_evm(blocklist);

        // Native tx (no fee currency) should never be rejected by blocklist
        let mut tx = make_cip64_tx(fc);
        tx.fee_currency = None;
        tx.op_tx.base.tx_type = 2; // EIP-1559

        let result = evm.transact_raw(tx);
        if let Err(e) = &result {
            let msg = format!("{e}");
            assert!(
                !msg.contains("blocklisted"),
                "Native tx should not get blocklist error, got: {msg}"
            );
        }
    }

    /// Verify that non-debit/credit errors (e.g. unregistered currency) do NOT
    /// cause the currency to be blocklisted. Only debit/credit failures should
    /// trigger blocklisting.
    #[test]
    fn test_non_debit_error_does_not_blocklist() {
        let fc = Address::with_last_byte(0xCC);
        let blocklist = FeeCurrencyBlocklist::default();

        let mut evm = make_test_evm(blocklist.clone());
        // Set a non-zero basefee so the EVM is in "block building" mode
        evm.ctx_mut().block.basefee = 1_000_000_000;

        // This CIP-64 tx will fail (fee currency not registered), but the
        // error is NOT a debit/credit failure, so it should NOT be blocklisted.
        let tx = make_cip64_tx(fc);
        let result = evm.transact_raw(tx);
        assert!(result.is_err(), "Expected tx to fail");
        assert!(!blocklist.is_blocked(fc), "Non-debit/credit error should not cause blocklisting");
    }

    /// Verify that RPC simulation paths (eth_call / eth_estimateGas / debug_trace*)
    /// never store CIP-64 receipt data. Storage is per-EVM, so cross-EVM
    /// corruption is impossible — but multi-tx tracing (`debug_traceBlock`,
    /// `debug_traceTransaction`) re-executes multiple txs through one EVM
    /// without calling `build_receipt`. Without the `is_block_building` gate
    /// in `transact_raw`, the second CIP-64 tx would trip the slot-occupied
    /// panic in `store_cip64_info` on perfectly legitimate RPC paths.
    ///
    /// The handler still populates `cip64_tx_info` during simulation for
    /// native-fee CIP-64 txs (`feeCurrency == 0x0`), so this guard lives in
    /// `transact_raw`, not the handler.
    #[test]
    fn test_cip64_info_not_stored_during_rpc_simulation() {
        use revm::state::AccountInfo;

        let blocklist = FeeCurrencyBlocklist::default();
        let mut evm = make_test_evm(blocklist);

        // Fund the caller so the balance check passes during simulated execution.
        let caller = Address::with_last_byte(0x01);
        evm.db_mut().insert_account_info(
            caller,
            AccountInfo { balance: U256::from(10u128.pow(20)), nonce: 0, ..Default::default() },
        );

        // RPC simulation mode.
        evm.ctx_mut().cfg.disable_base_fee = true;

        // Native-fee CIP-64 tx (`fee_currency = 0x0`): the handler sets
        // `cip64_tx_info = Some(..)` on this path even when base fee is
        // disabled, so the only line of defense against polluting the slot is
        // the `is_block_building` gate in `transact_raw`.
        let mut tx = make_cip64_tx(Address::ZERO);
        tx.fee_currency = Some(Address::ZERO);
        let result = evm.transact_raw(tx);
        assert!(result.is_ok(), "simulated tx should succeed: {result:?}");

        assert!(
            evm.cip64_storage.pop_cip64_receipt_data().is_none(),
            "RPC simulation must not store CIP-64 receipt data"
        );
    }

    /// Two [`CeloEvm`] instances produced by the same [`CeloEvmFactory`] must own
    /// independent [`Cip64Storage`] slots. This is the regression for #183: when
    /// the proofs-history ExEx re-executes blocks through the same factory, its
    /// EVM's CIP-64 writes must not bleed into the main-chain executor's storage.
    #[test]
    fn two_evms_from_same_factory_have_independent_slots() {
        let factory = CeloEvmFactory::default();
        let db_a = revm::database::InMemoryDB::default();
        let db_b = revm::database::InMemoryDB::default();
        let env = EvmEnv::<OpSpecId>::default();
        let evm_a = factory.create_evm(db_a, env.clone());
        let evm_b = factory.create_evm(db_b, env);

        // Push to A only.
        evm_a.cip64_storage().store_cip64_info(None, celo_revm::Cip64Info::default());

        // B's slot is untouched.
        assert!(
            evm_b.cip64_storage().pop_cip64_receipt_data().is_none(),
            "second EVM's slot must be empty — factory must not share storage between EVMs"
        );
        // A's slot still has the entry.
        assert!(
            evm_a.cip64_storage().pop_cip64_receipt_data().is_some(),
            "first EVM's slot must still hold its own entry"
        );
    }

    /// Verify that the blocklist is NOT enforced during RPC simulation
    /// (eth_call / eth_estimateGas). RPC mode disables the base fee check,
    /// which `transact_raw` uses as the signal for "not block building".
    #[test]
    fn test_blocklist_bypassed_in_rpc_simulation() {
        let fc = Address::with_last_byte(0xAA);
        let blocklist = FeeCurrencyBlocklist::default();
        blocklist.block_currency(fc, 1000);

        let mut evm = make_test_evm(blocklist);

        // Enable RPC simulation mode: disable base fee check
        evm.ctx_mut().cfg.disable_base_fee = true;

        // Even though the currency is blocklisted, transact_raw should NOT
        // reject it — the blocklist only applies during block building.
        let tx = make_cip64_tx(fc);
        let result = evm.transact_raw(tx);
        if let Err(e) = &result {
            let msg = format!("{e}");
            assert!(
                !msg.contains("blocklisted"),
                "Blocklist must not apply during RPC simulation, got: {msg}"
            );
        }
    }
}
