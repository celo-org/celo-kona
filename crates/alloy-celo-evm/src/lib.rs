#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::{borrow::Cow, format, sync::Arc};
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
use core::ops::{Deref, DerefMut};
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
pub mod fee_context_cache;

use blocklist::FeeCurrencyBlocklist;
use cip64_storage::Cip64Storage;
use fee_context_cache::{FeeContextResolver, FeeCurrencyContextCache};

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
    /// Shared memo of block-start fee-currency contexts, keyed by `(number, parent_hash)`, filled
    /// from the [`context_resolver`](Self::context_resolver) on a miss. Pins reth's per-tx
    /// `debug_trace*` replay EVMs to block-start CIP-64 rates. See [`fee_context_cache`].
    fee_context_cache: FeeCurrencyContextCache,
    /// Trusted resolver consulted on a [`fee_context_cache`](Self::fee_context_cache) miss; wired
    /// by `celo-reth`, `None` on no-std consumers (kona). See [`FeeContextResolver`].
    context_resolver: Option<Arc<dyn FeeContextResolver>>,
    /// Whether this EVM participates in block-start fee-context pinning (consults the
    /// [`fee_context_cache`](Self::fee_context_cache) and
    /// [`context_resolver`](Self::context_resolver)). `true` by default so the loose per-tx EVMs
    /// reth's RPC layer builds participate; consensus opts out at
    /// [`create_executor`](block::CeloBlockExecutorFactory) via
    /// [`for_block_executor`](Self::for_block_executor). See [`fee_context_cache`].
    fee_context_cache_enabled: bool,
    /// Whether this EVM stores CIP-64 receipt data into its [`Cip64Storage`] after each tx.
    ///
    /// The single-entry slot is popped once per tx by the receipt builder, so `store_cip64_info`
    /// panics if a second tx stores before the first is consumed. Only receipt-building executors
    /// may store: `false` by default, flipped on solely by
    /// [`for_block_executor`](Self::for_block_executor) (via
    /// [`create_executor`](block::CeloBlockExecutorFactory) and the dormant post-exec paths).
    /// Loose RPC EVMs (parity `trace_*`,
    /// otterscan `ots_*`, `replay_transactions_until` prefix replay) run many txs through one EVM
    /// without building receipts and must leave it off. (Replaced an earlier `!self.inspect` proxy
    /// that wrongly let the non-inspecting `replay_transactions_until` path store and panic.)
    cip64_store_enabled: bool,
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
            context_resolver: None,
            fee_context_cache_enabled: true,
            cip64_store_enabled: false,
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

    /// Configures this EVM for a receipt-building block executor — every consensus/replay path:
    /// block import, derivation, sequencing, kona proofs, and the dormant post-exec builders.
    /// Opts out of the block-start fee-context cache (`with_fee_context_cache_disabled`) and
    /// enables CIP-64 receipt storage (`with_cip64_store_enabled`) — the two `pub(crate)` setters
    /// below.
    ///
    /// These two flags are a matched pair and must always be set together: disabling the cache
    /// without enabling the store would drop CIP-64 receipt data, and enabling the store without
    /// disabling the cache would let a consensus executor consult the RPC-writable memo. This is
    /// the only cross-crate way to set either, so a new executor site cannot set one and forget
    /// the other. See the `fee_context_cache_enabled` / `cip64_store_enabled` field docs.
    #[must_use]
    pub const fn for_block_executor(self) -> Self {
        self.with_fee_context_cache_disabled().with_cip64_store_enabled()
    }

    /// Disables block-start fee-context pinning for this EVM. Prefer [`for_block_executor`], which
    /// sets this together with its matched CIP-64-store flag; this is `pub(crate)` so executor
    /// sites in other crates cannot set it in isolation. See the `fee_context_cache_enabled` field
    /// docs.
    ///
    /// [`for_block_executor`]: Self::for_block_executor
    #[must_use]
    pub(crate) const fn with_fee_context_cache_disabled(mut self) -> Self {
        self.fee_context_cache_enabled = false;
        self
    }

    /// Enables CIP-64 receipt-data storage for this EVM. Prefer [`for_block_executor`], which sets
    /// this together with its matched fee-context-cache flag; this is `pub(crate)` so executor
    /// sites in other crates cannot set it in isolation. See the `cip64_store_enabled` field docs.
    ///
    /// [`for_block_executor`]: Self::for_block_executor
    #[must_use]
    pub(crate) const fn with_cip64_store_enabled(mut self) -> Self {
        self.cip64_store_enabled = true;
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

        // The base-fee check is enabled during replay-style execution — sequencing, block import
        // / derivation re-execution, AND block tracing (`debug_trace*`, `trace_*`, `ots_*`) — and
        // disabled during call-style RPC simulation (`eth_call`, `eth_estimateGas`,
        // `debug_traceCall`). It is a defensive conjunct on the CIP-64 receipt-info store and
        // gates participation in the block-start fee-context cache below.
        let base_fee_check_enabled = !self.ctx().cfg.is_base_fee_check_disabled();

        // Block-start fee-currency context pinning for reth's per-tx `debug_trace*` replay EVMs
        // (see the `fee_context_cache` module docs for the full rationale). Gated on both:
        //   - `fee_context_cache_enabled`: consensus executors opt out at `create_executor`.
        //   - `base_fee_check_enabled`: call-style simulations (`eth_call`/`estimateGas`/
        //     `debug_traceCall`) load end-of-block rates, which are the intended semantics there.
        let use_fee_context_cache = self.fee_context_cache_enabled && base_fee_check_enabled;
        let block_number = self.ctx().block.number;
        if use_fee_context_cache
            && self.inner.fee_currency_context.updated_at_block != Some(block_number)
        {
            let number: u64 = block_number.saturating_to();
            // Genesis (block 0) has no parent to key on, and block-0 state *is* block-start state
            // (genesis carries no transactions), so the handler's fresh load is already correct —
            // pinning only applies from block 1 onward.
            if number > 0 {
                match self.db_mut().block_hash(number - 1) {
                    Ok(parent_hash) => {
                        if let Some(context) = self.fee_context_cache.get(number, parent_hash) {
                            // Memo hit: reuse the block-start context resolved for an earlier tx
                            // of this same block; `load_fee_currency_context` then short-circuits.
                            self.inner.fee_currency_context = context;
                        } else if let Some(context) = self
                            .context_resolver
                            .as_ref()
                            .and_then(|resolver| resolver.resolve(number, parent_hash))
                        {
                            // Memo miss: the trusted resolver computed the context from canonical
                            // state. Memoize it for the block's later per-tx EVMs, then seed it.
                            self.fee_context_cache.insert(number, parent_hash, context.clone());
                            self.inner.fee_currency_context = context;
                        } else {
                            // Unresolvable (no resolver, or a non-canonical/forged block, or
                            // pruned state): block-start rates can't be reconstructed from this
                            // mid-block per-tx EVM, and the rule is absolute — refuse rather than
                            // emit a silently-wrong trace against current-state rates.
                            return Err(EVMError::Custom(format!(
                                "CIP-64 block-start fee context unresolved for block {number}: \
                                 refusing to load current-state exchange rates (violates the \
                                 block-start-rates rule); ensure the node has this block's \
                                 canonical state and a fee-context resolver wired"
                            )));
                        }
                    }
                    Err(err) => {
                        // Couldn't determine the parent hash, so the block can be neither keyed
                        // nor resolved — no block-start source for this mid-block replay tx.
                        // Refuse rather than fall back to current-state rates.
                        return Err(EVMError::Database(err));
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
                // Hand this tx's CIP-64 pre/post transfer logs and `base_fee_in_erc20` to the
                // receipt builder. Store only on receipt-building EVMs, gated on both:
                //   - `cip64_store_enabled`: off for loose RPC trace/replay EVMs, which never build
                //     receipts and would fill the single slot twice (see the field docs).
                //   - `base_fee_check_enabled`: a defensive guard so call-style simulation EVMs
                //     (`disable_base_fee`) never store even if the flag were somehow set.
                // This keeps the slot-occupied panic a true signal of an executor double-store bug
                // (see `Cip64Storage` docs), not a false positive on legitimate tracing/replay.
                let cip64_info = self.inner.inner.0.ctx.tx.cip64_tx_info.take();
                if base_fee_check_enabled
                    && self.cip64_store_enabled
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
#[derive(Default, Clone)]
pub struct CeloEvmFactory {
    /// Shared fee currency blocklist. EVMs created by this factory *populate* this blocklist
    /// when a CIP-64 fee-currency debit/credit fails during execution, but only on the sequencing
    /// path (`CeloEvm::with_blocklist_enabled`); import/derivation EVMs leave it untouched. The
    /// sequencing-time payload filter (`CeloFeeCurrencyFilter` in `celo-reth`) reads it to skip
    /// such currencies. `transact_raw` itself never rejects blocklisted currencies. Defaults to
    /// empty.
    pub blocklist: FeeCurrencyBlocklist,
    /// Shared memo of block-start fee-currency contexts, consulted by the per-transaction trace
    /// EVMs reth's `debug_trace*` path builds so they validate CIP-64 fees against block-start
    /// exchange rates like consensus does. Filled from
    /// [`context_resolver`](Self::context_resolver) on a miss. See [`fee_context_cache`].
    ///
    /// An implementation detail, not configuration: nothing outside the crate inspects it, so it
    /// stays `pub(crate)` (wired by `Default`).
    pub(crate) fee_context_cache: FeeCurrencyContextCache,
    /// Trusted resolver that computes a block's block-start context from canonical state on a memo
    /// miss. Wired by the std-only node layer (`celo-reth`) via
    /// [`with_context_resolver`](Self::with_context_resolver); `None` on no-std consumers (kona).
    /// See [`FeeContextResolver`].
    pub(crate) context_resolver: Option<Arc<dyn FeeContextResolver>>,
}

impl core::fmt::Debug for CeloEvmFactory {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CeloEvmFactory")
            .field("blocklist", &self.blocklist)
            .field("fee_context_cache", &self.fee_context_cache)
            .field(
                "context_resolver",
                &self.context_resolver.as_ref().map(|_| "<dyn FeeContextResolver>"),
            )
            .finish()
    }
}

impl CeloEvmFactory {
    /// Sets the shared fee currency blocklist.
    pub fn with_blocklist(mut self, blocklist: FeeCurrencyBlocklist) -> Self {
        self.blocklist = blocklist;
        self
    }

    /// Installs the trusted block-start fee-context resolver. Called by the std-only node layer
    /// (`celo-reth`) with a provider-backed implementation; no-std consumers leave it unset. The
    /// resolver is cloned into every EVM this factory produces (as an `Arc`), and consulted only by
    /// pinning-enabled EVMs on a memo miss. See [`FeeContextResolver`] and [`fee_context_cache`].
    pub fn with_context_resolver(mut self, resolver: Arc<dyn FeeContextResolver>) -> Self {
        self.context_resolver = Some(resolver);
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
        // Tests inject a resolver directly where they need one.
        context_resolver: None,
        fee_context_cache_enabled: true,
        // Tests default to the receipt-building executor path (stores CIP-64 info); loose-EVM
        // tests flip this off to exercise the parity/ots/replay shapes.
        cip64_store_enabled: true,
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
            context_resolver: self.context_resolver.clone(),
            // On by default so the loose per-tx EVMs reth's RPC layer builds participate;
            // consensus executors flip it off in `create_executor` (see the field docs).
            fee_context_cache_enabled: true,
            // Off by default: only receipt-building executors store CIP-64 info, and they flip it
            // on in `create_executor`. Loose RPC EVMs (trace/ots/replay) must not store — the
            // single slot would be filled twice and panic (see the field docs).
            cip64_store_enabled: false,
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
    use alloy_primitives::{B256, TxKind};
    use celo_alloy_consensus::CeloTxType;
    use op_revm::{OpTransaction, transaction::deposit::DEPOSIT_TRANSACTION_TYPE};

    /// Test [`FeeContextResolver`] that returns a fixed context and counts calls, so a test can
    /// assert both the resolved value and *whether* the resolver was consulted.
    struct CountingResolver {
        context: celo_revm::FeeCurrencyContext,
        calls: Arc<spin::Mutex<u32>>,
    }
    impl FeeContextResolver for CountingResolver {
        fn resolve(
            &self,
            _block_number: u64,
            _parent_hash: B256,
        ) -> Option<celo_revm::FeeCurrencyContext> {
            *self.calls.lock() += 1;
            Some(self.context.clone())
        }
    }

    /// Builds a counting resolver returning `block_start_context(number, fc)`, plus its call
    /// counter.
    fn counting_resolver(
        number: u64,
        fc: Address,
    ) -> (Arc<dyn FeeContextResolver>, Arc<spin::Mutex<u32>>) {
        let calls = Arc::new(spin::Mutex::new(0u32));
        let resolver = Arc::new(CountingResolver {
            context: block_start_context(number, fc),
            calls: calls.clone(),
        });
        (resolver, calls)
    }

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

    /// Build a deposit `CeloTransaction<TxEnv>` from the given caller. Used to drive a block's
    /// block-start fee-context load with a transaction that succeeds regardless of fee currency.
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

    /// On a memo miss, `transact_raw` consults the trusted resolver, seeds its (canonical-derived)
    /// context, and memoizes it so the block's later per-tx EVMs reuse it without re-consulting
    /// the resolver.
    #[test]
    fn test_resolver_fills_memo_on_miss() {
        let fc = Address::with_last_byte(0xAA);
        let parent = test_db_parent_hash(5);
        let (resolver, calls) = counting_resolver(5, fc);

        // Empty memo -> resolver consulted, its context seeded and memoized.
        let mut evm = make_test_evm(FeeCurrencyBlocklist::default());
        evm.context_resolver = Some(resolver);
        evm.ctx_mut().block.number = U256::from(5);
        let _ = evm.transact_raw(make_cip64_tx(fc));
        assert_eq!(*calls.lock(), 1, "resolver must be consulted on a memo miss");
        assert_eq!(
            evm.inner.fee_currency_context.currency_exchange_rate(Some(fc)),
            Ok((U256::from(2), U256::from(1))),
            "the resolver's block-start context must be seeded into the EVM"
        );
        assert_eq!(
            evm.fee_context_cache.get(5, parent).expect("must be memoized").updated_at_block,
            Some(U256::from(5)),
        );

        // A fresh per-tx EVM sharing the memo hits it -- resolver NOT consulted again.
        let (resolver2, calls2) = counting_resolver(5, fc);
        let mut evm2 = make_test_evm(FeeCurrencyBlocklist::default());
        evm2.fee_context_cache = evm.fee_context_cache.clone();
        evm2.context_resolver = Some(resolver2);
        evm2.ctx_mut().block.number = U256::from(5);
        let _ = evm2.transact_raw(make_cip64_tx(fc));
        assert_eq!(*calls2.lock(), 0, "a memo hit must not re-consult the resolver");
        assert_eq!(
            evm2.inner.fee_currency_context.currency_exchange_rate(Some(fc)),
            Ok((U256::from(2), U256::from(1))),
        );
    }

    /// With no resolver wired (or one that returns `None`) and an empty memo, a pinning-enabled
    /// EVM at block > 0 must REFUSE rather than fall back to current-state (mid-block) rates: the
    /// block-start-rates rule is absolute and cannot be reconstructed from this per-tx EVM's own
    /// state. On a configured node the resolver answers for any canonical block; a `None` here
    /// means a non-canonical/forged block, pruned state, or a misconfigured node -- all of which
    /// must error, never silently trace against mid-block rates.
    #[test]
    fn test_unresolved_block_start_context_refuses() {
        // `make_test_evm` wires no resolver and starts with an empty memo; base fee enabled,
        // block > 0 -> the read path is unresolved.
        let mut evm = make_test_evm(FeeCurrencyBlocklist::default());
        evm.ctx_mut().block.number = U256::from(5);
        let result = evm.transact_raw(make_cip64_tx(Address::with_last_byte(0xAA)));
        assert!(
            matches!(result, Err(EVMError::Custom(_))),
            "an unresolved block-start context must refuse, not load current-state rates; \
             got {result:?}"
        );
    }

    /// Consensus executors (import/derivation/sequencing/proofs) opt out via
    /// `with_fee_context_cache_disabled` in `CeloBlockExecutorFactory::create_executor`: a
    /// disabled EVM must neither read the memo nor consult the resolver.
    #[test]
    fn test_disabled_evm_bypasses_pinning() {
        let fc = Address::with_last_byte(0xAA);
        let parent = test_db_parent_hash(5);

        // No read: a pre-memoized entry must not reach a disabled EVM.
        let cache = FeeCurrencyContextCache::default();
        cache.insert(5, parent, block_start_context(5, fc));
        let mut evm = make_test_evm(FeeCurrencyBlocklist::default());
        evm.fee_context_cache = cache;
        let mut evm = evm.with_fee_context_cache_disabled();
        evm.ctx_mut().block.number = U256::from(5);
        let _ = evm.transact_raw(make_cip64_tx(fc));
        assert!(
            evm.inner.fee_currency_context.currency_exchange_rate(Some(fc)).is_err(),
            "disabled EVM must load from its own DB state, not the shared memo"
        );

        // No resolve: a disabled EVM must not consult the resolver either.
        let (resolver, calls) = counting_resolver(5, fc);
        let mut evm =
            make_test_evm(FeeCurrencyBlocklist::default()).with_fee_context_cache_disabled();
        evm.context_resolver = Some(resolver);
        evm.ctx_mut().block.number = U256::from(5);
        let _ = evm.transact_raw(make_cip64_tx(fc));
        assert_eq!(*calls.lock(), 0, "disabled EVM must not consult the resolver");
    }

    /// Call-style simulations (`eth_call`/`eth_estimateGas`/`debug_traceCall` set
    /// `disable_base_fee`) run at end-of-block state where the rates they load are the intended
    /// semantics — they must neither read block-start rates from the memo nor consult the resolver.
    #[test]
    fn test_call_style_simulation_bypasses_pinning() {
        let fc = Address::with_last_byte(0xAA);

        // No read: a memoized rate must not reach the EVM under disable_base_fee.
        let cache = FeeCurrencyContextCache::default();
        cache.insert(5, test_db_parent_hash(5), block_start_context(5, fc));
        let mut evm = make_test_evm(FeeCurrencyBlocklist::default());
        evm.fee_context_cache = cache;
        evm.ctx_mut().block.number = U256::from(5);
        evm.ctx_mut().cfg.disable_base_fee = true;
        let _ = evm.transact_raw(make_cip64_tx(fc));
        assert!(
            evm.inner.fee_currency_context.currency_exchange_rate(Some(fc)).is_err(),
            "call-style simulation must load from its own DB state, not the memo"
        );

        // No resolve: the resolver must not be consulted under disable_base_fee.
        let (resolver, calls) = counting_resolver(5, fc);
        let mut evm = make_test_evm(FeeCurrencyBlocklist::default());
        evm.context_resolver = Some(resolver);
        evm.ctx_mut().block.number = U256::from(5);
        evm.ctx_mut().cfg.disable_base_fee = true;
        let _ = evm.transact_raw(make_cip64_tx(fc));
        assert_eq!(*calls.lock(), 0, "call-style simulation must not consult the resolver");
    }

    /// Genesis has no parent to key on — pinning is skipped (the `number > 0` gate), so the
    /// resolver is not consulted and the handler loads the context normally.
    #[test]
    fn test_genesis_block_skips_pinning() {
        let (resolver, calls) = counting_resolver(0, Address::with_last_byte(0xAA));
        let mut evm = make_test_evm(FeeCurrencyBlocklist::default());
        evm.context_resolver = Some(resolver);
        // make_test_evm defaults to block 0.
        evm.transact_raw(make_deposit_tx(Address::with_last_byte(0x01))).expect("deposit executes");
        assert_eq!(*calls.lock(), 0, "resolver must not be consulted at genesis");
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
    /// CIP-64 receipt data even on a store-enabled EVM: the `base_fee_check_enabled` defensive
    /// conjunct in `transact_raw` skips the store on those paths, which never build receipts.
    /// (The trace/replay paths are covered by
    /// [`test_loose_evm_replays_cip64_txs_without_storing`].)
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

    /// Regression: loose per-tx EVMs — parity `trace_*`, otterscan `ots_*`, and reth's
    /// `replay_transactions_until` prefix replay (`debug_traceTransaction`/`debug_traceCall`) —
    /// run many transactions through ONE EVM with the base-fee check ENABLED and never build
    /// receipts. They must not store CIP-64 receipt data: the single-slot `Cip64Storage` would be
    /// filled twice and `store_cip64_info` would panic on the second CIP-64 tx. The
    /// `cip64_store_enabled` flag (off for these EVMs) is what prevents the store.
    ///
    /// Both loose shapes are covered: `inspecting=false` is the non-inspecting
    /// `replay_transactions_until` EVM (the shape the earlier `!self.inspect` proxy failed to
    /// guard, so this exact case panicked); `inspecting=true` is the parity/ots trace EVM.
    #[test]
    fn test_loose_evm_replays_cip64_txs_without_storing() {
        use revm::state::AccountInfo;

        for inspecting in [false, true] {
            let mut evm = make_test_evm(FeeCurrencyBlocklist::default());
            evm.cip64_store_enabled = false; // loose RPC EVM (as `create_evm*` produces)
            evm.set_inspector_enabled(inspecting);

            let caller = Address::with_last_byte(0x01);
            evm.db_mut().insert_account_info(
                caller,
                AccountInfo { balance: U256::from(10u128.pow(20)), nonce: 0, ..Default::default() },
            );

            // Two successful native-fee CIP-64 txs through the same EVM (base fee enabled).
            // `transact_raw` does not commit, so the account nonce stays 0 and both nonce-0 txs
            // validate — enough to attempt the store twice. With the store gate mis-set this
            // panics on the second via the single-slot assertion; with the flag off it stores
            // nothing.
            for i in 0..2 {
                let mut tx = make_cip64_tx(Address::ZERO);
                tx.fee_currency = Some(Address::ZERO);
                let result = evm.transact_raw(tx);
                assert!(result.is_ok(), "loose replay tx {i} should succeed: {result:?}");
            }

            assert!(
                evm.cip64_storage.pop_cip64_receipt_data().is_none(),
                "loose replay EVM (inspecting={inspecting}) must not store CIP-64 receipt data"
            );
        }
    }

    /// The receipt-building executors (`CeloBlockExecutorFactory::create_executor`) set
    /// `cip64_store_enabled`, so a successful CIP-64 tx stores exactly one entry for
    /// `build_receipt` to pop.
    #[test]
    fn test_cip64_info_stored_on_executor_path() {
        use revm::state::AccountInfo;

        // `make_test_evm` sets `cip64_store_enabled = true` (the receipt-building path).
        let mut evm = make_test_evm(FeeCurrencyBlocklist::default());
        let caller = Address::with_last_byte(0x01);
        evm.db_mut().insert_account_info(
            caller,
            AccountInfo { balance: U256::from(10u128.pow(20)), nonce: 0, ..Default::default() },
        );

        let mut tx = make_cip64_tx(Address::ZERO);
        tx.fee_currency = Some(Address::ZERO);
        let result = evm.transact_raw(tx);
        assert!(result.is_ok(), "tx should succeed: {result:?}");

        assert!(
            evm.cip64_storage.pop_cip64_receipt_data().is_some(),
            "receipt-building executor must store CIP-64 receipt data"
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
