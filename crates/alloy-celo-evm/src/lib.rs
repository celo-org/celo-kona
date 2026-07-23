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
    post_exec::{PostExecEvm, PostExecExecutedTx, PostExecTxContext, WarmingState},
};
use alloy_primitives::{Address, Bytes, U256};
use celo_revm::{
    CeloBuilder, CeloContext, CeloPrecompiles, CeloTransaction, DefaultCelo, constants,
    constants::{
        FEE_CREDIT_ERROR_PREFIX, FEE_CURRENCY_NOT_REGISTERED_PREFIX, FEE_CURRENCY_REVERT_MARKER,
        FEE_DEBIT_ERROR_PREFIX,
    },
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
    /// Whether this EVM reads from and writes to the fee currency [`blocklist`](Self::blocklist).
    ///
    /// The blocklist is a *local sequencing heuristic*: it records currencies whose debit/credit
    /// calls failed with a currency-level fault (halt / EVM call failure — contract *reverts*
    /// are sender/state faults and never blocklist) while the node was building a block from
    /// its own mempool, so the sequencer can skip them for a while. It must therefore only be
    /// touched on the sequencing path.
    /// Block import and derivation re-execute already-canonical blocks and must produce identical
    /// results regardless of this node's accumulated heuristic, so they leave it alone entirely.
    ///
    /// EVMs are created with this `false` by default ([`CeloEvmFactory::create_evm`], used by the
    /// import/derivation executor and RPC). It is flipped to `true` only by
    /// `CeloEvmConfig::builder_for_next_block` — the one entry point reth routes sequencing
    /// through (the payload builder), and which import/derivation deliberately bypass.
    blocklist_enabled: bool,
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
            blocklist_enabled: false,
        }
    }

    /// Enables fee currency blocklist reads/writes for this EVM. Called only on the sequencing
    /// path (`CeloEvmConfig::builder_for_next_block`); import, derivation and RPC leave it off so
    /// they never touch the shared blocklist.
    #[must_use]
    pub const fn with_blocklist_enabled(mut self) -> Self {
        self.blocklist_enabled = true;
        self
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

        // The base-fee check is enabled during real state transitions — both sequencing AND block
        // import / derivation re-execution — and disabled during RPC simulation (`eth_call`,
        // `eth_estimateGas`, tracing). It gates the CIP-64 receipt-info store, which import and
        // derivation also need.
        let base_fee_check_enabled = !self.ctx().cfg.is_base_fee_check_disabled();

        // The fee currency blocklist is a local sequencing heuristic and is only ever touched on
        // the sequencing path: `blocklist_enabled` is set on EVMs built via
        // `CeloEvmConfig::builder_for_next_block` (the payload builder) and left off for import /
        // derivation re-execution and RPC. Import and derivation therefore neither read nor write
        // it. (The `base_fee_check_enabled` conjunct is redundant given `blocklist_enabled` but
        // kept as an explicit guard against ever enabling the blocklist on an RPC-simulation EVM.)
        //
        // NOTE: blocklist *rejection* is intentionally NOT performed here even on the sequencing
        // path; it is enforced upstream in `CeloFeeCurrencyFilter` (see `celo-reth`'s
        // `payload.rs`). Performing it here would also catch import/derivation EVMs were
        // `blocklist_enabled` ever set on them, letting a node's locally-accumulated
        // blocklist reject a valid canonical block built by another sequencer. Below we
        // only *populate* the blocklist, and only when `apply_blocklist` holds — so import
        // and derivation neither read nor write it. Stale-entry eviction also lives upstream
        // in `CeloPayloadTransactions::best_transactions`, since that is the one place
        // `is_blocked` is read.
        let apply_blocklist = self.blocklist_enabled && base_fee_check_enabled;

        let result = if self.inspect { self.inner.inspect_tx(tx) } else { self.inner.transact(tx) }
            .map_err(map_op_err);

        match &result {
            Ok(_) => {
                // CIP64 NOTE:
                // Extract and store the cip64 info so the receipt builder can add the
                // credit/debit logs when building the receipt. Store only on the real
                // execution path, the only place `build_receipt` consumes it. We require both:
                //   - `base_fee_check_enabled`: RPC simulation (eth_call/estimateGas) disables the
                //     base-fee check and never builds receipts.
                //   - `!self.inspect`: tracing replays many txs through one shared, inspecting EVM
                //     and never builds receipts. parity `trace_block`/`trace_filter` and otterscan
                //     `ots_*` keep the base-fee check enabled, so without this conjunct the second
                //     CIP-64 tx would trip the slot-occupied panic in `store_cip64_info`.
                // Confining the store to the receipt-building path keeps that panic a true
                // signal of an executor double-store bug (see `Cip64Storage` docs), rather than
                // a false positive on legitimate tracing.
                let cip64_info = self.inner.inner.0.ctx.tx.cip64_tx_info.take();
                if base_fee_check_enabled
                    && !self.inspect
                    && let Some(cip64_info) = cip64_info
                {
                    self.cip64_storage.store_cip64_info(fee_currency, cip64_info);
                }
            }
            Err(e) if apply_blocklist && fee_currency.is_some() => {
                // Classify why this CIP-64 tx failed during block building. Only a
                // fee-currency debit/credit failure should blocklist the currency, not
                // unrelated validation errors (nonce, gas limit, etc.) that happen to
                // involve a CIP-64 tx.
                //
                // Classification is by error-message prefix, not by matching a typed
                // variant: the celo-revm errors are typed at the source (e.g.
                // `FeeCurrencyError`, the FEE_DEBIT/CREDIT prefixes), but they reach here
                // flattened into op-revm's `OpTransactionError` / revm's
                // `InvalidTransaction` — closed enums with no Celo variant — so the only
                // signal that survives the boundary is the Display string.
                let fc = fee_currency.unwrap();
                let err_msg = alloc::format!("{e}");
                if err_msg.contains(FEE_DEBIT_ERROR_PREFIX)
                    || err_msg.contains(FEE_CREDIT_ERROR_PREFIX)
                {
                    if err_msg.contains(FEE_CURRENCY_REVERT_MARKER) {
                        // The fee-currency contract *reverted* the debit/credit — a
                        // sender/state fault, canonically `ERC20: transfer amount
                        // exceeds balance` from an underfunded sender that slipped
                        // past pool admission. The tx is dropped from the payload
                        // either way; blocklisting here would let a single
                        // underfunded sender suppress an entire healthy currency
                        // for the whole eviction window. op-geth never blocklists
                        // this case (its pool balance-filter keeps such txs from
                        // the builder), so blocklisting broke sequencer parity.
                        tracing::warn!(
                            target: "celo",
                            "fee-currency debit/credit reverted for {fc}: {e} — \
                             dropping tx without blocklisting the currency"
                        );
                        #[cfg(feature = "std")]
                        metrics::counter!(
                            "celo_payload_skipped_total",
                            "reason" => "debit_credit_reverted"
                        )
                        .increment(1);
                    } else {
                        // Halt (e.g. the debit exhausted its gas budget) or an
                        // EVM-level call failure — a genuine currency fault:
                        // blocklist so the payload builder stops retrying every
                        // tx of this currency.
                        tracing::warn!(
                            target: "celo",
                            "fee-currency debit/credit failed for {fc}: {e} — blocklisting"
                        );
                        let block_timestamp: u64 = self.ctx().block.timestamp.to();
                        self.blocklist.block_currency(fc, block_timestamp);
                    }
                } else if err_msg.contains(FEE_CURRENCY_NOT_REGISTERED_PREFIX) {
                    // The fee currency is not in the per-block fee-currency context: its
                    // directory config could not be read, so it was dropped while loading
                    // (see `celo_revm::contracts::core_contracts::get_currency_info`). The
                    // tx is excluded from the block. This is otherwise silent — it fails
                    // before debit/credit, so the blocklist branch above never logs it —
                    // so surface it here as both a log and a metric.
                    tracing::warn!(
                        target: "celo",
                        "CIP-64 tx excluded from block: fee currency {fc} is not loaded in the \
                         per-block fee-currency context ({e})"
                    );
                    #[cfg(feature = "std")]
                    metrics::counter!(
                        "celo_payload_skipped_total",
                        "reason" => "fee_currency_not_registered"
                    )
                    .increment(1);
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
    /// when a CIP-64 fee-currency debit/credit fails during execution, but only on the sequencing
    /// path (`CeloEvm::with_blocklist_enabled`); import/derivation EVMs leave it untouched. The
    /// sequencing-time payload filter (`CeloFeeCurrencyFilter` in `celo-reth`) reads it to skip
    /// such currencies. `transact_raw` itself never rejects blocklisted currencies. Defaults to
    /// empty.
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
        // Tests here exercise the sequencing-path blocklist behaviour, so enable it. The
        // RPC-simulation test additionally disables the base-fee check, which the
        // `base_fee_check_enabled` guard in `transact_raw` still honours independently.
        blocklist_enabled: true,
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
            // Off by default: the import/derivation executor and RPC create EVMs through the
            // factory and must not touch the blocklist. Sequencing flips it on via
            // `with_blocklist_enabled` in `CeloEvmConfig::builder_for_next_block`.
            blocklist_enabled: false,
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
// All four methods panic: if SDM is ever activated on Celo (e.g. via an upstream rebase), the
// panic surfaces the gap immediately rather than silently returning a default value.
// `warming_state`/`seed_warming_state` only carry SDM block-warming refund state across
// flashblock executors (op-rbuilder), a path Celo never takes.
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

    fn warming_state(&self) -> WarmingState {
        panic!("SDM unscheduled on Celo — `RollupConfig::is_sdm_active` must remain false");
    }

    fn seed_warming_state(&mut self, _state: WarmingState) {
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

    /// Run a CIP-64 tx through a sequencing-mode EVM whose fee currency `fc`
    /// is registered in the per-block context and backed by `code` at the
    /// token address, so the `debitGasFees` system call genuinely executes
    /// that bytecode. Returns the resulting error, stringified.
    fn transact_cip64_with_token_code(
        blocklist: FeeCurrencyBlocklist,
        fc: Address,
        code: &'static [u8],
    ) -> String {
        use celo_revm::fee_currency_context::FeeCurrencyInfo;
        use revm::state::{AccountInfo, Bytecode};

        let mut evm = make_test_evm(blocklist);
        // Non-zero basefee puts the EVM in block-building mode (apply_blocklist on).
        evm.ctx_mut().block.basefee = 1_000_000_000;
        evm.db_mut().insert_account_info(
            fc,
            AccountInfo::from_bytecode(Bytecode::new_raw(Bytes::from_static(code))),
        );
        // Register the currency in the per-block fee-currency context, pinned
        // to the current block so the handler uses it as-is instead of
        // rebuilding it from (empty) directory state.
        let mut currencies = alloy_primitives::map::HashMap::default();
        currencies.insert(
            fc,
            FeeCurrencyInfo {
                exchange_rate: (U256::from(1), U256::from(1)),
                intrinsic_gas: 50_000,
            },
        );
        let block_number = evm.ctx_mut().block.number;
        evm.inner.fee_currency_context =
            celo_revm::FeeCurrencyContext::new(currencies, Some(block_number));

        let mut tx = make_cip64_tx(fc);
        // Cover the standard intrinsic plus the currency's extra intrinsic gas.
        tx.op_tx.base.gas_limit = 200_000;
        let result = evm.transact_raw(tx);
        format!("{:?}", result.expect_err("CIP-64 tx with a failing debit must error"))
    }

    /// A fee-currency contract that *reverts* the debit — canonically an
    /// underfunded sender's `ERC20: transfer amount exceeds balance` — is a
    /// sender/state fault, not a currency fault. The tx is dropped from the
    /// payload either way, but the currency must NOT be blocklisted:
    /// otherwise a single underfunded sender suppresses every tx of a healthy
    /// currency for the whole eviction window while this node sequences.
    #[test]
    fn test_debit_revert_does_not_blocklist_currency() {
        let fc = Address::with_last_byte(0xD0);
        let blocklist = FeeCurrencyBlocklist::default();
        // PUSH1 0, PUSH1 0, REVERT — the debit call reverts.
        let err =
            transact_cip64_with_token_code(blocklist.clone(), fc, &[0x60, 0x00, 0x60, 0x00, 0xfd]);
        assert!(err.contains(FEE_DEBIT_ERROR_PREFIX), "expected a debit failure, got: {err}");
        assert!(
            !blocklist.is_blocked(fc),
            "a debit revert is a sender/state fault and must not blocklist the currency; \
             got error: {err}"
        );
    }

    /// A fee-currency contract that *halts* the debit (burns through the
    /// debit call's gas budget) is a genuine currency fault and must still be
    /// blocklisted.
    #[test]
    fn test_debit_halt_still_blocklists_currency() {
        let fc = Address::with_last_byte(0xD1);
        let blocklist = FeeCurrencyBlocklist::default();
        // JUMPDEST, PUSH1 0, JUMP — infinite loop, exhausts the debit budget → OOG halt.
        let err = transact_cip64_with_token_code(blocklist.clone(), fc, &[0x5b, 0x60, 0x00, 0x56]);
        assert!(err.contains(FEE_DEBIT_ERROR_PREFIX), "expected a debit failure, got: {err}");
        assert!(
            blocklist.is_blocked(fc),
            "a debit halt (out-of-gas) is a currency fault and must blocklist; got error: {err}"
        );
    }

    /// A CIP-64 tx in a fee currency missing from the per-block context fails
    /// before debit/credit, so the blocklist branch never logs it — `transact_raw`
    /// must instead classify it via `FEE_CURRENCY_NOT_REGISTERED_PREFIX` and meter
    /// it as `celo_payload_skipped_total{reason=fee_currency_not_registered}`.
    /// This drives the real path: the empty fee-currency context genuinely produces
    /// the typed `NotRegistered` error, flattened to a string carrying the prefix
    /// the classifier matches on — not a mocked error.
    #[cfg(feature = "std")]
    #[test]
    fn test_unregistered_fee_currency_is_metered_not_blocklisted() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let fc = Address::with_last_byte(0xCD);
        let blocklist = FeeCurrencyBlocklist::default();
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        let err_msg = metrics::with_local_recorder(&recorder, || {
            let mut evm = make_test_evm(blocklist.clone());
            // Non-zero basefee puts the EVM in block-building mode (apply_blocklist on).
            evm.ctx_mut().block.basefee = 1_000_000_000;
            let result = evm.transact_raw(make_cip64_tx(fc));
            format!("{:?}", result.expect_err("unregistered fee currency must fail"))
        });

        // Real classification signal: the tx genuinely took the NotRegistered path.
        assert!(
            err_msg.contains(FEE_CURRENCY_NOT_REGISTERED_PREFIX),
            "expected the not-registered prefix in the error, got: {err_msg}"
        );
        // NotRegistered is not a debit/credit failure, so it must not blocklist.
        assert!(!blocklist.is_blocked(fc), "unregistered currency must not be blocklisted");

        // ...and it must have incremented the skip counter with the right reason label.
        let skipped: u64 = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(ck, _, _, _)| {
                ck.key().name() == "celo_payload_skipped_total"
                    && ck
                        .key()
                        .labels()
                        .any(|l| l.key() == "reason" && l.value() == "fee_currency_not_registered")
            })
            .map(|(_, _, _, v)| match v {
                DebugValue::Counter(c) => c,
                other => panic!("expected a counter, got {other:?}"),
            })
            .sum();
        assert_eq!(skipped, 1, "celo_payload_skipped_total must increment exactly once");
    }

    /// Verify that base-fee-disabled RPC simulation (eth_call / eth_estimateGas) never stores
    /// CIP-64 receipt data: the `base_fee_check_enabled` gate in `transact_raw` skips the store
    /// on those paths, which never build receipts. (The inspecting/tracing path is covered
    /// separately by [`test_cip64_info_not_stored_while_inspecting`].)
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
        // the `base_fee_check_enabled` gate in `transact_raw`.
        let mut tx = make_cip64_tx(Address::ZERO);
        tx.fee_currency = Some(Address::ZERO);
        let result = evm.transact_raw(tx);
        assert!(result.is_ok(), "simulated tx should succeed: {result:?}");

        assert!(
            evm.cip64_storage.pop_cip64_receipt_data().is_none(),
            "RPC simulation must not store CIP-64 receipt data"
        );
    }

    /// Verify that an inspecting EVM (block tracing) does not store CIP-64 receipt data even
    /// with the base-fee check ENABLED — the parity `trace_block` / otterscan `ots_*` path.
    /// Before the `!self.inspect` gate in `transact_raw`, the store ran here and a second
    /// CIP-64 tx replayed through the same EVM tripped the slot-occupied panic in
    /// `store_cip64_info`. The panic is intentional for the executor path (see `Cip64Storage`
    /// docs), so the fix skips the store while inspecting rather than weakening the panic.
    #[test]
    fn test_cip64_info_not_stored_while_inspecting() {
        use revm::state::AccountInfo;

        let blocklist = FeeCurrencyBlocklist::default();
        let mut evm = make_test_evm(blocklist);

        let caller = Address::with_last_byte(0x01);
        evm.db_mut().insert_account_info(
            caller,
            AccountInfo { balance: U256::from(10u128.pow(20)), nonce: 0, ..Default::default() },
        );

        // Tracing EVM: inspecting, but base fee left ENABLED (unlike eth_call / estimateGas).
        evm.set_inspector_enabled(true);

        let mut tx = make_cip64_tx(Address::ZERO);
        tx.fee_currency = Some(Address::ZERO);
        let result = evm.transact_raw(tx);
        assert!(result.is_ok(), "inspecting tx should succeed: {result:?}");

        assert!(
            evm.cip64_storage.pop_cip64_receipt_data().is_none(),
            "tracing (inspecting EVM) must not store CIP-64 receipt data"
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
