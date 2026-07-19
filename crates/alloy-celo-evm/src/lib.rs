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
        FEE_CREDIT_ERROR_PREFIX, FEE_CURRENCY_NOT_REGISTERED_PREFIX, FEE_DEBIT_ERROR_PREFIX,
    },
    precompiles::transfer::{TRANSFER_ADDRESS, TRANSFER_GAS_COST},
};
use core::{
    fmt::Debug,
    ops::{Deref, DerefMut},
};
use op_revm::{
    L1BlockInfo, OpHaltReason, OpSpecId, precompiles::OpPrecompiles,
    transaction::deposit::DEPOSIT_TRANSACTION_TYPE,
};
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
pub mod fee_context_cache;

use blocklist::FeeCurrencyBlocklist;
use cip64_storage::Cip64Storage;
use fee_context_cache::{FeeCurrencyContextCache, L1_INFO_DEPOSITOR};

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
    /// calls failed while the node was building a block from its own mempool, so the sequencer
    /// can skip them for a while. It must therefore only be touched on the sequencing path.
    /// Block import and derivation re-execute already-canonical blocks and must produce identical
    /// results regardless of this node's accumulated heuristic, so they leave it alone entirely.
    ///
    /// EVMs are created with this `false` by default ([`CeloEvmFactory::create_evm`], used by the
    /// import/derivation executor and RPC). It is flipped to `true` only by
    /// `CeloEvmConfig::builder_for_next_block` — the one entry point reth routes sequencing
    /// through (the payload builder), and which import/derivation deliberately bypass.
    blocklist_enabled: bool,
    /// Shared cache of block-start fee-currency contexts, keyed by `(number, parent_hash)`.
    ///
    /// Exists for reth's `debug_trace*` path, which replays a block through a fresh EVM per
    /// transaction over cumulatively-committed state: without the cache, each per-tx EVM
    /// re-loads exchange rates from *mid-block* state, diverging from the consensus rule that
    /// all transactions in a block settle at block-start rates. See [`fee_context_cache`] for
    /// the full design.
    fee_context_cache: FeeCurrencyContextCache,
    /// Whether this EVM reads from and writes to the
    /// [`fee_context_cache`](Self::fee_context_cache).
    ///
    /// The cache is an *RPC replay aid*, and its write gate trusts transaction content that
    /// `debug_traceRawBlock` lets a caller forge (an unsigned deposit's `from`), so consensus
    /// execution must never consult it. EVMs are created with this `true` by default
    /// ([`CeloEvmFactory::create_evm`]) so the loose per-tx EVMs reth's RPC layer builds
    /// (`debug_trace*`, `trace_*`, replay helpers) participate; every consensus path — block
    /// import, derivation, sequencing, the kona proof executor — receives its EVM through
    /// [`CeloBlockExecutorFactory::create_executor`](block::CeloBlockExecutorFactory), which
    /// flips it off via [`with_fee_context_cache_disabled`](Self::with_fee_context_cache_disabled)
    /// (the inverse of the blocklist's opt-in pattern). Consensus loses nothing: a
    /// single-EVM-per-block executor loads the correct context fresh at tx 0 by construction.
    fee_context_cache_enabled: bool,
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
            blocklist_enabled: false,
            fee_context_cache: FeeCurrencyContextCache::default(),
            fee_context_cache_enabled: true,
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

    /// Disables the block-start fee-context cache for this EVM. Called on every consensus
    /// execution path via
    /// [`CeloBlockExecutorFactory::create_executor`](block::CeloBlockExecutorFactory) so that
    /// import, derivation, sequencing and proof execution neither read the RPC-writable cache
    /// nor pay its extra `Database::block_hash` lookup; only the loose per-tx EVMs the RPC
    /// layer builds keep it on. See the `fee_context_cache_enabled` field docs and the
    /// [`fee_context_cache`] module docs.
    #[must_use]
    pub const fn with_fee_context_cache_disabled(mut self) -> Self {
        self.fee_context_cache_enabled = false;
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
        // Capture whether this is the L1-info deposit (always tx 0 of a block) — the one
        // transaction whose EVM provably sees block-start state, gating fee-context harvesting
        // below.
        let is_l1_info_deposit = tx.op_tx.base.tx_type == DEPOSIT_TRANSACTION_TYPE
            && tx.op_tx.base.caller == L1_INFO_DEPOSITOR;

        // The base-fee check is enabled during replay-style execution — sequencing, block import
        // / derivation re-execution, AND block tracing (`debug_trace*`, `trace_*`, `ots_*`) — and
        // disabled during call-style RPC simulation (`eth_call`, `eth_estimateGas`,
        // `debug_traceCall`). It gates the CIP-64 receipt-info store, which import and derivation
        // also need, and participation in the block-start fee-context cache below.
        let base_fee_check_enabled = !self.ctx().cfg.is_base_fee_check_disabled();

        // Block-start fee-currency context cache (see `fee_context_cache` module docs).
        //
        // Consensus rule: every transaction of a block settles CIP-64 fees at the exchange rates
        // read from *block-start* state. Single-EVM-per-block paths (executor, parity tracer) get
        // that from the `updated_at_block` check in `load_fee_currency_context`; reth's
        // `debug_trace*` builds a fresh EVM per transaction over mid-block state, so here we seed
        // the context from the shared cache instead of letting the handler re-load mid-block
        // rates. Participation requires both:
        //   - `fee_context_cache_enabled`: consensus executors (import/derivation/sequencing/
        //     proofs) opt out at `CeloBlockExecutorFactory::create_executor` — they load the
        //     correct context fresh at tx 0 and must never depend on RPC-writable state.
        //   - `base_fee_check_enabled`: call-style simulations (`eth_call`/`estimateGas`/
        //     `debug_traceCall`) run at end-of-block state where the rates they load are the
        //     intended semantics.
        let use_fee_context_cache = self.fee_context_cache_enabled && base_fee_check_enabled;
        let block_number = self.ctx().block.number;
        let mut cache_key = None;
        if use_fee_context_cache
            && self.inner.fee_currency_context.updated_at_block != Some(block_number)
        {
            let number: u64 = block_number.saturating_to();
            // Genesis has no parent; a failing `block_hash` (fail open) skips caching entirely.
            if number > 0
                && let Ok(parent_hash) = self.db_mut().block_hash(number - 1)
            {
                cache_key = Some((number, parent_hash));
                // The L1-info deposit's EVM never *reads* the cache: in an honest replay its DB
                // is at block-start state, so a fresh load is correct by construction and the
                // read buys nothing. Skipping it means the harvest below always overwrites the
                // entry with a freshly-loaded context, so every block trace repairs a stale or
                // poisoned entry at tx 0 before its later per-tx EVMs consult it.
                if !is_l1_info_deposit {
                    if let Some(context) = self.fee_context_cache.get(number, parent_hash) {
                        // `load_fee_currency_context` will short-circuit on `updated_at_block`.
                        // No directory system calls run, so there is no warmth to neutralize —
                        // same observable state as consensus transactions 1..n of the block.
                        self.inner.fee_currency_context = context;
                    } else {
                        // A miss mid-block means this trace EVM falls back to loading rates from
                        // mid-block state — the pre-cache behavior — and the resulting trace may
                        // be silently wrong if an earlier tx moved a rate. Rare (FIFO eviction or
                        // an inconsistent `block_hash` view), but make it observable.
                        tracing::debug!(
                            target: "celo",
                            block = number,
                            "fee-context cache miss on replay-style execution; \
                             falling back to mid-block rates"
                        );
                    }
                }
            }
        }

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
                    tracing::warn!(
                        target: "celo",
                        "fee-currency debit/credit failed for {fc}: {e} — blocklisting"
                    );
                    let block_timestamp: u64 = self.ctx().block.timestamp.to();
                    self.blocklist.block_currency(fc, block_timestamp);
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

        // Harvest the block-start fee-currency context into the shared cache. Population is
        // restricted to the EVM that executed the L1-info deposit: in every honest replay that
        // transaction is index 0, so nothing of the block was committed to the DB the context
        // was loaded from — the context is block-start. `debug_traceRawBlock` lets a caller
        // forge a deposit at a later index (deposits are unsigned), so a poisoned write is
        // possible — but with consensus opted out (`fee_context_cache_enabled`) its blast
        // radius is trace output only, and honest traces overwrite the entry at tx 0 (the
        // deposit EVM never seeds, see above). Mid-block trace EVMs that missed the cache load
        // mid-block rates (pre-cache behavior) but never write. The `updated_at_block` guard
        // skips harvesting when execution failed before the handler loaded the context.
        if let Some((number, parent_hash)) = cache_key
            && is_l1_info_deposit
            && self.inner.fee_currency_context.updated_at_block == Some(block_number)
        {
            self.fee_context_cache.insert(
                number,
                parent_hash,
                self.inner.fee_currency_context.clone(),
            );
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
    /// Shared cache of block-start fee-currency contexts. Populated by the EVM executing a
    /// block's L1-info deposit (tx 0) and consulted by later EVMs replaying transactions of the
    /// same block, so that reth's per-transaction trace EVMs (`debug_trace*`) validate CIP-64
    /// fees against block-start exchange rates like consensus does. See [`fee_context_cache`].
    ///
    /// Unlike [`blocklist`](Self::blocklist) this is an implementation detail, not
    /// configuration: nothing outside the crate constructs or inspects it, so it stays
    /// `pub(crate)` (wired by `Default`).
    pub(crate) fee_context_cache: FeeCurrencyContextCache,
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
        fee_context_cache: FeeCurrencyContextCache::default(),
        fee_context_cache_enabled: true,
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
            fee_context_cache: self.fee_context_cache.clone(),
            // On by default so the loose per-tx EVMs reth's RPC layer builds participate;
            // consensus executors flip it off in `create_executor` (see the field docs).
            fee_context_cache_enabled: true,
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

    /// Build a deposit `CeloTransaction<TxEnv>` from the given caller. With
    /// [`L1_INFO_DEPOSITOR`] as caller this is the L1-info deposit — always tx 0 of a block —
    /// which is what gates fee-context cache population in `transact_raw`.
    fn make_deposit_tx(caller: Address) -> CeloTransaction<TxEnv> {
        CeloTransaction {
            op_tx: OpTransaction {
                base: TxEnv {
                    caller,
                    tx_type: DEPOSIT_TRANSACTION_TYPE,
                    gas_limit: 1_000_000,
                    ..Default::default()
                },
                enveloped_tx: None,
                deposit: Default::default(),
            },
            fee_currency: None,
            cip64_tx_info: None,
            effective_gas_price: None,
        }
    }

    /// The parent hash `transact_raw` computes for a test EVM at block `number`:
    /// `InMemoryDB` delegates `block_hash` to `EmptyDB`, which is deterministic.
    fn test_db_parent_hash(number: u64) -> alloy_primitives::B256 {
        revm::Database::block_hash(&mut revm::database::InMemoryDB::default(), number - 1).unwrap()
    }

    /// A `FeeCurrencyContext` as loaded at the start of block `number`, with `fc` registered.
    /// The empty in-memory DB can never produce a registered currency, so observing `fc` in an
    /// EVM's context proves it came from the cache.
    fn block_start_context(number: u64, fc: Address) -> celo_revm::FeeCurrencyContext {
        let mut currencies = alloy_primitives::map::HashMap::default();
        currencies.insert(
            fc,
            celo_revm::fee_currency_context::FeeCurrencyInfo {
                exchange_rate: (U256::from(2), U256::from(1)),
                intrinsic_gas: 50_000,
            },
        );
        celo_revm::FeeCurrencyContext::new(currencies, Some(U256::from(number)))
    }

    /// `transact_raw` must seed the fee-currency context from the shared cache instead of
    /// letting the handler re-load it from (mid-block) DB state. This is the fix for
    /// `debug_traceBlock*` replaying each tx in a fresh EVM: without seeding, a CIP-64 tx
    /// traced after an in-block rate update is validated at the wrong rate
    /// ("max fee per gas less than block base fee").
    #[test]
    fn test_transact_raw_seeds_context_from_cache() {
        let fc = Address::with_last_byte(0xAA);

        let cache = FeeCurrencyContextCache::default();
        cache.insert(5, test_db_parent_hash(5), block_start_context(5, fc));

        let mut evm = make_test_evm(FeeCurrencyBlocklist::default());
        evm.fee_context_cache = cache;
        evm.ctx_mut().block.number = U256::from(5);

        // Outcome irrelevant: seeding happens before execution and must survive tx failure.
        let _ = evm.transact_raw(make_cip64_tx(fc));

        assert_eq!(
            evm.inner.fee_currency_context.currency_exchange_rate(Some(fc)),
            Ok((U256::from(2), U256::from(1))),
            "context must come from the cache, not a fresh load from the (empty) DB"
        );
    }

    /// Only the EVM executing the L1-info deposit (provably at block-start state) may
    /// populate the cache; any other tx must not, or a mid-block trace EVM could poison
    /// the entry with mid-block rates.
    #[test]
    fn test_only_l1_info_deposit_populates_cache() {
        let parent = test_db_parent_hash(5);

        // L1-info deposit → populates.
        let mut evm = make_test_evm(FeeCurrencyBlocklist::default());
        evm.ctx_mut().block.number = U256::from(5);
        evm.transact_raw(make_deposit_tx(L1_INFO_DEPOSITOR)).expect("deposit must execute");
        let harvested = evm
            .fee_context_cache
            .get(5, parent)
            .expect("L1-info deposit must populate the block-start cache");
        assert_eq!(harvested.updated_at_block, Some(U256::from(5)));

        // User deposit (index > 0 possible) → must not populate.
        let mut evm = make_test_evm(FeeCurrencyBlocklist::default());
        evm.ctx_mut().block.number = U256::from(5);
        evm.transact_raw(make_deposit_tx(Address::with_last_byte(0x01)))
            .expect("deposit must execute");
        assert!(evm.fee_context_cache.get(5, parent).is_none());

        // Regular (non-deposit) tx → must not populate.
        let mut evm = make_test_evm(FeeCurrencyBlocklist::default());
        evm.ctx_mut().block.number = U256::from(5);
        let _ = evm.transact_raw(make_cip64_tx(Address::with_last_byte(0xAA)));
        assert!(evm.fee_context_cache.get(5, parent).is_none());
    }

    /// Consensus executors (import/derivation/sequencing/proofs) opt out of the cache via
    /// `with_fee_context_cache_disabled` in `CeloBlockExecutorFactory::create_executor`: a
    /// disabled EVM must neither read a (potentially RPC-poisoned) entry nor populate one.
    #[test]
    fn test_disabled_evm_bypasses_cache() {
        let fc = Address::with_last_byte(0xAA);
        let parent = test_db_parent_hash(5);

        // No read: a pre-seeded (think: poisoned) entry must not reach the EVM.
        let cache = FeeCurrencyContextCache::default();
        cache.insert(5, parent, block_start_context(5, fc));
        let mut evm = make_test_evm(FeeCurrencyBlocklist::default());
        evm.fee_context_cache = cache;
        let mut evm = evm.with_fee_context_cache_disabled();
        evm.ctx_mut().block.number = U256::from(5);
        let _ = evm.transact_raw(make_cip64_tx(fc));
        assert!(
            evm.inner.fee_currency_context.currency_exchange_rate(Some(fc)).is_err(),
            "disabled EVM must load from its own DB state, not the shared cache"
        );

        // No write: even the L1-info deposit must not populate when disabled.
        let mut evm =
            make_test_evm(FeeCurrencyBlocklist::default()).with_fee_context_cache_disabled();
        evm.ctx_mut().block.number = U256::from(5);
        evm.transact_raw(make_deposit_tx(L1_INFO_DEPOSITOR)).expect("deposit must execute");
        assert!(evm.fee_context_cache.get(5, parent).is_none());
    }

    /// The L1-info deposit's EVM must never seed from the cache (its fresh load is correct by
    /// construction) and its harvest must overwrite the existing entry — this is the
    /// self-healing property: every block trace repairs a stale or poisoned entry at tx 0
    /// before later per-tx EVMs of the same request read it.
    #[test]
    fn test_l1_info_deposit_ignores_and_repairs_existing_entry() {
        let fc = Address::with_last_byte(0xAA);
        let parent = test_db_parent_hash(5);

        // A "poisoned" entry: registers `fc`, which the empty DB can never produce.
        let cache = FeeCurrencyContextCache::default();
        cache.insert(5, parent, block_start_context(5, fc));

        let mut evm = make_test_evm(FeeCurrencyBlocklist::default());
        evm.fee_context_cache = cache.clone();
        evm.ctx_mut().block.number = U256::from(5);
        evm.transact_raw(make_deposit_tx(L1_INFO_DEPOSITOR)).expect("deposit must execute");

        // Not seeded: the deposit's own context came from a fresh (empty-DB) load.
        assert!(
            evm.inner.fee_currency_context.currency_exchange_rate(Some(fc)).is_err(),
            "the L1-info deposit's EVM must never read the cache"
        );
        // Repaired: the poisoned entry was overwritten with the freshly-loaded context.
        let repaired = cache.get(5, parent).expect("entry must still exist");
        assert_eq!(repaired.updated_at_block, Some(U256::from(5)));
        assert!(
            repaired.currency_exchange_rate(Some(fc)).is_err(),
            "harvest must overwrite the poisoned entry with the fresh block-start context"
        );
    }

    /// Call-style simulations (`eth_call`/`eth_estimateGas`/`debug_traceCall` set
    /// `disable_base_fee`) run at end-of-block state where the rates they load are the
    /// intended semantics — they must neither read block-start rates from the cache nor
    /// populate it.
    #[test]
    fn test_call_style_simulation_bypasses_cache() {
        let fc = Address::with_last_byte(0xAA);

        // No read: seeded rate must not reach the EVM.
        let cache = FeeCurrencyContextCache::default();
        cache.insert(5, test_db_parent_hash(5), block_start_context(5, fc));
        let mut evm = make_test_evm(FeeCurrencyBlocklist::default());
        evm.fee_context_cache = cache;
        evm.ctx_mut().block.number = U256::from(5);
        evm.ctx_mut().cfg.disable_base_fee = true;
        let _ = evm.transact_raw(make_cip64_tx(fc));
        assert!(
            evm.inner.fee_currency_context.currency_exchange_rate(Some(fc)).is_err(),
            "call-style simulation must load from its own DB state, not the cache"
        );

        // No write: even the L1-info deposit must not populate under disable_base_fee.
        let mut evm = make_test_evm(FeeCurrencyBlocklist::default());
        evm.ctx_mut().block.number = U256::from(5);
        evm.ctx_mut().cfg.disable_base_fee = true;
        evm.transact_raw(make_deposit_tx(L1_INFO_DEPOSITOR)).expect("deposit must execute");
        assert!(evm.fee_context_cache.get(5, test_db_parent_hash(5)).is_none());
    }

    /// Genesis has no parent to key on — the cache must be skipped entirely, not fed a
    /// bogus key.
    #[test]
    fn test_genesis_block_skips_cache() {
        let mut evm = make_test_evm(FeeCurrencyBlocklist::default());
        // make_test_evm defaults to block 0.
        evm.transact_raw(make_deposit_tx(L1_INFO_DEPOSITOR)).expect("deposit must execute");
        // Nothing to assert via get() without a defined key shape for genesis — assert the
        // context was still loaded normally (handler path unaffected).
        assert_eq!(evm.inner.fee_currency_context.updated_at_block, Some(U256::ZERO));
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
