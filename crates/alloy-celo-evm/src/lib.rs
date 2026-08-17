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
        FEE_CREDIT_ERROR_PREFIX, FEE_CURRENCY_HALT_MARKER, FEE_CURRENCY_NOT_REGISTERED_PREFIX,
        FEE_CURRENCY_REVERT_MARKER, FEE_DEBIT_ERROR_PREFIX,
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
    /// calls *halted* while the node was building a block from its own mempool, so the sequencer
    /// can skip them for a while. Halts are the only failures that blocklist: contract *reverts*
    /// are ambiguous (canonically an underfunded sender), and EVM-level call errors are the
    /// node's own infrastructure faults — neither is evidence against the currency. It must
    /// therefore only be touched on the sequencing path.
    /// Block import and derivation re-execute already-canonical blocks and must produce identical
    /// results regardless of this node's accumulated heuristic, so they leave it alone entirely.
    ///
    /// EVMs are created with this `false` by default ([`CeloEvmFactory::create_evm`], used by the
    /// import/derivation executor and RPC). It is flipped to `true` only by the sequencing-side
    /// builders — `CeloEvmConfig::builder_for_next_block` (the payload-builder entry point) and
    /// its dormant post-exec sibling — which import/derivation deliberately bypass.
    blocklist_enabled: bool,
    /// Whether this EVM stores CIP-64 receipt data into its [`Cip64Storage`] after each
    /// transaction.
    ///
    /// The store hands a tx's pre/post transfer logs and `base_fee_in_erc20` to the receipt
    /// builder, which pops exactly one entry per CIP-64 transaction in `build_receipt`. The slot
    /// holds one entry and `store_cip64_info` panics on a second store, so only EVMs that build
    /// receipts may store.
    ///
    /// EVMs are created with this `false` by default ([`CeloEvmFactory::create_evm`]); it is
    /// flipped to `true` only for receipt-building executors:
    /// [`CeloBlockExecutorFactory::create_executor`](block::CeloBlockExecutorFactory) — which
    /// import, derivation, sequencing and kona proofs all go through — plus celo-reth's two
    /// dormant post-exec block builders, which build receipts outside `create_executor`. The
    /// RPC layer builds loose per-tx EVMs — parity `trace_*`, otterscan `ots_*`, and
    /// `replay_transactions_until` — that run a whole block through one EVM without building
    /// receipts, and leave it off.
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

    /// Creates a FeeCurrencyContext from the current EVM state.
    pub fn create_fee_currency_context(&mut self) -> celo_revm::FeeCurrencyContext
    where
        I: Inspector<CeloContext<DB>>,
        P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
    {
        celo_revm::FeeCurrencyContext::new_from_evm(&mut self.inner)
    }

    /// Replaces the EVM's fee currency context, e.g. with a block-start context captured before
    /// simulating a call at a mid-block position. The context's `updated_at_block` stamp makes
    /// the handler skip its lazy per-block load as long as the block environment matches.
    pub fn set_fee_currency_context(
        &mut self,
        fee_currency_context: celo_revm::FeeCurrencyContext,
    ) {
        self.inner.fee_currency_context = fee_currency_context;
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

    /// Enables CIP-64 receipt-data storage for this EVM. Only receipt-building executors may call
    /// this; see the `cip64_store_enabled` field docs.
    #[must_use]
    pub const fn with_cip64_store_enabled(mut self) -> Self {
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
        // / derivation re-execution, AND block tracing (`debug_traceTransaction`,
        // `debug_traceBlock*`, parity `trace_*`, `ots_*`) — and disabled during call-style RPC
        // simulation (`eth_call`, `eth_estimateGas`, `debug_traceCall`).
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
                // Hand this tx's pre/post transfer logs and `base_fee_in_erc20` to the receipt
                // builder, which pops one entry per CIP-64 transaction in `build_receipt`. Only
                // receipt-building executors set `cip64_store_enabled` (see its field docs);
                // confining the store to them keeps the slot-occupied panic in `store_cip64_info`
                // a true signal of an executor double-store bug rather than a false positive on
                // RPC replay. The store must NOT additionally require the base-fee check:
                // `eth_simulateV1` (default `validation=false`) disables that check on a
                // receipt-building executor, and `build_cip64_receipt` asserts that a successful
                // CIP-64 tx has stored data — skipping the store there panics at receipt build.
                let cip64_info = self.inner.inner.0.ctx.tx.cip64_tx_info.take();
                if self.cip64_store_enabled
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
                    // ORDER MATTERS: the revert arm must be checked first. A revert
                    // message embeds attacker-controlled bytes (the decoded
                    // `Error(string)` payload), so a sender could revert with the
                    // literal halt-marker text; checking halt first would let that
                    // spoof a "currency fault" and blocklist a healthy currency.
                    // The genuine markers are prepended by `process_call_result`
                    // before any contract output, and halt reasons carry no
                    // attacker bytes, so revert-first is spoof-proof both ways.
                    if err_msg.contains(FEE_CURRENCY_REVERT_MARKER) {
                        // The fee-currency contract *reverted* the debit/credit.
                        // Canonically that is a sender (`ERC20: transfer amount
                        // exceeds balance`) who was funded at pool admission but
                        // drained afterwards — but a paused or blacklisting token
                        // reverts the same way, so a revert is ambiguous and
                        // insufficient evidence to blocklist a whole currency.
                        // The tx is dropped from the payload either way;
                        // blocklisting here let a single underfunded sender
                        // suppress an entire healthy currency until the
                        // blocklist's timed expiry (`BLOCKLIST_EVICTION_SECONDS`,
                        // 2h) or a manual `admin_unblockFeeCurrency`.
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
                    } else if err_msg.contains(FEE_CURRENCY_HALT_MARKER) {
                        // Halt (e.g. the debit exhausted its gas budget, or the
                        // contract executed invalid bytecode) — the one failure
                        // that is unambiguously the currency's fault: blocklist
                        // so the payload builder stops retrying every tx of this
                        // currency.
                        tracing::warn!(
                            target: "celo",
                            "fee-currency debit/credit halted for {fc}: {e} — blocklisting"
                        );
                        // The one arm that blocklists: meter it so a blocklist
                        // addition is alertable on its own, not only via the
                        // downstream `reason="blocklisted"` skips that fire
                        // only while further txs of this currency arrive.
                        #[cfg(feature = "std")]
                        metrics::counter!(
                            "celo_payload_skipped_total",
                            "reason" => "debit_credit_halted"
                        )
                        .increment(1);
                        let block_timestamp: u64 = self.ctx().block.timestamp.to();
                        self.blocklist.block_currency(fc, block_timestamp);
                    } else {
                        // Neither marker: the system call itself errored — an
                        // EVM-infrastructure failure (`CoreContractError::Evm`,
                        // e.g. a database read failing mid-call). That is this
                        // node's fault, not the currency's; blocklisting here
                        // would dark-list a healthy currency for 2h over a local
                        // I/O hiccup.
                        tracing::warn!(
                            target: "celo",
                            "fee-currency debit/credit failed with an EVM-level error for \
                             {fc}: {e} — dropping tx without blocklisting the currency"
                        );
                        #[cfg(feature = "std")]
                        metrics::counter!(
                            "celo_payload_skipped_total",
                            "reason" => "debit_credit_evm_error"
                        )
                        .increment(1);
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

/// Creates a [`CeloEvm`] for testing over the given database.
#[cfg(test)]
fn make_test_evm_with_db<DB: Database>(
    db: DB,
    blocklist: FeeCurrencyBlocklist,
) -> CeloEvm<DB, revm::inspector::NoOpInspector> {
    let spec_id = OpSpecId::FJORD;
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
        // Tests here exercise the sequencing-path blocklist behaviour, so enable it.
        blocklist_enabled: true,
        // Default to the receipt-building executor path; loose-EVM tests build through the
        // factory instead (`make_loose_test_evm`).
        cip64_store_enabled: true,
    }
}

/// Creates a [`CeloEvm`] for testing with an in-memory database.
#[cfg(test)]
fn make_test_evm(
    blocklist: FeeCurrencyBlocklist,
) -> CeloEvm<revm::database::InMemoryDB, revm::inspector::NoOpInspector> {
    make_test_evm_with_db(revm::database::InMemoryDB::default(), blocklist)
}

/// Creates a loose RPC-style [`CeloEvm`] through the factory, the way reth's RPC layer does:
/// non-inspecting via [`CeloEvmFactory::create_evm`] (the `replay_transactions_until` shape),
/// inspecting via [`CeloEvmFactory::create_evm_with_inspector`] (the parity/ots trace EVM).
///
/// Asserts that the factory leaves the CIP-64 store off — the invariant the loose-EVM tests
/// rest on. Were `build_evm` ever to default it on, hand-flagged test EVMs would keep passing
/// while every production trace/replay double-stored; building through the factory pins the
/// default itself.
#[cfg(test)]
fn make_loose_test_evm(
    inspecting: bool,
) -> CeloEvm<revm::database::InMemoryDB, revm::inspector::NoOpInspector, PrecompilesMap> {
    let factory = CeloEvmFactory::default();
    let db = revm::database::InMemoryDB::default();
    let mut env = EvmEnv::<OpSpecId>::default();
    env.cfg_env.chain_id = 42220;
    env.cfg_env.spec = OpSpecId::FJORD;
    let evm = if inspecting {
        factory.create_evm_with_inspector(db, env, revm::inspector::NoOpInspector {})
    } else {
        factory.create_evm(db, env)
    };
    assert!(
        !evm.cip64_store_enabled,
        "factory-built EVMs must leave CIP-64 receipt-data storage disabled"
    );
    evm
}

/// Registers `fee_currency` in the EVM's fee-currency context at `rate` units per CELO.
///
/// Pinning `updated_at_block` to the EVM's block number keeps `load_fee_currency_context`
/// from reloading the context from the (empty) test state on the first transaction.
#[cfg(test)]
fn register_fee_currency<P>(
    evm: &mut CeloEvm<revm::database::InMemoryDB, revm::inspector::NoOpInspector, P>,
    fee_currency: Address,
    rate: u64,
) {
    let mut currencies = alloy_primitives::map::HashMap::default();
    currencies.insert(
        fee_currency,
        celo_revm::fee_currency_context::FeeCurrencyInfo {
            exchange_rate: (U256::from(rate), U256::from(1u64)),
            intrinsic_gas: 0,
        },
    );
    let block_number = evm.ctx().block.number;
    evm.inner.fee_currency_context =
        celo_revm::FeeCurrencyContext::new(currencies, Some(block_number));
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
            // Off by default; `create_executor` flips it on for receipt-building executors.
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
    use alloc::{string::String, vec::Vec};
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

    /// Put the given sequencing-mode EVM in block-building mode, register `fc`
    /// in the per-block fee-currency context, and run a CIP-64 tx through
    /// `transact_raw`, driving the `debitGasFees` system call against whatever
    /// state the EVM's database holds. Returns the resulting error, stringified.
    fn run_cip64_debit<DB: Database>(
        evm: &mut CeloEvm<DB, revm::inspector::NoOpInspector>,
        fc: Address,
    ) -> String
    where
        CeloPrecompiles: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
    {
        use celo_revm::fee_currency_context::FeeCurrencyInfo;

        // Non-zero basefee puts the EVM in block-building mode (apply_blocklist on).
        evm.ctx_mut().block.basefee = 1_000_000_000;
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

    /// Run a CIP-64 tx through a sequencing-mode EVM whose fee currency `fc`
    /// is registered in the per-block context and backed by `code` at the
    /// token address, so the `debitGasFees` system call genuinely executes
    /// that bytecode. Returns the resulting error, stringified.
    fn transact_cip64_with_token_code(
        blocklist: FeeCurrencyBlocklist,
        fc: Address,
        code: Bytes,
    ) -> String {
        use revm::state::{AccountInfo, Bytecode};

        let mut evm = make_test_evm(blocklist);
        evm.db_mut().insert_account_info(fc, AccountInfo::from_bytecode(Bytecode::new_raw(code)));
        run_cip64_debit(&mut evm, fc)
    }

    /// A fee-currency contract that *reverts* the debit — canonically an
    /// underfunded sender's `ERC20: transfer amount exceeds balance` — is
    /// not sufficient evidence of a currency fault. The tx is dropped from
    /// the payload either way, but the currency must NOT be blocklisted:
    /// otherwise a single underfunded sender suppresses every tx of a healthy
    /// currency for the blocklist's whole 2h expiry period while this node
    /// sequences.
    #[test]
    fn test_debit_revert_does_not_blocklist_currency() {
        let fc = Address::with_last_byte(0xD0);
        let blocklist = FeeCurrencyBlocklist::default();
        // PUSH1 0, PUSH1 0, REVERT — the debit call reverts.
        let err = transact_cip64_with_token_code(
            blocklist.clone(),
            fc,
            Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xfd]),
        );
        assert!(err.contains(FEE_DEBIT_ERROR_PREFIX), "expected a debit failure, got: {err}");
        assert!(
            !blocklist.is_blocked(fc),
            "a debit revert is ambiguous (canonically a sender fault) and must not blocklist \
             the currency; got error: {err}"
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
        let err = transact_cip64_with_token_code(
            blocklist.clone(),
            fc,
            Bytes::from_static(&[0x5b, 0x60, 0x00, 0x56]),
        );
        assert!(err.contains(FEE_DEBIT_ERROR_PREFIX), "expected a debit failure, got: {err}");
        assert!(err.contains(FEE_CURRENCY_HALT_MARKER), "expected a halt failure, got: {err}");
        assert!(
            blocklist.is_blocked(fc),
            "a debit halt (out-of-gas) is a currency fault and must blocklist; got error: {err}"
        );
    }

    /// A fee-currency contract that debits fine but *halts* the post-execution
    /// `creditGasFees` refund is just as much a currency fault as a debit halt:
    /// the credit failure must flatten through op-revm's error flow with the
    /// credit prefix + halt marker and blocklist the currency. Pinned as a unit
    /// test because the credit path re-enters via a different handler hook than
    /// the debit — a revm/op-revm bump could break its error flattening while
    /// the debit-path tests stay green.
    #[test]
    fn test_credit_halt_still_blocklists_currency() {
        let fc = Address::with_last_byte(0xD4);
        let blocklist = FeeCurrencyBlocklist::default();
        // Branch on calldata size: `balanceOf(address)` (36 bytes) and
        // `debitGasFees(address,uint256)` (68 bytes) both return
        // `type(uint256).max` — an unbounded balance for the max-fee check, and
        // ignored output for the void debit; `creditGasFees` calls carry 260
        // bytes → jump into an infinite loop → OOG halt on the credit.
        //   PUSH1 0x64, CALLDATASIZE, GT, PUSH1 0x12, JUMPI,
        //   PUSH1 0, NOT, PUSH1 0, MSTORE, PUSH1 0x20, PUSH1 0, RETURN,
        //   JUMPDEST, PUSH1 0x12, JUMP
        let err = transact_cip64_with_token_code(
            blocklist.clone(),
            fc,
            Bytes::from_static(&[
                0x60, 0x64, 0x36, 0x11, 0x60, 0x12, 0x57, 0x60, 0x00, 0x19, 0x60, 0x00, 0x52, 0x60,
                0x20, 0x60, 0x00, 0xf3, 0x5b, 0x60, 0x12, 0x56,
            ]),
        );
        assert!(err.contains(FEE_CREDIT_ERROR_PREFIX), "expected a credit failure, got: {err}");
        assert!(
            !err.contains(FEE_DEBIT_ERROR_PREFIX),
            "the debit must succeed so the failure is attributable to the credit: {err}"
        );
        assert!(err.contains(FEE_CURRENCY_HALT_MARKER), "expected a halt failure, got: {err}");
        assert!(
            blocklist.is_blocked(fc),
            "a credit halt (out-of-gas) is a currency fault and must blocklist; got error: {err}"
        );
    }

    /// A revert whose `Error(string)` payload contains the literal halt-marker
    /// text must still classify as a revert and must NOT blocklist. The revert
    /// message is the one attacker-controlled string in the flattened error, so
    /// if the classifier checked the halt marker first, a sender could spoof a
    /// "currency fault" and dark-list a healthy currency at will.
    #[test]
    fn test_spoofed_halt_marker_in_revert_does_not_blocklist() {
        let fc = Address::with_last_byte(0xD3);
        let blocklist = FeeCurrencyBlocklist::default();

        // ABI-encode `Error(string)` carrying the halt-marker text as revert data.
        let msg = FEE_CURRENCY_HALT_MARKER.as_bytes();
        let mut revert_data = Vec::new();
        revert_data.extend_from_slice(&[0x08, 0xc3, 0x79, 0xa0]); // Error(string) selector
        revert_data.extend_from_slice(&U256::from(0x20).to_be_bytes::<32>()); // string offset
        revert_data.extend_from_slice(&U256::from(msg.len()).to_be_bytes::<32>()); // string length
        revert_data.extend_from_slice(msg);
        revert_data.resize(revert_data.len().div_ceil(32) * 32, 0); // right-pad to a word

        // CODECOPY the blob (at code offset 12, right after these 12 opcode bytes)
        // into memory and REVERT with it.
        let len = u8::try_from(revert_data.len()).expect("revert data fits one PUSH1");
        let mut code = alloc::vec![
            0x60, len, // PUSH1 len
            0x60, 0x0c, // PUSH1 12 (data offset within the code)
            0x60, 0x00, // PUSH1 0  (memory destination)
            0x39, // CODECOPY
            0x60, len, // PUSH1 len
            0x60, 0x00, // PUSH1 0
            0xfd, // REVERT
        ];
        code.extend_from_slice(&revert_data);

        let err = transact_cip64_with_token_code(blocklist.clone(), fc, code.into());
        assert!(
            err.contains(FEE_CURRENCY_REVERT_MARKER),
            "expected a genuine revert classification, got: {err}"
        );
        assert!(
            err.contains(FEE_CURRENCY_HALT_MARKER),
            "the spoofed halt marker should survive into the decoded revert message: {err}"
        );
        assert!(
            !blocklist.is_blocked(fc),
            "attacker-controlled revert text must not be able to spoof a halt and blocklist; \
             got error: {err}"
        );
    }

    /// A debit failure carrying neither the revert nor the halt marker is an
    /// EVM-infrastructure error (`CoreContractError::Evm`, e.g. a database read
    /// failing mid-call) — the node's fault, not the currency's — and must not
    /// blocklist. It must instead be metered as
    /// `celo_payload_skipped_total{reason=debit_credit_evm_error}`.
    #[cfg(feature = "std")]
    #[test]
    fn test_debit_evm_error_does_not_blocklist_currency() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        use revm::state::{AccountInfo, Bytecode};

        #[derive(Debug)]
        struct TestDbError;
        impl core::fmt::Display for TestDbError {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str("test db error")
            }
        }
        impl core::error::Error for TestDbError {}
        impl revm::database_interface::DBErrorMarker for TestDbError {}

        /// Delegates to an [`revm::database::InMemoryDB`] but fails every storage
        /// read of `fail_addr`, simulating a state-provider I/O error surfacing
        /// mid-debit (and only there — unrelated reads keep working so the tx
        /// genuinely reaches the debit system call).
        #[derive(Debug)]
        struct FailingStorageDb {
            inner: revm::database::InMemoryDB,
            fail_addr: Address,
        }
        impl revm::Database for FailingStorageDb {
            type Error = TestDbError;
            fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
                Ok(revm::Database::basic(&mut self.inner, address).unwrap())
            }
            fn code_by_hash(
                &mut self,
                code_hash: alloy_primitives::B256,
            ) -> Result<Bytecode, Self::Error> {
                Ok(revm::Database::code_by_hash(&mut self.inner, code_hash).unwrap())
            }
            fn storage(
                &mut self,
                address: Address,
                index: revm::primitives::StorageKey,
            ) -> Result<revm::primitives::StorageValue, Self::Error> {
                if address == self.fail_addr {
                    return Err(TestDbError);
                }
                Ok(revm::Database::storage(&mut self.inner, address, index).unwrap())
            }
            fn block_hash(&mut self, number: u64) -> Result<alloy_primitives::B256, Self::Error> {
                Ok(revm::Database::block_hash(&mut self.inner, number).unwrap())
            }
        }

        let fc = Address::with_last_byte(0xD2);
        let blocklist = FeeCurrencyBlocklist::default();
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        // Token code: PUSH1 0, SLOAD, STOP — the debit call reads the token's
        // storage, which the wrapper DB fails with a genuine database error.
        let mut inner = revm::database::InMemoryDB::default();
        inner.insert_account_info(
            fc,
            AccountInfo::from_bytecode(Bytecode::new_raw(Bytes::from_static(&[
                0x60, 0x00, 0x54, 0x00,
            ]))),
        );

        let err = metrics::with_local_recorder(&recorder, || {
            let mut evm =
                make_test_evm_with_db(FailingStorageDb { inner, fail_addr: fc }, blocklist.clone());
            run_cip64_debit(&mut evm, fc)
        });

        assert!(err.contains(FEE_DEBIT_ERROR_PREFIX), "expected a debit failure, got: {err}");
        assert!(
            !err.contains(FEE_CURRENCY_REVERT_MARKER) && !err.contains(FEE_CURRENCY_HALT_MARKER),
            "a database error must carry neither contract-fault marker, got: {err}"
        );
        assert!(
            !blocklist.is_blocked(fc),
            "an EVM-infrastructure error is the node's fault and must not blocklist the \
             currency; got error: {err}"
        );

        let skipped: u64 = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(ck, _, _, _)| {
                ck.key().name() == "celo_payload_skipped_total"
                    && ck
                        .key()
                        .labels()
                        .any(|l| l.key() == "reason" && l.value() == "debit_credit_evm_error")
            })
            .map(|(_, _, _, v)| match v {
                DebugValue::Counter(c) => c,
                other => panic!("expected a counter, got {other:?}"),
            })
            .sum();
        assert_eq!(skipped, 1, "celo_payload_skipped_total must increment exactly once");
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

    /// Regression: a store-enabled executor with the base-fee check DISABLED must still store
    /// CIP-64 receipt data. This is the `eth_simulateV1` shape (default `validation=false`):
    /// reth builds a receipt-building block executor via `create_block_builder` →
    /// `create_executor` but sets `disable_base_fee`, and `build_cip64_receipt` asserts that a
    /// successful CIP-64 tx has stored data — a base-fee conjunct on the store gate would panic
    /// there. Call-style simulation (`eth_call` / `eth_estimateGas`) uses loose store-disabled
    /// EVMs and is covered by [`test_loose_evm_replays_cip64_txs_without_storing`].
    ///
    /// The handler populates `cip64_tx_info` for native-fee CIP-64 txs
    /// (`feeCurrency == 0x0`) even when the base fee is disabled, so the tx
    /// below reaches the store gate.
    #[test]
    fn test_cip64_info_stored_when_base_fee_check_disabled() {
        use revm::state::AccountInfo;

        let blocklist = FeeCurrencyBlocklist::default();
        let mut evm = make_test_evm(blocklist);

        // Fund the caller so the balance check passes during simulated execution.
        let caller = Address::with_last_byte(0x01);
        evm.db_mut().insert_account_info(
            caller,
            AccountInfo { balance: U256::from(10u128.pow(20)), nonce: 0, ..Default::default() },
        );

        // eth_simulateV1 validation=false mode.
        evm.ctx_mut().cfg.disable_base_fee = true;

        let mut tx = make_cip64_tx(Address::ZERO);
        tx.fee_currency = Some(Address::ZERO);
        let result = evm.transact_raw(tx);
        assert!(result.is_ok(), "simulated tx should succeed: {result:?}");

        assert!(
            evm.cip64_storage.pop_cip64_receipt_data().is_some(),
            "store-enabled simulate executor must store CIP-64 receipt data"
        );
    }

    /// The same `eth_simulateV1` shape, but paying in a real ERC20 fee currency.
    ///
    /// Dropping the base-fee conjunct from the store gate is not enough on its own here:
    /// disabling the base-fee check also disables the ERC20 debit, which is the only other
    /// writer of `cip64_tx_info`, so the tx succeeded with `None` to store and
    /// `build_cip64_receipt`'s "succeeded but no receipt data" assert panicked. The handler now
    /// stores a minimal `Cip64Info` whenever the debit is skipped.
    ///
    /// Also pins the stored base fee to the *converted* rate rather than the native base fee.
    #[test]
    fn test_cip64_info_stored_for_erc20_fee_currency_when_base_fee_check_disabled() {
        use revm::state::AccountInfo;

        const BALANCE: u128 = 10u128.pow(20);

        let mut evm = make_test_evm(FeeCurrencyBlocklist::default());
        let caller = Address::with_last_byte(0x01);
        evm.db_mut().insert_account_info(
            caller,
            AccountInfo { balance: U256::from(BALANCE), nonce: 0, ..Default::default() },
        );

        let fee_currency = Address::with_last_byte(0xAB);
        register_fee_currency(&mut evm, fee_currency, 2);

        const BASEFEE: u64 = 1_000_000_000;
        evm.ctx_mut().block.basefee = BASEFEE;
        // eth_simulateV1 validation=false mode.
        evm.ctx_mut().cfg.disable_base_fee = true;

        let mut tx = make_cip64_tx(fee_currency);
        // Enough gas for the call to succeed: the receipt assert only fires on success.
        tx.op_tx.base.gas_limit = 100_000;
        let result = evm.transact_raw(tx).expect("simulated tx should not be rejected");
        assert!(result.result.is_success(), "simulated tx should succeed: {:?}", result.result);

        let stored = evm
            .cip64_storage
            .pop_cip64_receipt_data()
            .expect("store-enabled simulate executor must store CIP-64 receipt data");
        assert_eq!(stored.fee_currency, Some(fee_currency));
        assert_eq!(
            stored.cip64_info.base_fee_in_erc20,
            Some(u128::from(BASEFEE) * 2),
            "stored base fee must be denominated in the fee currency, not native CELO"
        );

        // The other half of the fix: the entry is the *minimal* one, written because the debit
        // was skipped. If a future change lets the debit run on this path these stop being
        // empty/zero — and the entry would then be the debit's, not this arm's.
        let info = &stored.cip64_info;
        assert!(
            info.logs_pre.is_empty() && info.logs_post.is_empty(),
            "no debit/credit system call ran, so there are no transfer logs to merge"
        );
        assert_eq!(
            (
                info.debit_gas_used,
                info.debit_gas_refunded,
                info.credit_gas_used,
                info.credit_gas_refunded
            ),
            (0, 0, 0, 0),
            "no debit/credit system call ran, so there is no system-call gas to account for"
        );
        // ...and the caller was not charged in CELO either: an ERC20-fee tx pays no native gas.
        assert_eq!(
            result.state.get(&caller).expect("caller is touched by the tx").info.balance,
            U256::from(BALANCE),
            "an ERC20-fee CIP-64 tx must not be charged native gas"
        );
    }

    /// The debit is also what denominates `effective_gas_price`, so with it skipped `GASPRICE`
    /// inside a simulated ERC20-fee tx used to read the *native* price while the receipt
    /// reported a fee-currency base fee. The handler now sets the price too.
    ///
    /// `max_fee_per_gas` is raised above both base fees so the tip — not the cap — decides the
    /// effective price; otherwise the native and fee-currency answers are both the cap and the
    /// assertion could not tell them apart.
    #[test]
    fn test_erc20_fee_simulation_denominates_gasprice() {
        use revm::state::{AccountInfo, Bytecode};

        const BASEFEE: u64 = 1_000_000_000;
        const RATE: u64 = 2;
        /// `make_cip64_tx`'s `gas_priority_fee`.
        const PRIORITY_FEE: u128 = 100;

        let mut evm = make_test_evm(FeeCurrencyBlocklist::default());
        let caller = Address::with_last_byte(0x01);
        evm.db_mut().insert_account_info(
            caller,
            AccountInfo { balance: U256::from(10u128.pow(20)), nonce: 0, ..Default::default() },
        );

        // A callee that returns GASPRICE: GASPRICE; PUSH0; MSTORE; PUSH1 0x20; PUSH0; RETURN.
        // Not `make_cip64_tx`'s default 0x02 target — that address is the SHA-256 precompile,
        // which shadows any code installed there.
        let callee = Address::with_last_byte(0xC0);
        let code =
            Bytecode::new_raw(Bytes::from_static(&[0x3a, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3]));
        evm.db_mut().insert_account_info(
            callee,
            AccountInfo { code_hash: code.hash_slow(), code: Some(code), ..Default::default() },
        );

        let fee_currency = Address::with_last_byte(0xAB);
        register_fee_currency(&mut evm, fee_currency, RATE);
        evm.ctx_mut().block.basefee = BASEFEE;
        // eth_simulateV1 validation=false mode.
        evm.ctx_mut().cfg.disable_base_fee = true;

        let mut tx = make_cip64_tx(fee_currency);
        tx.op_tx.base.kind = TxKind::Call(callee);
        tx.op_tx.base.gas_limit = 100_000;
        tx.op_tx.base.gas_price = 10 * u128::from(BASEFEE);
        let result = evm.transact_raw(tx).expect("simulated tx should not be rejected");
        let output = result.result.output().expect("callee returns GASPRICE");

        assert_eq!(
            U256::from_be_slice(output),
            U256::from(u128::from(BASEFEE) * u128::from(RATE) + PRIORITY_FEE),
            "GASPRICE in an ERC20-fee simulation must be denominated in the fee currency"
        );
    }

    /// The other shape reaching the minimal-`Cip64Info` arm with an ERC20 fee currency:
    /// `eth_call` / `eth_estimateGas`. Those disable the base-fee check just like
    /// `eth_simulateV1`, so the arm writes `cip64_tx_info` — but they run on loose,
    /// store-disabled EVMs, where `transact_raw` takes the field and drops it.
    ///
    /// Pins that nothing reaches the single-slot `Cip64Storage`: two ERC20-fee CIP-64 calls
    /// through one EVM would otherwise trip `store_cip64_info`'s slot-occupied panic.
    /// [`test_loose_evm_replays_cip64_txs_without_storing`] covers the same invariant for
    /// native-fee txs with the base-fee check left on.
    #[test]
    fn test_loose_evm_replays_erc20_fee_calls_without_storing() {
        use revm::state::AccountInfo;

        let mut evm = make_loose_test_evm(false);

        let caller = Address::with_last_byte(0x01);
        evm.db_mut().insert_account_info(
            caller,
            AccountInfo { balance: U256::from(10u128.pow(20)), nonce: 0, ..Default::default() },
        );

        let fee_currency = Address::with_last_byte(0xAB);
        register_fee_currency(&mut evm, fee_currency, 2);
        evm.ctx_mut().block.basefee = 1_000_000_000;
        // eth_call / eth_estimateGas mode.
        evm.ctx_mut().cfg.disable_base_fee = true;

        // `transact_raw` does not commit, so the nonce stays 0 and both nonce-0 txs validate —
        // enough to attempt the store twice.
        for i in 0..2 {
            let mut tx = make_cip64_tx(fee_currency);
            tx.op_tx.base.gas_limit = 100_000;
            let result = evm.transact_raw(tx);
            assert!(result.is_ok(), "call-shape tx {i} should succeed: {result:?}");
        }

        assert!(
            evm.cip64_storage.pop_cip64_receipt_data().is_none(),
            "loose call EVM must not store CIP-64 receipt data"
        );
    }

    /// Regression: loose per-tx EVMs — parity `trace_*`, otterscan `ots_*`, and reth's
    /// `replay_transactions_until` prefix replay — run many transactions through ONE EVM with the
    /// base-fee check ENABLED and never build receipts, so they must not store CIP-64 receipt
    /// data: the single-slot `Cip64Storage` would be filled twice and panic on the second CIP-64
    /// tx. Both shapes are covered — `inspecting=false` is `replay_transactions_until`,
    /// `inspecting=true` the parity/ots trace EVM.
    #[test]
    fn test_loose_evm_replays_cip64_txs_without_storing() {
        use revm::state::AccountInfo;

        for inspecting in [false, true] {
            let mut evm = make_loose_test_evm(inspecting);

            let caller = Address::with_last_byte(0x01);
            evm.db_mut().insert_account_info(
                caller,
                AccountInfo { balance: U256::from(10u128.pow(20)), nonce: 0, ..Default::default() },
            );

            // Two native-fee CIP-64 txs through the same EVM. `transact_raw` does not commit, so
            // the nonce stays 0 and both nonce-0 txs validate — enough to attempt the store twice.
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
