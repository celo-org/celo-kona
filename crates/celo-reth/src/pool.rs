//! Celo fee-currency-aware transaction pool types.
//!
//! Provides [`CeloPoolTx`], a wrapper around [`OpPooledTransaction`] that converts
//! CIP-64 fee-currency gas prices to native equivalents. This ensures the pool's
//! pending/queued classification and replacement logic work correctly for transactions
//! that pay fees in non-native currencies.

use crate::primitives::CeloTransactionSigned;
use alloy_consensus::Transaction;
use alloy_eips::{
    Typed2718, eip2930::AccessList, eip4844::BlobTransactionValidationError,
    eip7594::BlobTransactionSidecarVariant,
};
use alloy_primitives::{Address, B256, Bytes, TxHash, TxKind, U256};
use celo_alloy_consensus::CeloPooledTransaction;
use celo_revm::{
    non_native_fee_currency,
    units::{Fc, Native, NativeU256},
};
use op_revm::OpSpecId;
use reth_optimism_txpool::{
    OpPooledTransaction, OpPooledTx, conditional::MaybeConditionalTransaction,
    estimated_da_size::DataAvailabilitySized, interop::MaybeInteropTransaction,
};
use reth_primitives_traits::{InMemorySize, Recovered, SealedBlock};
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::{
    EthBlobTransactionSidecar, EthPoolTransaction, PoolTransaction, TransactionPool,
    TransactionValidationOutcome, TransactionValidator, ValidPoolTransaction,
    error::{InvalidPoolTransactionError, PoolTransactionError},
};
use revm::{interpreter::gas::calculate_initial_tx_gas, primitives::hardfork::SpecId};
use std::{
    borrow::Cow,
    fmt::Debug,
    sync::{Arc, Mutex, OnceLock},
};

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// Prometheus metrics for the Celo pool validator.
///
/// All counters use the `celo_pool_` prefix.
struct CeloPoolMetrics;

impl CeloPoolMetrics {
    fn cip64_rejection(reason: &'static str) {
        metrics::counter!("celo_pool_cip64_rejections_total", "reason" => reason).increment(1);
    }
    fn cip64_accepted() {
        metrics::counter!("celo_pool_cip64_accepted_total").increment(1);
    }
    fn exchange_rate_lookup() {
        metrics::counter!("celo_pool_exchange_rate_lookups_total").increment(1);
    }
    fn pool_eviction(count: u64) {
        metrics::counter!("celo_pool_maintainer_evictions_total").increment(count);
    }
    fn maintainer_failure() {
        metrics::counter!("celo_pool_maintainer_failures_total").increment(1);
    }
}

/// Inner OP pool transaction type.
///
/// This is `reth_optimism_txpool::OpPooledTransaction` (the pool wrapper), NOT
/// `op_alloy_consensus::OpPooledTransaction` (the devp2p wire envelope) —
/// upstream reuses the name for two different concepts. The wire-envelope
/// equivalent here is `celo_alloy_consensus::CeloPooledTransaction` (the
/// second type parameter below).
///
/// `CeloPoolTx` wraps this to add native-equivalent fee caching for CIP-64.
type InnerPoolTx = OpPooledTransaction<CeloTransactionSigned, CeloPooledTransaction>;

// ---------------------------------------------------------------------------
// ExchangeRate
// ---------------------------------------------------------------------------

/// Fee currency exchange rate: `native_value = fc_value * denominator / numerator`.
#[derive(Debug, Clone, Copy)]
pub struct ExchangeRate {
    /// Units of fee-currency per unit of native (scaled).
    pub numerator: u128,
    /// Units of native per unit of fee-currency (scaled).
    pub denominator: u128,
}

impl ExchangeRate {
    /// Convert a fee-currency amount to its native-CELO equivalent.
    ///
    /// Saturates to `u128::MAX` only when the true mathematical result exceeds
    /// `u128`.
    ///
    /// # Panics
    ///
    /// Panics if `numerator` is zero. This is a logic error — the constructor
    /// rejects zero numerator/denominator rates.
    pub fn to_native(&self, amount: Fc) -> Native {
        assert!(self.numerator != 0, "ExchangeRate numerator must not be zero");
        Native::new(mul_div_saturating(amount.into_inner(), self.denominator, self.numerator))
    }

    /// Convert a native-CELO amount to its fee-currency equivalent.
    ///
    /// Saturates to `u128::MAX` only when the true mathematical result exceeds
    /// `u128`.
    ///
    /// # Panics
    ///
    /// Panics if `denominator` is zero. This is a logic error — the constructor
    /// rejects zero numerator/denominator rates.
    pub fn to_fc(&self, native_amount: Native) -> Fc {
        assert!(self.denominator != 0, "ExchangeRate denominator must not be zero");
        Fc::new(mul_div_saturating(native_amount.into_inner(), self.numerator, self.denominator))
    }
}

/// Compute `(amount * mul) / div` at u256 precision, saturating to `u128::MAX`
/// if the result exceeds `u128`. The intermediate `u128 * u128` product always
/// fits in u256, so unlike the naive u128 form this never spuriously saturates
/// when the final quotient would fit.
fn mul_div_saturating(amount: u128, mul: u128, div: u128) -> u128 {
    let r = U256::from(amount).saturating_mul(U256::from(mul)) / U256::from(div);
    u128::try_from(r).unwrap_or(u128::MAX)
}

// ---------------------------------------------------------------------------
// CeloPoolTx
// ---------------------------------------------------------------------------

/// Native-denominated fee values that the pool exposes through
/// [`alloy_consensus::Transaction`].
///
/// Bundling the two fee fields behind a single `Option` makes the
/// "denomination = type" invariant load-bearing: the `Native` newtype is only
/// reachable inside `Some(NativeFees { .. })`, which by construction is set
/// only when the values are truly native-CELO-denominated. The transitional
/// state — a CIP-64 tx whose `inner.max_fee_per_gas()` is FC-denominated and
/// has not yet been converted — is represented by `None`, so the type can
/// never lie about what it holds.
#[derive(Copy, Clone, Debug)]
struct NativeFees {
    max_fee_per_gas: Native,
    max_priority_fee_per_gas: Option<Native>,
}

/// Fee-currency-aware pool transaction.
///
/// Wraps [`OpPooledTransaction`] and overrides [`Transaction::max_fee_per_gas`] and
/// [`Transaction::max_priority_fee_per_gas`] to return native-equivalent values for
/// CIP-64 transactions. This makes the pool's base fee check (`ENOUGH_FEE_CAP_BLOCK`)
/// and replacement check (`is_underpriced`) work correctly across currencies.
///
/// **Simplification vs op-geth:** Pool ordering uses `CoinbaseTipOrdering` which compares
/// native-equivalent fees, rather than op-geth's `CompareWithRates` which does
/// cross-currency comparison using exchange rates directly. CIP-64 txs from different
/// fee currencies are ordered by their native-equivalent tip. This is acceptable because:
///
///   - Tx ordering in the pool is not consensus-critical (the sequencer picks final order).
///   - Both approaches produce equivalent results when exchange rates are stable.
///   - The native-equivalent comparison avoids the complexity of maintaining rate state inside the
///     ordering comparator.
#[derive(Debug, Clone)]
pub struct CeloPoolTx {
    inner: InnerPoolTx,
    /// Native-equivalent fees.
    ///
    /// - Non-CIP-64 txs: populated at construction — `inner.max_fee_per_gas()` is already native
    ///   CELO.
    /// - CIP-64 txs: `None` until [`Self::apply_exchange_rate`] runs, then `Some` with FC values
    ///   converted to native equivalents.
    ///
    /// Trait accessors `expect` this is `Some`. The pool validator
    /// (`apply_exchange_rates_to_pool_tx`) runs the conversion before the
    /// inner validator's stateless checks read the fee accessors, so the
    /// only way to hit the panic is a misuse: constructing a CIP-64
    /// `CeloPoolTx` outside the validator and reading its fees without
    /// first calling [`Self::apply_exchange_rate`].
    native_fees: Option<NativeFees>,
    /// Cached fee currency address (avoids deep-cloning the tx envelope on each access).
    fee_currency: Option<Address>,
    /// Cost checked against the sender's native CELO balance.
    ///
    /// - Non-CIP-64 txs: same as `inner.cost()` (`gas_limit * max_fee + value`).
    /// - CIP-64 txs: just `value`. Gas is paid in the fee currency and is checked separately
    ///   against the sender's ERC20 balance, so it must not be added to the native-CELO cost —
    ///   doing so would reject otherwise-valid txs whose sender has plenty of ERC20 balance but
    ///   only `value` worth of CELO.
    native_cost: NativeU256,
}

/// Message used by accessors that `expect` `native_fees` is populated.
const NATIVE_FEES_NOT_SET: &str = "CeloPoolTx::native_fees must be populated before reading fees: \
    non-CIP-64 txs set it in `new`; CIP-64 txs set it via `apply_exchange_rate`";

/// Extract the fee currency address from a pool transaction without cloning.
///
/// A `feeCurrency` of `Address::ZERO` is treated as native CELO (mapped to `None`) —
/// the zero address cannot host an ERC20 fee currency contract, and the celo-revm
/// handler already treats it as native during execution. Normalizing here ensures
/// the pool and the execution layer agree on which txs use the native fee path.
fn extract_fee_currency(inner: &InnerPoolTx) -> Option<Address> {
    non_native_fee_currency(
        inner.transaction().as_cip64().and_then(|signed| signed.tx().fee_currency),
    )
}

impl CeloPoolTx {
    /// Create a new [`CeloPoolTx`].
    ///
    /// For non-CIP-64 txs the native-fee cache is populated immediately since
    /// `inner`'s fee fields are already native CELO. For CIP-64 txs the cache
    /// is left empty — the caller (the pool validator) must follow up with
    /// [`Self::apply_exchange_rate`] before any code reads the fee accessors.
    pub fn new(inner: InnerPoolTx) -> Self {
        let fee_currency = extract_fee_currency(&inner);
        let native_fees = if fee_currency.is_some() {
            None
        } else {
            Some(NativeFees {
                max_fee_per_gas: Native::new(inner.max_fee_per_gas()),
                max_priority_fee_per_gas: inner.max_priority_fee_per_gas().map(Native::new),
            })
        };
        // For CIP-64 txs the raw `inner.cost()` is `gas_limit * max_fee + value`
        // in *fee-currency* units — treating that as native CELO would reject
        // valid txs whose sender funded gas with ERC20 but holds only `value`
        // in CELO. Use just `value`: gas is paid in the fee currency and is
        // checked separately against the sender's ERC20 balance, so it must
        // not be added to the native-CELO requirement.
        let native_cost =
            NativeU256::new(if fee_currency.is_some() { inner.value() } else { *inner.cost() });
        Self { inner, native_fees, fee_currency, native_cost }
    }

    /// Apply an exchange rate to convert fee-currency values to native equivalents.
    ///
    /// No-op for native (non-CIP-64) transactions — their fee fields are already
    /// denominated in native CELO and were cached at construction. `native_cost`
    /// is intentionally left unchanged: CIP-64 gas is paid in the fee currency,
    /// so the native-CELO requirement is just `value` and was seeded correctly
    /// by [`Self::new`].
    pub fn apply_exchange_rate(&mut self, rate: ExchangeRate) {
        if self.fee_currency.is_none() {
            return;
        }
        self.native_fees = Some(NativeFees {
            max_fee_per_gas: rate.to_native(Fc::new(self.inner.max_fee_per_gas())),
            max_priority_fee_per_gas: self
                .inner
                .max_priority_fee_per_gas()
                .map(|v| rate.to_native(Fc::new(v))),
        });
    }

    /// Returns the fee currency address if this is a CIP-64 transaction.
    pub const fn fee_currency(&self) -> Option<Address> {
        self.fee_currency
    }

    /// Maximum gas cost (`gas_limit × max fee per gas`) in the transaction's
    /// *fee-denomination* units: fee-currency units for CIP-64 txs, native CELO
    /// otherwise. Used by the cumulative pooled-expenditure admission check,
    /// which compares same-currency costs — callers filter by
    /// [`Self::fee_currency`] first.
    pub fn fc_gas_cost(&self) -> U256 {
        U256::from(self.inner.gas_limit()).saturating_mul(U256::from(self.inner.max_fee_per_gas()))
    }
}

// ---------------------------------------------------------------------------
// alloy_consensus::Transaction — override fee methods
// ---------------------------------------------------------------------------

impl Transaction for CeloPoolTx {
    fn chain_id(&self) -> Option<u64> {
        self.inner.chain_id()
    }
    fn nonce(&self) -> u64 {
        self.inner.nonce()
    }
    fn gas_limit(&self) -> u64 {
        self.inner.gas_limit()
    }
    fn gas_price(&self) -> Option<u128> {
        // Delegates to inner without conversion: CIP-64 txs are EIP-1559 style and
        // return `None` from `gas_price()`, so no exchange rate adjustment is needed.
        self.inner.gas_price()
    }
    fn max_fee_per_gas(&self) -> u128 {
        self.native_fees.expect(NATIVE_FEES_NOT_SET).max_fee_per_gas.into_inner()
    }
    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.native_fees
            .expect(NATIVE_FEES_NOT_SET)
            .max_priority_fee_per_gas
            .map(Native::into_inner)
    }
    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        self.inner.max_fee_per_blob_gas()
    }
    fn priority_fee_or_price(&self) -> u128 {
        // The `None` branch is unreachable for CIP-64 txs (always EIP-1559 style with
        // priority fee set). For non-CIP-64 txs, the cached priority is copied from
        // inner unchanged, so the fallback is only hit for legacy txs.
        self.native_fees
            .expect(NATIVE_FEES_NOT_SET)
            .max_priority_fee_per_gas
            .map(Native::into_inner)
            .unwrap_or_else(|| self.inner.priority_fee_or_price())
    }
    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        let fees = self.native_fees.expect(NATIVE_FEES_NOT_SET);
        let native_max_fee = fees.max_fee_per_gas.into_inner();
        let native_max_prio = fees.max_priority_fee_per_gas.map(Native::into_inner);
        base_fee.map_or(native_max_fee, |base_fee| {
            let tip = native_max_fee.saturating_sub(base_fee as u128);
            if let Some(max_prio) = native_max_prio &&
                tip > max_prio
            {
                return max_prio + base_fee as u128;
            }
            native_max_fee
        })
    }
    fn is_dynamic_fee(&self) -> bool {
        self.inner.is_dynamic_fee()
    }
    fn kind(&self) -> TxKind {
        self.inner.kind()
    }
    fn is_create(&self) -> bool {
        self.inner.is_create()
    }
    fn value(&self) -> U256 {
        self.inner.value()
    }
    fn input(&self) -> &Bytes {
        self.inner.input()
    }
    fn access_list(&self) -> Option<&AccessList> {
        self.inner.access_list()
    }
    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        self.inner.blob_versioned_hashes()
    }
    fn authorization_list(&self) -> Option<&[alloy_eips::eip7702::SignedAuthorization]> {
        self.inner.authorization_list()
    }
}

// ---------------------------------------------------------------------------
// Typed2718
// ---------------------------------------------------------------------------

impl Typed2718 for CeloPoolTx {
    fn ty(&self) -> u8 {
        self.inner.ty()
    }
}

// ---------------------------------------------------------------------------
// InMemorySize
// ---------------------------------------------------------------------------

impl InMemorySize for CeloPoolTx {
    fn size(&self) -> usize {
        let Self { inner, native_fees, fee_currency, native_cost } = self;
        inner.size() +
            core::mem::size_of_val(native_fees) +
            core::mem::size_of_val(fee_currency) +
            core::mem::size_of_val(native_cost)
    }
}

// ---------------------------------------------------------------------------
// PoolTransaction
// ---------------------------------------------------------------------------

impl PoolTransaction for CeloPoolTx {
    type TryFromConsensusError = <CeloPooledTransaction as TryFrom<CeloTransactionSigned>>::Error;
    type Consensus = CeloTransactionSigned;
    type Pooled = CeloPooledTransaction;

    fn clone_into_consensus(&self) -> Recovered<Self::Consensus> {
        self.inner.clone_into_consensus()
    }

    fn consensus_ref(&self) -> Recovered<&Self::Consensus> {
        self.inner.consensus_ref()
    }

    fn into_consensus(self) -> Recovered<Self::Consensus> {
        self.inner.into_consensus()
    }

    fn into_consensus_with2718(
        self,
    ) -> reth_primitives_traits::WithEncoded<Recovered<Self::Consensus>> {
        self.inner.into_consensus_with2718()
    }

    fn from_pooled(tx: Recovered<Self::Pooled>) -> Self {
        Self::new(InnerPoolTx::from_pooled(tx))
    }

    fn hash(&self) -> &TxHash {
        self.inner.hash()
    }

    fn sender(&self) -> Address {
        self.inner.sender()
    }

    fn sender_ref(&self) -> &Address {
        self.inner.sender_ref()
    }

    fn cost(&self) -> &U256 {
        self.native_cost.as_u256()
    }

    fn encoded_length(&self) -> usize {
        self.inner.encoded_length()
    }
}

// ---------------------------------------------------------------------------
// EthPoolTransaction
// ---------------------------------------------------------------------------

impl EthPoolTransaction for CeloPoolTx {
    fn take_blob(&mut self) -> EthBlobTransactionSidecar {
        EthBlobTransactionSidecar::None
    }

    fn try_into_pooled_eip4844(
        self,
        _sidecar: Arc<BlobTransactionSidecarVariant>,
    ) -> Option<Recovered<Self::Pooled>> {
        None
    }

    fn try_from_eip4844(
        _tx: Recovered<Self::Consensus>,
        _sidecar: BlobTransactionSidecarVariant,
    ) -> Option<Self> {
        None
    }

    fn validate_blob(
        &self,
        _sidecar: &BlobTransactionSidecarVariant,
        _settings: &alloy_eips::eip4844::env_settings::KzgSettings,
    ) -> Result<(), BlobTransactionValidationError> {
        Err(BlobTransactionValidationError::NotBlobTransaction(self.ty()))
    }
}

// ---------------------------------------------------------------------------
// OP-specific traits
// ---------------------------------------------------------------------------

impl MaybeConditionalTransaction for CeloPoolTx {
    fn set_conditional(
        &mut self,
        conditional: alloy_rpc_types_eth::erc4337::TransactionConditional,
    ) {
        self.inner.set_conditional(conditional)
    }

    fn conditional(&self) -> Option<&alloy_rpc_types_eth::erc4337::TransactionConditional> {
        self.inner.conditional()
    }
}

impl MaybeInteropTransaction for CeloPoolTx {
    fn set_interop_deadline(&self, deadline: u64) {
        self.inner.set_interop_deadline(deadline)
    }

    fn interop_deadline(&self) -> Option<u64> {
        self.inner.interop_deadline()
    }
}

impl DataAvailabilitySized for CeloPoolTx {
    fn estimated_da_size(&self) -> u64 {
        self.inner.estimated_da_size()
    }
}

impl OpPooledTx for CeloPoolTx {
    fn encoded_2718(&self) -> Cow<'_, Bytes> {
        Cow::Borrowed(self.inner.encoded_2718())
    }
}

// ---------------------------------------------------------------------------
// FeeCurrencyDirectory reader
// ---------------------------------------------------------------------------

/// Result of looking up exchange rate, balance, and debit simulation for a fee currency.
pub(crate) struct FcLookupResult {
    pub(crate) rate: Option<ExchangeRate>,
    /// Raw ERC20 balance of the sender. `None` if the query failed or was not requested.
    pub(crate) balance: Option<U256>,
    pub(crate) debit_ok: Option<bool>,
    /// The fee currency's extra intrinsic gas (from `getCurrencyConfig`), added on
    /// top of the standard intrinsic gas at block-build time. `None` if the rate
    /// lookup failed (the currency is rejected as unregistered anyway) or the
    /// config read failed — in which case the intrinsic-gas admission check is
    /// skipped and the block builder remains the backstop.
    pub(crate) intrinsic_gas: Option<u64>,
}

/// Trait for looking up fee currency exchange rates, balances, and debit simulation.
///
/// Extracted from the concrete `StateProviderFactory`-based implementation to allow
/// mocking in tests.
pub(crate) trait FcLookup {
    fn lookup_rate_and_balance(
        &self,
        fee_currency: Address,
        fee_currency_directory: Address,
        balance_check: Option<(Address, U256)>,
    ) -> FcLookupResult;
}

/// Couples a [`StateProviderFactory`] with the chain's active [`OpSpecId`] so the
/// pool's system-call EVM runs fee-currency bytecode at the right fork (see
/// `build_pool_evm`). Holds the provider by reference; constructed per
/// validation from the spec cached on [`CeloExchangeRateApplier`].
pub(crate) struct ProviderFcLookup<'a, P> {
    pub(crate) provider: &'a P,
    pub(crate) spec: OpSpecId,
}

impl<P: StateProviderFactory> FcLookup for ProviderFcLookup<'_, P> {
    fn lookup_rate_and_balance(
        &self,
        fee_currency: Address,
        fee_currency_directory: Address,
        balance_check: Option<(Address, U256)>,
    ) -> FcLookupResult {
        lookup_rate_and_balance_impl(
            self.provider,
            fee_currency,
            fee_currency_directory,
            balance_check,
            self.spec,
        )
    }
}

/// Look up the exchange rate, check ERC20 balance, and simulate `debitGasFees`
/// for a fee currency.
///
/// Performs all queries in a single EVM instance to avoid duplicate state
/// provider and EVM construction on the pool validation hot path.
/// The EVM's in-memory journal is discarded when it drops, so the debit
/// simulation causes no persistent state changes.
/// Gas limit for pool EVM system calls (exchange rate, balance, debit sim).
/// Caps execution time to prevent adversarial/buggy fee currency contracts from
/// stalling the pool validator. 1M gas is plenty for simple view calls and
/// ERC20 transfers.
const POOL_SYSTEM_CALL_GAS_LIMIT: u64 = 1_000_000;

/// Build the pool's system-call EVM over `db`, configured at chain fork `spec`.
///
/// The spec MUST be the chain's active fork rather than a context-free default:
/// fee-currency contracts compiled with a recent Solidity emit post-Merge opcodes
/// (PUSH0 = Shanghai, MCOPY/TLOAD = Cancun, i.e. any standard modern OpenZeppelin
/// ERC20). At a pre-Shanghai fork (e.g. BEDROCK) those halt with `NotActivated`, so the
/// `getExchangeRate` / `balanceOf` / `debitGasFees` simulations fail and the
/// pool's pre-checks silently no-op. Raising the spec keeps legacy bytecode
/// working (opcodes are only added across forks) while letting modern bytecode
/// run, matching what execution does.
fn build_pool_evm<DB: revm::Database>(
    db: DB,
    spec: OpSpecId,
) -> celo_revm::CeloEvm<DB, revm::inspector::NoOpInspector> {
    use celo_revm::{CeloBuilder, DefaultCelo};
    use revm::Context;

    Context::celo().with_db(db).modify_cfg_chained(|cfg| cfg.spec = spec).build_celo()
}

fn lookup_rate_and_balance_impl(
    provider: &dyn StateProviderFactory,
    fee_currency: Address,
    fee_currency_directory: Address,
    balance_check: Option<(Address, U256)>,
    spec: OpSpecId,
) -> FcLookupResult {
    use alloy_sol_types::SolCall;
    use celo_revm::contracts::{
        core_contracts::{getCurrencyConfigCall, getExchangeRateCall},
        erc20::IFeeCurrencyERC20,
    };
    use reth_revm::database::StateProviderDatabase;
    use revm::context_interface::result::ExecutionResult;

    let state = match provider.latest() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(target: "celo::pool", %e, ?fee_currency, "Failed to get latest state for FC lookup");
            return FcLookupResult {
                rate: None,
                balance: None,
                debit_ok: None,
                intrinsic_gas: None,
            };
        }
    };
    let db = StateProviderDatabase::new(state);
    let mut evm = build_pool_evm(db, spec);

    // 1. Look up exchange rate
    let rate_calldata = getExchangeRateCall { token: fee_currency }.abi_encode();
    let rate = evm
        .transact_system_call_with_gas_limit(
            fee_currency_directory,
            rate_calldata.into(),
            POOL_SYSTEM_CALL_GAS_LIMIT,
        )
        .inspect_err(|e| {
            tracing::warn!(target: "celo::pool", %e, ?fee_currency, "EVM system call failed for exchange rate lookup");
        })
        .ok()
        .and_then(|result| match result {
            ExecutionResult::Success { output, .. } => Some(output.into_data()),
            other => {
                tracing::warn!(target: "celo::pool", ?fee_currency, ?other, "Exchange rate query returned non-success");
                None
            }
        })
        .and_then(|output| {
            let r = getExchangeRateCall::abi_decode_returns(&output).ok()?;
            let numerator = u128::try_from(r.numerator).ok()?;
            let denominator = u128::try_from(r.denominator).ok()?;
            if numerator == 0 || denominator == 0 {
                return None;
            }
            Some(ExchangeRate { numerator, denominator })
        });

    // If rate lookup failed, the caller will reject the tx as UnregisteredCurrency
    // regardless of balance/debit — skip the remaining EVM calls to avoid wasted
    // work (and adversarial-tx DoS amplification).
    if rate.is_none() {
        return FcLookupResult { rate, balance: None, debit_ok: None, intrinsic_gas: None };
    }

    // 1b. Fetch the fee currency's extra intrinsic gas (getCurrencyConfig).
    // The block builder adds this on top of the standard intrinsic gas
    // (`validate_celo_initial_tx_gas` in celo-revm), so the pool needs it to
    // reject under-funded CIP-64 txs at admission instead of letting them drop
    // silently at build time. Mirrors `get_intrinsic_gas` in core_contracts,
    // including the saturating u64 cap. A failed/undecodable read leaves it
    // `None` and the intrinsic-gas check is skipped (build remains the backstop).
    let intrinsic_gas = {
        let cfg_calldata = getCurrencyConfigCall { token: fee_currency }.abi_encode();
        evm.transact_system_call_with_gas_limit(
            fee_currency_directory,
            cfg_calldata.into(),
            POOL_SYSTEM_CALL_GAS_LIMIT,
        )
        .inspect_err(|e| {
            tracing::warn!(target: "celo::pool", %e, ?fee_currency, "EVM system call failed for currency config lookup");
        })
        .ok()
        .and_then(|result| match result {
            ExecutionResult::Success { output, .. } => Some(output.into_data()),
            other => {
                tracing::warn!(target: "celo::pool", ?fee_currency, ?other, "Currency config query returned non-success");
                None
            }
        })
        .and_then(|output| {
            let cfg = getCurrencyConfigCall::abi_decode_returns(&output).ok()?;
            Some(u64::try_from(cfg.intrinsicGas).unwrap_or(u64::MAX))
        })
    };

    // 2. Check ERC20 balance (only if requested)
    let balance = balance_check.as_ref().and_then(|(sender, _required)| {
        let bal_calldata = IFeeCurrencyERC20::balanceOfCall { account: *sender }.abi_encode();
        let result = evm
            .transact_system_call_with_gas_limit(
                fee_currency,
                bal_calldata.into(),
                POOL_SYSTEM_CALL_GAS_LIMIT,
            )
            .inspect_err(|e| {
                tracing::warn!(target: "celo::pool", %e, ?fee_currency, "EVM system call failed for balance check");
            })
            .ok()?;
        let output = match result {
            ExecutionResult::Success { output, .. } => output.into_data(),
            other => {
                tracing::warn!(target: "celo::pool", ?fee_currency, ?other, "Balance check returned non-success");
                return None;
            }
        };
        let balance = IFeeCurrencyERC20::balanceOfCall::abi_decode_returns(&output).ok()?;
        Some(balance)
    });

    // 3. Simulate debitGasFees (only if balance was sufficient).
    //
    // Catches tokens with custom hooks (pause, blacklist) that would pass
    // the balance check but fail at execution time. The system-call caller is
    // `Address::ZERO` (= `CELO_SYSTEM_ADDRESS`), matching the `onlyVm` modifier
    // that fee currency contracts require. We use `transact_system_call_with_gas_limit`
    // — not `system_call_one_with_caller` — so the simulation is capped at
    // `POOL_SYSTEM_CALL_GAS_LIMIT` instead of inheriting the default 30M
    // budget, which an adversarial or buggy fee currency contract could
    // otherwise burn on every pool admission.
    let balance_ok =
        balance_check.as_ref().zip(balance).map(|((_, required), bal)| bal >= *required);
    let debit_ok = balance_check.and_then(|(sender, required)| {
        if balance_ok != Some(true) {
            return None;
        }
        let debit_calldata =
            IFeeCurrencyERC20::debitGasFeesCall { from: sender, value: required }.abi_encode();
        let result = evm.transact_system_call_with_gas_limit(
            fee_currency,
            debit_calldata.into(),
            POOL_SYSTEM_CALL_GAS_LIMIT,
        );
        match result {
            Ok(ExecutionResult::Success { .. }) => Some(true),
            Ok(ExecutionResult::Revert { .. }) => Some(false),
            Ok(other) => {
                tracing::warn!(target: "celo::pool", ?fee_currency, ?other, "Debit simulation returned non-success/revert");
                None
            }
            Err(e) => {
                tracing::warn!(target: "celo::pool", %e, ?fee_currency, "EVM system call failed for debit simulation");
                None
            }
        }
    });

    FcLookupResult { rate, balance, debit_ok, intrinsic_gas }
}

// ---------------------------------------------------------------------------
// CeloExchangeRateApplier
// ---------------------------------------------------------------------------

/// Type alias for the base fee floor computation closure.
pub type BaseFeeFloorFn = Arc<dyn Fn(&dyn alloy_consensus::BlockHeader, u64) -> u64 + Send + Sync>;

/// Computes the [`OpSpecId`] for the next block from the chain spec, given the
/// estimated next-block timestamp. Refreshed each head block (like
/// [`BaseFeeFloorFn`]) so the pool tracks the chain's current fork, matching the
/// block builder, which derives the spec dynamically too. No hardcoded spec to
/// update on a future hardfork. Drives both the pool's system-call EVM (see
/// `build_pool_evm`) and, via [`OpSpecId::into_eth_spec`], the CIP-64
/// intrinsic-gas admission check.
pub type SpecFn = Arc<dyn Fn(u64) -> OpSpecId + Send + Sync>;

/// Reads a sender's already-committed fee-currency expenditure from the live
/// pool at validation time.
///
/// Arguments are `(sender, fee_currency, nonce)`; returns `(spent, prev_at_nonce)`:
///
/// - `spent`: total FC cost (`gas_limit × FC max fee`) of the sender's pooled txs that pay in
///   `fee_currency`.
/// - `prev_at_nonce`: FC cost of the sender's pooled tx at `nonce` (zero if none, or if it pays in
///   a different currency). When the validated tx replaces a pooled one, only the fee *bump* counts
///   against the balance.
///
/// This mirrors op-geth's `ExistingExpenditure`/`ExistingCost` callbacks
/// (`core/txpool/validation.go`): deriving expenditure from the live pool
/// — instead of a validator-side reservation cache — means replaced or
/// evicted txs can never inflate the total and falsely reject payable txs
/// (issue #250).
pub type PooledFcCostsFn = Arc<dyn Fn(Address, Address, u64) -> (U256, U256) + Send + Sync>;

/// Sum `txs` (one sender's pooled transactions) into the [`PooledFcCostsFn`]
/// result for `fee_currency` and `nonce`: total same-currency FC cost, and the
/// cost of the same-currency tx at `nonce` (the replacement credit).
pub(crate) fn sum_pooled_fc_costs(
    txs: &[Arc<ValidPoolTransaction<CeloPoolTx>>],
    fee_currency: Address,
    nonce: u64,
) -> (U256, U256) {
    let mut spent = U256::ZERO;
    let mut prev_at_nonce = U256::ZERO;
    for pooled in txs {
        let tx = &pooled.transaction;
        if tx.fee_currency() == Some(fee_currency) {
            let cost = tx.fc_gas_cost();
            spent = spent.saturating_add(cost);
            if tx.nonce() == nonce {
                prev_at_nonce = cost;
            }
        }
    }
    (spent, prev_at_nonce)
}

/// Builds the [`PooledFcCostsFn`] over a live pool handle.
///
/// Sums *all* of the sender's pooled txs (pending, base-fee-parked, and
/// queued), not just the pending subpool: op-geth's pending list — the set its
/// `TotalCostFor` sums — keeps txs priced below the current base fee, which
/// reth parks in the base-fee subpool, so a pending-only read would let a
/// sender's parked obligations go uncounted and over-admit. Queued
/// (nonce-gapped) txs are counted too, which op-geth does not do; that errs
/// towards rejection, and only for senders whose own pooled txs already exceed
/// their balance — over-admission is the costlier failure, since a tx whose
/// `debitGasFees` fails at block building gets its whole currency blocklisted
/// for sequencing.
pub fn pooled_fc_costs_reader<P>(pool: P) -> PooledFcCostsFn
where
    P: TransactionPool<Transaction = CeloPoolTx> + 'static,
{
    Arc::new(move |sender, fee_currency, nonce| {
        sum_pooled_fc_costs(&pool.get_transactions_by_sender(sender), fee_currency, nonce)
    })
}

/// Wraps a [`TransactionValidator`] and applies fee-currency exchange rates
/// to validated CIP-64 transactions, so that the pool sees native-equivalent
/// gas prices for ordering, replacement, and base fee classification.
pub struct CeloExchangeRateApplier<V, P> {
    inner: V,
    provider: P,
    fee_currency_directory: Address,
    /// Current base fee floor (in native wei). Updated on each new head block
    /// via `on_new_head_block` to handle the pre-Jovian → Jovian transition:
    /// pre-Jovian uses the static 25 Gwei floor, post-Jovian reads `min_base_fee`
    /// from the chain spec (which parses the parent's `extraData`).
    base_fee_floor: Arc<std::sync::atomic::AtomicU64>,
    /// Computes the base fee floor for the next block given the current tip block.
    /// Returns the base fee floor for the next block given the tip block's header
    /// and estimated next-block timestamp.
    /// Returns 0 if the floor cannot be determined (dev mode).
    base_fee_floor_fn: BaseFeeFloorFn,
    /// Active fork for the next block, refreshed each head block via `spec_fn`.
    /// Drives the pool's system-call EVM (so modern fee-currency bytecode runs;
    /// see `build_pool_evm`) and, via [`OpSpecId::into_eth_spec`], the standard
    /// intrinsic gas in the CIP-64 intrinsic-gas admission check. Matches what the
    /// block builder derives, so there is no hardcoded spec to update on a hardfork.
    next_block_spec: Arc<Mutex<OpSpecId>>,
    /// Computes the next-block [`OpSpecId`] from the chain spec. See [`SpecFn`].
    spec_fn: SpecFn,
    /// Minimum priority fee in native wei. CIP-64 txs must have a priority fee
    /// that, when converted to FC units, is at least this value converted to FC.
    minimum_priority_fee: u128,
    /// Maximum transaction fee in wei (`gas_limit * max_fee_per_gas`). `None` or
    /// `Some(0)` disables the check. For CIP-64 txs, this uses the native-equivalent
    /// max fee after exchange-rate conversion.
    tx_fee_cap: Option<u128>,
    /// Live pooled-expenditure reader for the CIP-64 cumulative balance check.
    ///
    /// Set once in the node builder *after* the pool is constructed (the
    /// validator is built first, so it cannot capture the pool handle at
    /// construction time). While unset, the cumulative check degrades to the
    /// per-tx balance check — unobservable in practice, since validation only
    /// runs through the pool, which the builder wires up before use.
    pooled_fc_costs: Arc<OnceLock<PooledFcCostsFn>>,
}

impl<V: Debug, P> Debug for CeloExchangeRateApplier<V, P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CeloExchangeRateApplier").field("inner", &self.inner).finish()
    }
}

impl<V, P> CeloExchangeRateApplier<V, P> {
    /// Create a new [`CeloExchangeRateApplier`].
    #[allow(clippy::too_many_arguments)] // flat config mirrors the node-builder call site
    pub fn new(
        inner: V,
        provider: P,
        fee_currency_directory: Address,
        base_fee_floor: u64,
        base_fee_floor_fn: BaseFeeFloorFn,
        spec: OpSpecId,
        spec_fn: SpecFn,
        minimum_priority_fee: u128,
        tx_fee_cap: Option<u128>,
        pooled_fc_costs: Arc<OnceLock<PooledFcCostsFn>>,
    ) -> Self {
        Self {
            inner,
            provider,
            fee_currency_directory,
            base_fee_floor: Arc::new(std::sync::atomic::AtomicU64::new(base_fee_floor)),
            base_fee_floor_fn,
            next_block_spec: Arc::new(Mutex::new(spec)),
            spec_fn,
            minimum_priority_fee,
            tx_fee_cap,
            pooled_fc_costs,
        }
    }
}

/// Rejection reason for Celo pool validation.
///
/// Most variants are CIP-64-specific, but `ExceedsFeeCap` applies to all tx types.
/// Implements [`PoolTransactionError`] directly so it can be passed to
/// [`InvalidPoolTransactionError::other`] without separate error structs.
#[derive(Debug)]
enum CeloPoolRejection {
    /// The fee currency is not registered in the FeeCurrencyDirectory.
    UnregisteredCurrency(Address),
    /// The sender has insufficient ERC20 balance for the fee currency.
    InsufficientBalance {
        currency: Address,
        sender: Address,
        required: U256,
        balance: U256,
        /// True when rejection is due to cumulative cost across the sender's
        /// pooled txs (all subpools; see [`PooledFcCostsFn`]).
        cumulative: bool,
    },
    /// The fee cap (in FC terms) is below the base fee floor converted to FC.
    BelowBaseFeeFloor { currency: Address, max_fee_fc: u128, base_fee_floor_fc: u128 },
    /// The priority fee (in FC terms) is below the minimum tip converted to FC.
    BelowMinTip { currency: Address, min_tip_fc: u128, actual: u128 },
    /// The `debitGasFees()` simulation failed (e.g. token is paused or blacklisted).
    DebitSimulationFailed { currency: Address, sender: Address },
    /// The fee currency is registered but its `balanceOf` query failed (reverted or returned
    /// undecodable data), so the sender's balance could not be verified.
    BalanceLookupFailed { currency: Address, sender: Address },
    /// The transaction fee (`gas_limit * max_fee_per_gas`) exceeds the configured fee cap.
    ExceedsFeeCap {
        max_tx_fee_wei: u128,
        tx_fee_cap_wei: u128,
        /// Set when the tx uses a CIP-64 fee currency.
        fee_currency: Option<Address>,
    },
    /// The gas limit cannot cover the CIP-64 build-time intrinsic gas: the
    /// standard intrinsic gas plus the fee currency's extra intrinsic gas. The
    /// block builder would drop such a tx (`CallGasCostMoreThanGasLimit`) with no
    /// trace, so reject it at admission with a clear, permanent error instead.
    IntrinsicGasTooLow { currency: Address, gas_limit: u64, required: u64 },
}

impl std::fmt::Display for CeloPoolRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnregisteredCurrency(fc) => {
                write!(f, "unregistered fee-currency address {fc}")
            }
            Self::InsufficientBalance { currency, sender, required, balance, cumulative } => {
                if *cumulative {
                    write!(
                        f,
                        "cumulative fee-currency cost ({required}) exceeds balance ({balance}) \
                         for sender {sender} in fee-currency {currency}"
                    )
                } else {
                    write!(
                        f,
                        "insufficient fee-currency balance: required {required}, \
                         available {balance} for sender {sender} in fee-currency {currency}"
                    )
                }
            }
            Self::BelowBaseFeeFloor { currency, max_fee_fc, base_fee_floor_fc } => {
                write!(
                    f,
                    "fee cap ({max_fee_fc}) below base fee floor ({base_fee_floor_fc}) \
                     for fee-currency {currency}"
                )
            }
            Self::BelowMinTip { currency, min_tip_fc, actual } => {
                write!(
                    f,
                    "CIP-64 priority fee {actual} below minimum {min_tip_fc} \
                     for fee-currency {currency}"
                )
            }
            Self::DebitSimulationFailed { currency, sender } => {
                write!(
                    f,
                    "debitGasFees simulation failed for sender {sender} \
                     in fee-currency {currency}"
                )
            }
            Self::BalanceLookupFailed { currency, sender } => {
                write!(
                    f,
                    "fee-currency balanceOf query failed for sender {sender} \
                     in fee-currency {currency}"
                )
            }
            Self::ExceedsFeeCap { max_tx_fee_wei, tx_fee_cap_wei, fee_currency } => {
                if let Some(fc) = fee_currency {
                    write!(
                        f,
                        "tx fee ({max_tx_fee_wei} wei) exceeds the configured cap \
                         ({tx_fee_cap_wei} wei) for fee-currency {fc}"
                    )
                } else {
                    write!(
                        f,
                        "tx fee ({max_tx_fee_wei} wei) exceeds the configured cap \
                         ({tx_fee_cap_wei} wei)"
                    )
                }
            }
            Self::IntrinsicGasTooLow { currency, gas_limit, required } => {
                write!(
                    f,
                    "intrinsic gas too low: gas limit {gas_limit} below required {required} \
                     (standard intrinsic + fee-currency intrinsic) for fee-currency {currency}"
                )
            }
        }
    }
}

impl std::error::Error for CeloPoolRejection {}

impl PoolTransactionError for CeloPoolRejection {
    fn is_bad_transaction(&self) -> bool {
        match self {
            // Insufficient balance is transient — balance may change.
            Self::InsufficientBalance { .. } => false,
            // Debit simulation failure is transient — token state may change.
            Self::DebitSimulationFailed { .. } => false,
            // A failed balanceOf query is treated as transient — the token/state may recover.
            Self::BalanceLookupFailed { .. } => false,
            // Fee cap rejection is permanent — the tx's gas cost won't change.
            Self::ExceedsFeeCap { .. } => true,
            _ => true,
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Apply the fee-currency exchange rate to a [`CeloPoolTx`] (no-op for native
/// txs) and run the CIP-64-specific admission checks: base-fee floor, minimum
/// tip, ERC20 balance with cumulative tracking, `debitGasFees` simulation, and
/// the configured tx fee cap.
///
/// Must run **before** the wrapped inner validator's stateless checks because
/// those checks read `max_fee_per_gas()`/`max_priority_fee_per_gas()`, which
/// `expect` `native_fees` to be populated.
///
/// `pooled_fc_costs` supplies the sender's live pooled expenditure for the
/// cumulative balance check; see [`PooledFcCostsFn`].
#[allow(clippy::too_many_arguments)] // flat pool-admission config; a struct adds no clarity here
fn apply_exchange_rates_to_pool_tx(
    lookup: &dyn FcLookup,
    tx: &mut CeloPoolTx,
    fee_currency_directory: Address,
    base_fee_floor: u64,
    minimum_priority_fee: u128,
    tx_fee_cap: Option<u128>,
    pooled_fc_costs: &dyn Fn(Address, Address, u64) -> (U256, U256),
    eth_spec: SpecId,
) -> Result<(), CeloPoolRejection> {
    if let Some(fc) = tx.fee_currency() {
        let max_fee_fc = Fc::new(tx.inner.max_fee_per_gas());
        let max_priority_fee_fc: Option<Fc> = tx.inner.max_priority_fee_per_gas().map(Fc::new);

        // Look up exchange rate, check ERC20 balance, and simulate debit
        // in a single EVM instance. `required_fc` stays as raw U256 because
        // FcLookup::lookup_rate_and_balance, CeloPoolRejection::InsufficientBalance,
        // and the pooled-expenditure read all speak U256 — the `_fc` suffix is the
        // only denomination marker the type system can't reach.
        let required_fc = tx.fc_gas_cost();
        let sender = tx.sender();
        CeloPoolMetrics::exchange_rate_lookup();
        let result =
            lookup.lookup_rate_and_balance(fc, fee_currency_directory, Some((sender, required_fc)));

        let rate = match result.rate {
            Some(r) => r,
            None => {
                tracing::warn!(
                    target: "celo::pool",
                    ?fc,
                    "Rejecting CIP-64 tx: unregistered fee currency"
                );
                CeloPoolMetrics::cip64_rejection("unregistered_currency");
                return Err(CeloPoolRejection::UnregisteredCurrency(fc));
            }
        };

        // Check: gas limit must cover the build-time intrinsic gas (standard
        // intrinsic + the fee currency's extra intrinsic gas). The inner
        // validator only enforces the standard intrinsic, so without this a tx
        // with a gas limit in [standard, standard + fc_intrinsic) passes
        // admission and is then silently dropped by the block builder
        // (CallGasCostMoreThanGasLimit, with no log/metric). Mirrors celo-revm's
        // `validate_celo_initial_tx_gas`. Skipped when `intrinsic_gas` is None
        // (the config read failed) — the block builder stays the backstop.
        if let Some(fc_intrinsic) = result.intrinsic_gas {
            let (access_list_accounts, access_list_storage_keys) =
                tx.access_list().map_or((0, 0), |al| {
                    (al.0.len() as u64, al.0.iter().map(|i| i.storage_keys.len() as u64).sum())
                });
            let authorization_list_num = tx.authorization_list().map_or(0, |a| a.len() as u64);
            let standard_intrinsic = calculate_initial_tx_gas(
                eth_spec,
                tx.input().as_ref(),
                tx.is_create(),
                access_list_accounts,
                access_list_storage_keys,
                authorization_list_num,
            )
            .initial_total_gas;
            let required = standard_intrinsic.saturating_add(fc_intrinsic);
            if tx.gas_limit() < required {
                tracing::warn!(
                    target: "celo::pool",
                    ?fc,
                    gas_limit = tx.gas_limit(),
                    required,
                    standard_intrinsic,
                    fc_intrinsic,
                    "Rejecting CIP-64 tx: gas limit below build-time intrinsic gas"
                );
                CeloPoolMetrics::cip64_rejection("intrinsic_gas_too_low");
                return Err(CeloPoolRejection::IntrinsicGasTooLow {
                    currency: fc,
                    gas_limit: tx.gas_limit(),
                    required,
                });
            }
        }

        // Check: fee cap must be >= base fee floor converted to FC.
        let base_fee_floor_fc = rate.to_fc(Native::new(base_fee_floor as u128));
        if max_fee_fc < base_fee_floor_fc {
            tracing::warn!(
                target: "celo::pool",
                ?fc,
                %max_fee_fc,
                %base_fee_floor_fc,
                "Rejecting CIP-64 tx: fee cap below base fee floor"
            );
            CeloPoolMetrics::cip64_rejection("below_base_fee_floor");
            return Err(CeloPoolRejection::BelowBaseFeeFloor {
                currency: fc,
                max_fee_fc: max_fee_fc.into_inner(),
                base_fee_floor_fc: base_fee_floor_fc.into_inner(),
            });
        }

        // Check: effective tip must meet the minimum tip converted to FC.
        // The effective tip is min(max_fee - base_fee_floor_fc, priority_fee),
        // matching op-geth's EffectiveGasTipIntCmp(minTip, baseFeeFloor).
        let min_tip_fc = rate.to_fc(Native::new(minimum_priority_fee));
        let actual_priority = max_priority_fee_fc.unwrap_or_default();
        let effective_tip = max_fee_fc.saturating_sub(base_fee_floor_fc).min(actual_priority);
        if effective_tip < min_tip_fc {
            tracing::warn!(
                target: "celo::pool",
                ?fc,
                %effective_tip,
                %min_tip_fc,
                "Rejecting CIP-64 tx: effective tip below minimum"
            );
            CeloPoolMetrics::cip64_rejection("below_min_tip");
            return Err(CeloPoolRejection::BelowMinTip {
                currency: fc,
                min_tip_fc: min_tip_fc.into_inner(),
                actual: effective_tip.into_inner(),
            });
        }

        tx.apply_exchange_rate(rate);
        tracing::info!(
            target: "celo::pool",
            ?fc,
            numerator = rate.numerator,
            denominator = rate.denominator,
            %max_fee_fc,
            max_fee_native = %tx.native_fees.expect(NATIVE_FEES_NOT_SET).max_fee_per_gas,
            "Applied exchange rate to CIP-64 pool tx"
        );

        // Check ERC20 balance result (query already done above). A `None` balance
        // means the balanceOf query failed; the `else` branch rejects it (fail
        // closed) now that the pool EVM runs at the chain's active spec.
        if let Some(balance) = result.balance {
            if required_fc > balance {
                tracing::warn!(
                    target: "celo::pool",
                    ?fc,
                    ?sender,
                    ?required_fc,
                    ?balance,
                    "Rejecting CIP-64 tx: insufficient fee currency balance"
                );
                CeloPoolMetrics::cip64_rejection("insufficient_balance");
                return Err(CeloPoolRejection::InsufficientBalance {
                    currency: fc,
                    sender,
                    required: required_fc,
                    balance,
                    cumulative: false,
                });
            }

            // Cumulative balance check (op-geth parity, issue #250): together
            // with the sender's pooled txs in this currency, this tx must stay
            // within balance. Expenditure is read from the live pool at
            // validation time — never cached — so replaced or evicted txs
            // cannot inflate it. When this tx replaces a pooled tx at the same
            // nonce, only the fee *bump* counts (op-geth's `ExistingCost`
            // credit).
            //
            // Concurrent validations of the same sender cannot see each other
            // (neither tx is in the pool yet), so a same-instant burst can
            // overdraft within a few-ms window. op-geth accepts the equivalent
            // exposure (a sender can drain its balance right after admission);
            // the block builder skips such txs when the debit fails.
            let (spent_fc, prev_at_nonce_fc) = pooled_fc_costs(sender, fc, tx.nonce());
            let needed_fc = spent_fc.saturating_add(required_fc).saturating_sub(prev_at_nonce_fc);
            if needed_fc > balance {
                tracing::warn!(
                    target: "celo::pool",
                    ?fc,
                    ?sender,
                    ?required_fc,
                    spent = %spent_fc,
                    replaced = %prev_at_nonce_fc,
                    ?balance,
                    "Rejecting CIP-64 tx: cumulative fee currency cost exceeds balance"
                );
                CeloPoolMetrics::cip64_rejection("cumulative_balance_exceeded");
                return Err(CeloPoolRejection::InsufficientBalance {
                    currency: fc,
                    sender,
                    required: needed_fc,
                    balance,
                    cumulative: true,
                });
            }
        } else {
            // The currency is registered (rate found) and a balance check was requested, so a
            // missing balance here means the balanceOf query failed (reverted / undecodable).
            // Fail closed: such a token cannot pay fees (the execution-time debit would fail
            // too), and admitting it would be a fail-open divergence from upstream's
            // deterministic balance rejection. Safe now that the pool EVM runs at the chain's
            // active spec (see `build_pool_evm`): a modern currency's balanceOf no longer
            // returns None merely because PUSH0 halted at the pre-Shanghai default the pool
            // EVM previously used.
            tracing::warn!(
                target: "celo::pool",
                ?fc,
                ?sender,
                "Rejecting CIP-64 tx: fee currency balanceOf query failed"
            );
            CeloPoolMetrics::cip64_rejection("balance_lookup_failed");
            return Err(CeloPoolRejection::BalanceLookupFailed { currency: fc, sender });
        }

        // Check debit simulation result. If debit_ok is None (not attempted
        // or query failed), allow the tx — it will be caught at execution.
        if let Some(false) = result.debit_ok {
            tracing::warn!(
                target: "celo::pool",
                ?fc,
                ?sender,
                "Rejecting CIP-64 tx: debitGasFees simulation failed"
            );
            CeloPoolMetrics::cip64_rejection("debit_simulation_failed");
            return Err(CeloPoolRejection::DebitSimulationFailed { currency: fc, sender });
        }
    }

    // Fee cap check: applies to both CIP-64 (using native-equivalent fee) and native txs.
    // We derive the gas fee from `gas_limit * max_fee_per_gas` rather than `cost - value`,
    // because for CIP-64 `native_cost` excludes gas (gas is paid in fee currency, not CELO).
    if let Some(cap) = tx_fee_cap &&
        cap > 0
    {
        // Widen to U256 only to multiply gas_limit without overflow.
        let fee_cost = NativeU256::new(
            U256::from(tx.gas_limit()).saturating_mul(U256::from(tx.max_fee_per_gas())),
        );
        let max_tx_fee_wei = u128::try_from(fee_cost.into_inner()).map_or_else(
            |_| {
                tracing::warn!(
                    target: "celo::pool",
                    %fee_cost,
                    "Fee cost exceeds u128::MAX, clamping — tx likely has extreme fee values"
                );
                Native::new(u128::MAX)
            },
            Native::new,
        );
        if max_tx_fee_wei > Native::new(cap) {
            if tx.fee_currency().is_some() {
                CeloPoolMetrics::cip64_rejection("exceeds_fee_cap");
            }
            return Err(CeloPoolRejection::ExceedsFeeCap {
                max_tx_fee_wei: max_tx_fee_wei.into_inner(),
                tx_fee_cap_wei: cap,
                fee_currency: tx.fee_currency(),
            });
        }
    }

    if tx.fee_currency().is_some() {
        CeloPoolMetrics::cip64_accepted();
    }

    Ok(())
}

impl<V, P> TransactionValidator for CeloExchangeRateApplier<V, P>
where
    V: TransactionValidator<Transaction = CeloPoolTx>,
    P: StateProviderFactory + Debug + Send + Sync + 'static,
{
    type Transaction = CeloPoolTx;
    type Block = V::Block;

    fn validate_transaction(
        &self,
        origin: reth_transaction_pool::TransactionOrigin,
        mut transaction: Self::Transaction,
    ) -> impl core::future::Future<Output = TransactionValidationOutcome<Self::Transaction>> + Send
    {
        // Apply the exchange rate and run the CIP-64-specific checks BEFORE
        // delegating to the inner validator. Its stateless tip-vs-fee-cap
        // check calls `max_fee_per_gas()`/`max_priority_fee_per_gas()`, both
        // of which `expect` `native_fees` to be populated — for CIP-64 txs
        // that population only happens here.
        let base_fee_floor = self.base_fee_floor.load(std::sync::atomic::Ordering::Acquire);
        // One cached fork drives both the pool's system-call EVM (so modern
        // fee-currency bytecode runs; see `build_pool_evm`) and the intrinsic-gas
        // admission check (via its eth-spec projection).
        let spec = *self.next_block_spec.lock().unwrap_or_else(|e| e.into_inner());
        let eth_spec = spec.into_eth_spec();
        let lookup = ProviderFcLookup { provider: &self.provider, spec };
        let pooled_fc_costs = self.pooled_fc_costs.get().cloned();
        let prepared = apply_exchange_rates_to_pool_tx(
            &lookup,
            &mut transaction,
            self.fee_currency_directory,
            base_fee_floor,
            self.minimum_priority_fee,
            self.tx_fee_cap,
            &|sender, fc, nonce| {
                pooled_fc_costs
                    .as_ref()
                    .map_or((U256::ZERO, U256::ZERO), |read| read(sender, fc, nonce))
            },
            eth_spec,
        );
        // Split into Ok/Err up-front so both branches of the async block
        // share a single concrete future type.
        let staged = match prepared {
            Ok(()) => Ok(self.inner.validate_transaction(origin, transaction)),
            Err(rejection) => Err((transaction, rejection)),
        };
        async move {
            match staged {
                Err((tx, rejection)) => TransactionValidationOutcome::Invalid(
                    tx,
                    InvalidPoolTransactionError::other(rejection),
                ),
                Ok(inner_fut) => inner_fut.await,
            }
        }
    }

    fn on_new_head_block(&self, new_tip_block: &SealedBlock<Self::Block>) {
        self.inner.on_new_head_block(new_tip_block);

        // Recompute the base fee floor for the next block.
        // Pre-Jovian: static 25 Gwei floor. Post-Jovian: read from chain spec.
        use alloy_consensus::BlockHeader;
        let header = new_tip_block.header();
        // Estimate next block timestamp as current + 1 (conservative; exact value
        // only needs to be close enough for fork activation boundary checks).
        let next_ts = header.timestamp().saturating_add(1);
        let new_floor = (self.base_fee_floor_fn)(header, next_ts);
        self.base_fee_floor.store(new_floor, std::sync::atomic::Ordering::Release);

        // Recompute the active fork for the next block (same trigger as the floor)
        // so the pool EVM spec and the intrinsic-gas admission check track fork
        // activations automatically — nothing to update by hand when a hardfork lands.
        let new_spec = (self.spec_fn)(next_ts);
        *self.next_block_spec.lock().unwrap_or_else(|e| e.into_inner()) = new_spec;
    }
}

// ---------------------------------------------------------------------------
// CeloPoolMaintainer — evict CIP-64 txs when fee currency is deregistered
// ---------------------------------------------------------------------------

/// Monitors canonical state changes and evicts pooled CIP-64 transactions
/// whose fee currency has been deregistered from the `FeeCurrencyDirectory`.
#[derive(Debug)]
pub struct CeloPoolMaintainer<Pool, P> {
    pool: Pool,
    provider: P,
    fee_currency_directory: Address,
    /// Cached set of registered currencies. Only scan pool when this changes.
    ///
    /// `None` means the maintainer has never observed a successful query,
    /// so no diff baseline exists yet. Diffing against an empty baseline
    /// would silently miss currencies removed during a startup outage,
    /// leaving stale CIP-64 txs in the pool — see [`Self::on_new_block`].
    registered_currencies: Option<std::collections::HashSet<Address>>,
}

impl<Pool, P> CeloPoolMaintainer<Pool, P> {
    /// Create a new [`CeloPoolMaintainer`].
    pub const fn new(pool: Pool, provider: P, fee_currency_directory: Address) -> Self {
        Self { pool, provider, fee_currency_directory, registered_currencies: None }
    }
}

impl<Pool, P> CeloPoolMaintainer<Pool, P>
where
    Pool: reth_transaction_pool::TransactionPool<Transaction = CeloPoolTx>,
    P: StateProviderFactory,
{
    /// Query the currently registered fee currencies from the `FeeCurrencyDirectory`.
    fn query_registered_currencies(&self) -> Option<std::collections::HashSet<Address>> {
        use alloy_sol_types::SolCall;
        use celo_revm::contracts::core_contracts::getCurrenciesCall;
        use reth_revm::database::StateProviderDatabase;
        use revm::context_interface::result::ExecutionResult;

        let state = self.provider.latest().inspect_err(|e| {
            tracing::warn!(target: "celo::pool", %e, "Failed to get latest state for currency query");
        }).ok()?;
        let db = StateProviderDatabase::new(state);
        // The FeeCurrencyDirectory is legacy bytecode and this read-only
        // `getCurrencies` query has no fork-gated semantics, so the Celo default
        // spec suffices (and stays forward-compatible if the directory is ever
        // redeployed with PUSH0-emitting bytecode). The maintainer never rejects
        // txs, so unlike the validator it needs no per-head spec tracking.
        let mut evm = build_pool_evm(db, celo_revm::CELO_DEFAULT_SPEC);

        let calldata = getCurrenciesCall {}.abi_encode();
        let result = evm
            .transact_system_call_with_gas_limit(
                self.fee_currency_directory,
                calldata.into(),
                POOL_SYSTEM_CALL_GAS_LIMIT,
            )
            .inspect_err(|e| {
                tracing::warn!(target: "celo::pool", %e, "EVM system call failed querying fee currencies");
            })
            .ok()?;

        match result {
            ExecutionResult::Success { output, .. } => {
                let currencies: Vec<Address> =
                    getCurrenciesCall::abi_decode_returns(&output.into_data()).ok()?;
                Some(currencies.into_iter().collect())
            }
            other => {
                tracing::warn!(target: "celo::pool", ?other, "Fee currency query returned non-success");
                None
            }
        }
    }

    /// Run the maintainer, listening for canonical state changes.
    pub async fn run(
        mut self,
        mut events: reth_provider::CanonStateNotifications<crate::primitives::CeloPrimitives>,
    ) {
        // Try to seed the cache. If this fails, [`Self::on_new_block`]
        // detects the uninitialized state on its first successful query
        // and scans the full pool against the freshly observed registry.
        self.on_new_block();

        loop {
            match events.recv().await {
                Ok(_notification) => {
                    self.on_new_block();
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(
                        target: "celo::pool",
                        skipped,
                        "Celo pool maintainer lagged, checking currencies"
                    );
                    self.on_new_block();
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::warn!(
                        target: "celo::pool",
                        "Canonical state stream closed, stopping Celo pool maintainer"
                    );
                    break;
                }
            }
        }
    }

    /// Handle a new canonical block: check if registered currencies changed
    /// and evict CIP-64 txs with deregistered currencies.
    fn on_new_block(&mut self) {
        let Some(new_currencies) = self.query_registered_currencies() else {
            CeloPoolMetrics::maintainer_failure();
            return;
        };

        let to_evict: Vec<TxHash> = match self.registered_currencies.as_ref() {
            // Cache is up to date — nothing to evict.
            Some(old) if *old == new_currencies => Vec::new(),
            Some(old) => {
                let removed: std::collections::HashSet<_> =
                    old.difference(&new_currencies).copied().collect();
                if removed.is_empty() {
                    Vec::new()
                } else {
                    tracing::info!(
                        target: "celo::pool",
                        ?removed,
                        "Detected deregistered fee currencies"
                    );
                    self.pool_txs_with_currency(|fc| removed.contains(fc))
                }
            }
            // First successful query (e.g. startup query failed and left
            // the cache empty). Diffing against an empty baseline would
            // silently miss currencies that were deregistered during the
            // outage, so scan the whole pool against the current registry.
            None => self.pool_txs_with_currency(|fc| !new_currencies.contains(fc)),
        };

        if !to_evict.is_empty() {
            CeloPoolMetrics::pool_eviction(to_evict.len() as u64);
            tracing::info!(
                target: "celo::pool",
                count = to_evict.len(),
                "Evicting CIP-64 txs with unregistered fee currencies"
            );
            self.pool.remove_transactions(to_evict);
        }

        self.registered_currencies = Some(new_currencies);
    }

    /// Collect hashes of pooled CIP-64 transactions whose fee currency
    /// matches `pred`.
    fn pool_txs_with_currency(&self, pred: impl Fn(&Address) -> bool) -> Vec<TxHash> {
        let all_txs = self.pool.all_transactions();
        all_txs
            .pending
            .iter()
            .chain(all_txs.queued.iter())
            .filter_map(|vtx| {
                let fc = vtx.transaction.fee_currency()?;
                pred(&fc).then(|| *vtx.hash())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{make_test_tx, make_test_tx_with_nonce};

    /// A fee-currency contract compiled with a recent Solidity emits PUSH0
    /// (0x5f, EIP-3855, a Shanghai opcode). The pool's system-call EVM must run
    /// at the chain's active fork, otherwise that bytecode halts `NotActivated`
    /// and the pool's balance/debit/rate pre-checks silently no-op. Build a
    /// minimal contract whose code is `PUSH0; POP; STOP` and confirm it halts at
    /// BEDROCK (pre-Shanghai) but runs at the active spec once `build_pool_evm`
    /// raises it.
    #[test]
    fn build_pool_evm_honors_chain_spec_for_modern_opcodes() {
        use revm::{
            context_interface::result::ExecutionResult,
            database::InMemoryDB,
            state::{AccountInfo, Bytecode},
        };

        let target = Address::with_last_byte(0x42);
        let make_db = || {
            // PUSH0, POP, STOP
            let code = Bytecode::new_raw(Bytes::from_static(&[0x5f, 0x50, 0x00]));
            let mut db = InMemoryDB::default();
            db.insert_account_info(target, AccountInfo::from_bytecode(code));
            db
        };

        // BEDROCK maps to SpecId::MERGE (pre-Shanghai): PUSH0 is not activated.
        let mut bedrock = build_pool_evm(make_db(), OpSpecId::BEDROCK);
        let at_bedrock = bedrock.transact_system_call_with_gas_limit(
            target,
            Bytes::new(),
            POOL_SYSTEM_CALL_GAS_LIMIT,
        );
        assert!(
            !matches!(at_bedrock, Ok(ExecutionResult::Success { .. })),
            "PUSH0 must not execute at BEDROCK, got {at_bedrock:?}",
        );

        // Isthmus maps to SpecId::PRAGUE: the same bytecode runs to completion.
        let mut isthmus = build_pool_evm(make_db(), OpSpecId::ISTHMUS);
        let at_isthmus = isthmus.transact_system_call_with_gas_limit(
            target,
            Bytes::new(),
            POOL_SYSTEM_CALL_GAS_LIMIT,
        );
        assert!(
            matches!(at_isthmus, Ok(ExecutionResult::Success { .. })),
            "PUSH0 must execute at the active spec, got {at_isthmus:?}",
        );
    }

    #[test]
    fn test_exchange_rate_to_native() {
        // 1 fc = 500 native (denominator/numerator = 1000/2 = 500)
        let rate = ExchangeRate { numerator: 2, denominator: 1000 };
        assert_eq!(rate.to_native(Fc::new(100)), Native::new(50_000));
        assert_eq!(rate.to_native(Fc::new(0)), Native::new(0));

        // 1 fc = 0.5 native (numerator > denominator)
        let rate = ExchangeRate { numerator: 2000, denominator: 1000 };
        assert_eq!(rate.to_native(Fc::new(100)), Native::new(50));
    }

    #[test]
    fn test_apply_exchange_rate_converts_fees() {
        let fc = Address::with_last_byte(0xAA);
        let mut tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 100, Address::with_last_byte(1));

        // rate: 1 FC = 2 native (denominator=2, numerator=1)
        let rate = ExchangeRate { numerator: 1, denominator: 2 };
        tx.apply_exchange_rate(rate);

        // native_max_fee = 1_000_000_000 * 2 / 1 = 2_000_000_000
        assert_eq!(tx.max_fee_per_gas(), 2_000_000_000);
        // native_priority = 100 * 2 / 1 = 200
        assert_eq!(tx.max_priority_fee_per_gas(), Some(200));
    }

    #[test]
    fn test_apply_exchange_rate_noop_for_native() {
        let mut tx = make_test_tx(None, 21_000, 1_000_000_000, 100, Address::with_last_byte(1));
        let original_fee = tx.max_fee_per_gas();
        let original_priority = tx.max_priority_fee_per_gas();
        let original_cost = *tx.cost();

        let rate = ExchangeRate { numerator: 1, denominator: 2 };
        tx.apply_exchange_rate(rate);

        // fee_currency is None → early return, all fields unchanged
        assert_eq!(tx.max_fee_per_gas(), original_fee);
        assert_eq!(tx.max_priority_fee_per_gas(), original_priority);
        assert_eq!(*tx.cost(), original_cost);
    }

    #[test]
    fn test_extract_fee_currency_cip64() {
        let fc = Address::with_last_byte(0xBB);
        let tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 100, Address::with_last_byte(1));
        assert_eq!(tx.fee_currency(), Some(fc));
    }

    #[test]
    fn test_extract_fee_currency_native() {
        let tx = make_test_tx(None, 21_000, 1_000_000_000, 100, Address::with_last_byte(1));
        assert_eq!(tx.fee_currency(), None);
    }

    #[test]
    fn test_extract_fee_currency_zero_address_treated_as_native() {
        // A CIP-64 tx that sets `feeCurrency = 0x000…000` should be routed through the
        // native fee path, matching how the celo-revm handler treats the zero address.
        let tx = make_test_tx(
            Some(Address::ZERO),
            21_000,
            1_000_000_000,
            100,
            Address::with_last_byte(1),
        );
        assert_eq!(tx.fee_currency(), None);
    }

    #[test]
    fn test_native_cost_after_exchange_rate() {
        let fc = Address::with_last_byte(0xCC);
        let mut tx = make_test_tx(Some(fc), 100, 1_000, 10, Address::with_last_byte(1));

        // rate: 1 FC = 0.5 native (numerator=2, denominator=1)
        let rate = ExchangeRate { numerator: 2, denominator: 1 };
        tx.apply_exchange_rate(rate);

        // For CIP-64, gas is paid in fee currency, so native_cost stays at `value`
        // (== 0 here) — including native-equivalent gas would falsely reject senders
        // who hold ERC20 for gas but little CELO.
        assert_eq!(*tx.cost(), U256::ZERO);
    }

    #[test]
    fn test_cost_unchanged_for_native_tx() {
        let tx = make_test_tx(None, 100, 1_000, 10, Address::with_last_byte(1));
        // For native tx: cost = gas_limit * max_fee + value = 100 * 1000 + 0 = 100_000
        assert_eq!(*tx.cost(), U256::from(100_000));
    }

    #[test]
    fn test_initial_native_cost_excludes_fc_gas_for_cip64() {
        // Before `apply_exchange_rate` is called, the cost seen by the inner
        // validator's native-balance check must NOT include the fee-currency
        // gas cost — otherwise CIP-64 txs whose sender holds plenty of ERC20
        // but little CELO get spuriously rejected. `make_test_tx` builds a tx
        // with `value == 0`, so the expected initial cost is 0.
        let fc = Address::with_last_byte(0xCD);
        let tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 100, Address::with_last_byte(1));
        assert_eq!(
            *tx.cost(),
            U256::ZERO,
            "initial native_cost for a CIP-64 tx must exclude FC-denominated gas"
        );
    }

    #[test]
    #[should_panic(expected = "numerator must not be zero")]
    fn test_exchange_rate_zero_numerator_panics() {
        let rate = ExchangeRate { numerator: 0, denominator: 1000 };
        let _ = rate.to_native(Fc::new(500));
    }

    #[test]
    fn test_exchange_rate_to_fc() {
        // 1 fc = 500 native (denominator/numerator = 1000/2 = 500)
        // So 1 native = 1/500 fc => native * numerator / denominator
        let rate = ExchangeRate { numerator: 2, denominator: 1000 };
        // to_fc(500) = 500 * 2 / 1000 = 1
        assert_eq!(rate.to_fc(Native::new(500)), Fc::new(1));
        assert_eq!(rate.to_fc(Native::new(0)), Fc::new(0));

        // 1 fc = 0.5 native => 1 native = 2 fc
        let rate = ExchangeRate { numerator: 2000, denominator: 1000 };
        // to_fc(100) = 100 * 2000 / 1000 = 200
        assert_eq!(rate.to_fc(Native::new(100)), Fc::new(200));
    }

    #[test]
    fn test_exchange_rate_to_fc_roundtrip() {
        let rate = ExchangeRate { numerator: 3, denominator: 1000 };
        let native = Native::new(1_000_000u128);
        let fc = rate.to_fc(native);
        let back = rate.to_native(fc);
        // Round-trip may lose precision due to integer division, but should be close
        assert!(back <= native);
        assert!((native.into_inner() - back.into_inner()) < rate.denominator / rate.numerator + 1);
    }

    #[test]
    #[should_panic(expected = "denominator must not be zero")]
    fn test_exchange_rate_to_fc_zero_denominator_panics() {
        let rate = ExchangeRate { numerator: 1000, denominator: 0 };
        let _ = rate.to_fc(Native::new(500));
    }

    #[test]
    fn test_cross_currency_replacement_uses_native_equivalents() {
        // Two CIP-64 txs with different fee currencies but same sender/nonce.
        // After exchange rate conversion, max_fee_per_gas returns native-equivalent
        // values, enabling correct cross-currency comparison.
        let sender = Address::with_last_byte(1);
        let fc_a = Address::with_last_byte(0xAA);
        let fc_b = Address::with_last_byte(0xBB);

        // TX A: fee_currency A, max_fee=1000, rate 1:2 (1 FC = 2 native)
        let mut tx_a = make_test_tx(Some(fc_a), 21_000, 1000, 50, sender);
        tx_a.apply_exchange_rate(ExchangeRate { numerator: 1, denominator: 2 });
        // native max_fee = 1000 * 2 = 2000
        assert_eq!(tx_a.max_fee_per_gas(), 2000);

        // TX B: fee_currency B, max_fee=500, rate 1:5 (1 FC = 5 native)
        let mut tx_b = make_test_tx(Some(fc_b), 21_000, 500, 20, sender);
        tx_b.apply_exchange_rate(ExchangeRate { numerator: 1, denominator: 5 });
        // native max_fee = 500 * 5 = 2500
        assert_eq!(tx_b.max_fee_per_gas(), 2500);

        // TX B has higher native-equivalent fee, so replacement check (which
        // compares max_fee_per_gas()) will correctly see B > A.
        assert!(tx_b.max_fee_per_gas() > tx_a.max_fee_per_gas());
    }

    /// The pool's `CoinbaseTipOrdering` must rank CIP-64 txs by their
    /// native-equivalent effective tip, not by raw fee-currency magnitude: a tx
    /// whose fee currency is "cheaper per unit" still wins when its native tip is
    /// higher. The cross-currency test above only checks the `max_fee_per_gas()`
    /// accessor; this drives the actual comparator the pool orders with.
    #[test]
    fn test_coinbase_tip_ordering_ranks_by_native_equivalent_tip() {
        use reth_transaction_pool::{CoinbaseTipOrdering, TransactionOrdering};

        let base_fee = 100u64;
        let sender = Address::with_last_byte(1);

        // A: raw FC max_fee=1000, priority=1000; rate 1 FC = 2 native
        //    -> native max_fee=2000, priority=2000 -> tip = min(2000-100, 2000) = 1900
        let mut tx_a =
            make_test_tx(Some(Address::with_last_byte(0xA1)), 21_000, 1000, 1000, sender);
        tx_a.apply_exchange_rate(ExchangeRate { numerator: 1, denominator: 2 });
        assert_eq!(tx_a.max_fee_per_gas(), 2000);

        // B: raw FC max_fee=500 (LOWER than A's raw 1000), priority=500; rate 1 FC = 5 native
        //    -> native max_fee=2500, priority=2500 -> tip = min(2500-100, 2500) = 2400
        let mut tx_b = make_test_tx(Some(Address::with_last_byte(0xB1)), 21_000, 500, 500, sender);
        tx_b.apply_exchange_rate(ExchangeRate { numerator: 1, denominator: 5 });
        assert_eq!(tx_b.max_fee_per_gas(), 2500);

        let ordering = CoinbaseTipOrdering::<CeloPoolTx>::default();
        // Despite B's lower raw fee-currency fee, its native-equivalent tip is higher,
        // so the comparator must rank B strictly above A.
        assert!(
            ordering.priority(&tx_b, base_fee) > ordering.priority(&tx_a, base_fee),
            "CoinbaseTipOrdering must order CIP-64 txs by native-equivalent tip"
        );
    }

    // -----------------------------------------------------------------------
    // MockFcLookup + apply_exchange_rates_to_pool_tx integration tests
    // -----------------------------------------------------------------------

    #[derive(Debug)]
    struct MockFcLookup {
        rate: Option<ExchangeRate>,
        /// Raw ERC20 balance to return. `None` means query failed.
        balance: Option<U256>,
        debit_ok: Option<bool>,
        /// Fee-currency extra intrinsic gas to return. `None` skips the
        /// intrinsic-gas admission check (mirrors a failed config read).
        intrinsic_gas: Option<u64>,
    }

    impl FcLookup for MockFcLookup {
        fn lookup_rate_and_balance(
            &self,
            _fee_currency: Address,
            _fee_currency_directory: Address,
            _balance_check: Option<(Address, U256)>,
        ) -> FcLookupResult {
            FcLookupResult {
                rate: self.rate,
                balance: self.balance,
                debit_ok: self.debit_ok,
                intrinsic_gas: self.intrinsic_gas,
            }
        }
    }

    /// No pooled expenditure — the default for single-tx admission tests.
    fn no_pooled_txs(_sender: Address, _fc: Address, _nonce: u64) -> (U256, U256) {
        (U256::ZERO, U256::ZERO)
    }

    /// A CIP-64 tx whose gas limit cannot cover the build-time intrinsic gas
    /// (standard intrinsic + the fee currency's extra intrinsic gas) must be
    /// rejected at pool admission, not silently dropped by the block builder.
    /// This is the real cause of the "cEUR tx pending but never mined" reports:
    /// a probe hardcoded gas=90_000, but a cEUR CIP-64 needs 21_000 + 80_000.
    #[test]
    fn test_apply_rates_rejects_cip64_below_build_intrinsic_gas() {
        let fc = Address::with_last_byte(0xEE);
        // 90_000 < 21_000 (standard, empty input) + 80_000 (fee-currency intrinsic)
        let mut tx = make_test_tx(Some(fc), 90_000, 1_000_000_000, 100, Address::with_last_byte(1));

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
            intrinsic_gas: Some(80_000),
        };

        let result = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            0,
            0,
            None,
            &no_pooled_txs,
            SpecId::PRAGUE,
        );

        match result {
            Err(CeloPoolRejection::IntrinsicGasTooLow { currency, gas_limit, required }) => {
                assert_eq!(currency, fc);
                assert_eq!(gas_limit, 90_000);
                assert_eq!(required, 101_000, "21_000 standard + 80_000 fee-currency intrinsic");
            }
            other => panic!("expected IntrinsicGasTooLow, got {other:?}"),
        }
    }

    /// The same tx with a gas limit exactly at the build-time intrinsic gas
    /// (21_000 + 80_000 = 101_000) must pass the intrinsic-gas admission check.
    #[test]
    fn test_apply_rates_accepts_cip64_at_build_intrinsic_gas() {
        let fc = Address::with_last_byte(0xEF);
        let mut tx =
            make_test_tx(Some(fc), 101_000, 1_000_000_000, 100, Address::with_last_byte(1));

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
            intrinsic_gas: Some(80_000),
        };

        let result = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            0,
            0,
            None,
            &no_pooled_txs,
            SpecId::PRAGUE,
        );
        assert!(
            result.is_ok(),
            "gas limit at exactly the build intrinsic must be admitted: {result:?}"
        );
    }

    #[test]
    fn test_apply_rates_successful_conversion() {
        let fc = Address::with_last_byte(0xAA);
        let mut tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 100, Address::with_last_byte(1));

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 2 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };

        let result = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            0,
            0,
            None,
            &no_pooled_txs,
            SpecId::PRAGUE,
        );
        assert!(result.is_ok());

        // 1_000_000_000 * 2/1 = 2_000_000_000
        assert_eq!(tx.max_fee_per_gas(), 2_000_000_000);
    }

    /// Regression for f2b24192: `apply_exchange_rates_to_pool_tx` must not
    /// fold FC-denominated gas into the native-CELO cost check. With a CIP-64
    /// tx whose `value` is zero, the native cost after conversion must remain
    /// zero — the gas cost is denominated in fee currency and is checked
    /// separately against the sender's ERC20 balance. Folding it in here
    /// rejects valid CIP-64 txs whose senders have plenty of ERC20 for gas
    /// but little native CELO.
    #[test]
    fn test_apply_rates_does_not_fold_fc_gas_into_native_cost() {
        let fc = Address::with_last_byte(0xCD);
        // Non-trivial rate + non-trivial gas_limit: the buggy form would
        // produce a non-zero native cost here. Correct form: cost stays at
        // `value` (zero, from make_test_tx).
        let mut tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 100, Address::with_last_byte(1));

        let mock = MockFcLookup {
            // 1 FC = 2 native; gas in FC is far from zero
            rate: Some(ExchangeRate { numerator: 1, denominator: 2 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };

        apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            0,
            0,
            None,
            &no_pooled_txs,
            SpecId::PRAGUE,
        )
        .expect("ok");

        assert_eq!(
            *tx.cost(),
            U256::ZERO,
            "native_cost for CIP-64 must equal value (0 here) — gas is paid in FC"
        );
    }

    #[test]
    fn test_apply_rates_unregistered_currency() {
        let fc = Address::with_last_byte(0xAA);
        let mut tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 100, Address::with_last_byte(1));

        let mock = MockFcLookup { rate: None, balance: None, debit_ok: None, intrinsic_gas: None };
        let result = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            0,
            0,
            None,
            &no_pooled_txs,
            SpecId::PRAGUE,
        );
        assert!(matches!(result, Err(CeloPoolRejection::UnregisteredCurrency(_))));
    }

    #[test]
    fn test_apply_rates_insufficient_balance() {
        let fc = Address::with_last_byte(0xAA);
        let mut tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 100, Address::with_last_byte(1));

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::ZERO),
            debit_ok: None,
            intrinsic_gas: None,
        };
        let result = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            0,
            0,
            None,
            &no_pooled_txs,
            SpecId::PRAGUE,
        );
        assert!(matches!(result, Err(CeloPoolRejection::InsufficientBalance { .. })));
    }

    #[test]
    fn test_apply_rates_rejects_balance_lookup_failure() {
        // Registered currency (rate Some) whose balanceOf query failed (balance None). Must fail
        // closed, not be admitted: admitting it is a fail-open divergence from upstream's
        // deterministic balance rejection. (The currency was requested for balance check, and the
        // rate was found, so a None balance here can only mean the query itself failed.) This is
        // safe to enforce now that the pool EVM runs at the chain's active spec — see
        // `build_pool_evm`: previously balanceOf=None was the norm for modern (PUSH0) currencies
        // at the pre-Shanghai default the pool EVM previously used, so fail-closed wrongly
        // rejected valid txs.
        let fc = Address::with_last_byte(0xAA);
        let mut tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 100, Address::with_last_byte(1));

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: None,
            debit_ok: None,
            intrinsic_gas: None,
        };
        let result = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            0,
            0,
            None,
            &no_pooled_txs,
            SpecId::PRAGUE,
        );
        assert!(
            matches!(result, Err(CeloPoolRejection::BalanceLookupFailed { .. })),
            "registered currency with failed balanceOf must fail closed, got {result:?}",
        );
    }

    #[test]
    fn test_apply_rates_below_base_fee_floor() {
        let fc = Address::with_last_byte(0xAA);
        // max_fee_per_gas = 100 in FC terms
        let mut tx = make_test_tx(Some(fc), 21_000, 100, 10, Address::with_last_byte(1));

        // rate: 1:1, base_fee_floor = 25 Gwei
        // base_fee_floor_fc = to_fc(25_000_000_000) = 25_000_000_000
        // 100 < 25_000_000_000 → reject
        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };
        let result = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            25_000_000_000,
            0,
            None,
            &no_pooled_txs,
            SpecId::PRAGUE,
        );
        assert!(matches!(result, Err(CeloPoolRejection::BelowBaseFeeFloor { .. })));
    }

    #[test]
    fn test_apply_rates_below_min_tip() {
        let fc = Address::with_last_byte(0xAA);
        // priority_fee = 5 in FC terms
        let mut tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 5, Address::with_last_byte(1));

        // rate: 1:1, min_priority_fee = 100
        // min_tip_fc = to_fc(100) = 100
        // 5 < 100 → reject
        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };
        let result = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            0,
            100,
            None,
            &no_pooled_txs,
            SpecId::PRAGUE,
        );
        assert!(matches!(result, Err(CeloPoolRejection::BelowMinTip { .. })));
    }

    #[test]
    fn test_apply_rates_debit_simulation_failed() {
        let fc = Address::with_last_byte(0xAA);
        let mut tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 100, Address::with_last_byte(1));

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(false),
            intrinsic_gas: None,
        };
        let result = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            0,
            0,
            None,
            &no_pooled_txs,
            SpecId::PRAGUE,
        );
        assert!(matches!(result, Err(CeloPoolRejection::DebitSimulationFailed { .. })));
    }

    // -----------------------------------------------------------------------
    // Fee cap tests (Item #8)
    // -----------------------------------------------------------------------

    #[test]
    fn test_fee_cap_cip64_within_cap_after_conversion() {
        let fc = Address::with_last_byte(0xAA);
        // CIP-64 tx: gas=21000, max_fee=1000 FC, value=0
        // FC cost = 21_000_000 FC units → looks large in raw terms
        let mut tx = make_test_tx(Some(fc), 21_000, 1000, 100, Address::with_last_byte(1));

        // rate: 1 FC = 0.001 native (numerator=1000, denominator=1)
        // native_max_fee = 1000 * 1/1000 = 1
        // native_cost = 21_000 * 1 = 21_000 wei
        // Cap = 100_000 wei → within cap → accepted
        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1000, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };
        let result = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            0,
            0,
            Some(100_000),
            &no_pooled_txs,
            SpecId::PRAGUE,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_fee_cap_cip64_exceeds_cap_after_conversion() {
        let fc = Address::with_last_byte(0xAA);
        // CIP-64 tx: gas=21000, max_fee=1_000_000_000 FC
        let mut tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 100, Address::with_last_byte(1));

        // rate: 1:1 → native_cost = 21_000 * 1_000_000_000 = 21_000_000_000_000
        // Cap = 1_000_000_000_000 (1000 Gwei) → exceeds → rejected
        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };
        let result = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            0,
            0,
            Some(1_000_000_000_000),
            &no_pooled_txs,
            SpecId::PRAGUE,
        );
        assert!(matches!(result, Err(CeloPoolRejection::ExceedsFeeCap { .. })));
    }

    #[test]
    fn test_fee_cap_native_tx_exceeds_cap() {
        // Native tx: gas=21_000, max_fee=1_000_000_000, value=0
        // cost - value = 21_000 * 1_000_000_000 = 21_000_000_000_000
        let mut tx = make_test_tx(None, 21_000, 1_000_000_000, 100, Address::with_last_byte(1));

        let mock = MockFcLookup { rate: None, balance: None, debit_ok: None, intrinsic_gas: None };
        let result = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            0,
            0,
            Some(1_000_000_000_000),
            &no_pooled_txs,
            SpecId::PRAGUE,
        );
        assert!(matches!(result, Err(CeloPoolRejection::ExceedsFeeCap { .. })));
    }

    #[test]
    fn test_fee_cap_disabled_with_zero_or_none() {
        // Native tx with large cost, but cap disabled (0 or None) → both pass
        for cap in [Some(0), None] {
            let mut tx = make_test_tx(None, 21_000, 1_000_000_000, 100, Address::with_last_byte(1));

            let mock =
                MockFcLookup { rate: None, balance: None, debit_ok: None, intrinsic_gas: None };
            let result = apply_exchange_rates_to_pool_tx(
                &mock,
                &mut tx,
                Address::ZERO,
                0,
                0,
                cap,
                &no_pooled_txs,
                SpecId::PRAGUE,
            );
            assert!(result.is_ok(), "cap={cap:?} should disable fee cap check");
        }
    }

    // -----------------------------------------------------------------------
    // Gap 11: Effective tip uses min(max_fee - base_fee_floor_fc, priority_fee)
    // -----------------------------------------------------------------------

    #[test]
    fn test_effective_tip_below_min_when_fee_headroom_is_low() {
        let fc = Address::with_last_byte(0xAA);
        // max_fee = 1_000_000_100, priority_fee = 200
        // base_fee_floor = 1_000_000_000 (1 Gwei in native)
        // rate: 1:1 → base_fee_floor_fc = 1_000_000_000
        // effective_tip = min(1_000_000_100 - 1_000_000_000, 200) = min(100, 200) = 100
        // min_tip_fc = to_fc(150) = 150
        // 100 < 150 → reject
        let mut tx = make_test_tx(Some(fc), 21_000, 1_000_000_100, 200, Address::with_last_byte(1));

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };
        let result = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            1_000_000_000,
            150,
            None,
            &no_pooled_txs,
            SpecId::PRAGUE,
        );
        assert!(
            matches!(result, Err(CeloPoolRejection::BelowMinTip { actual: 100, .. })),
            "Should reject: effective tip (100) < min tip (150), got {result:?}"
        );
    }

    #[test]
    fn test_effective_tip_passes_when_priority_fee_is_limiting() {
        let fc = Address::with_last_byte(0xAA);
        // max_fee = 2_000_000_000, priority_fee = 200
        // base_fee_floor = 1_000_000_000
        // rate: 1:1
        // effective_tip = min(2_000_000_000 - 1_000_000_000, 200) = min(1_000_000_000, 200) = 200
        // min_tip = 100
        // 200 >= 100 → accept
        let mut tx = make_test_tx(Some(fc), 21_000, 2_000_000_000, 200, Address::with_last_byte(1));

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };
        let result = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            1_000_000_000,
            100,
            None,
            &no_pooled_txs,
            SpecId::PRAGUE,
        );
        assert!(result.is_ok(), "Should accept: effective tip (200) >= min tip (100)");
    }

    // -----------------------------------------------------------------------
    // Test 12: ABI encoding smoke test for exchange rate lookup
    // -----------------------------------------------------------------------

    #[test]
    fn test_get_exchange_rate_abi_encoding() {
        use alloy_sol_types::SolCall;
        use celo_revm::contracts::core_contracts::getExchangeRateCall;

        let fc = Address::with_last_byte(0xAA);
        let call = getExchangeRateCall { token: fc };
        let encoded = call.abi_encode();

        // Selector is 4 bytes + 32-byte address = 36 bytes
        assert_eq!(encoded.len(), 36, "Encoded call should be 36 bytes");

        // Verify selector matches keccak256("getExchangeRate(address)")[..4]
        let expected_selector = &alloy_primitives::keccak256("getExchangeRate(address)")[..4];
        assert_eq!(&encoded[..4], expected_selector, "Selector mismatch");

        // Round-trip decode
        let decoded = getExchangeRateCall::abi_decode_returns(&alloy_primitives::hex!(
            "0000000000000000000000000000000000000000000000000000000000000064"  // numerator = 100
            "00000000000000000000000000000000000000000000000000000000000003e8"  // denominator = 1000
        ))
        .expect("decode should succeed");

        assert_eq!(decoded.numerator, alloy_primitives::U256::from(100));
        assert_eq!(decoded.denominator, alloy_primitives::U256::from(1000));
    }

    // -----------------------------------------------------------------------
    // CIP-64 high gas limit + exchange rate → fee cap enforcement
    // -----------------------------------------------------------------------

    #[test]
    fn test_cip64_high_gas_limit_exceeds_fee_cap_after_rate_conversion() {
        // CIP-64 tx with block-level gas limit (30M) and a moderate per-gas fee.
        // In FC terms the cost looks small, but after exchange rate conversion to
        // native the total fee exceeds the configured cap.
        //
        // gas=30_000_000, max_fee=10 FC, rate 1:1 → native cost = 300_000_000
        // cap = 100_000_000 → exceeds
        let fc = Address::with_last_byte(0xAA);
        let mut tx = make_test_tx(Some(fc), 30_000_000, 10, 1, Address::with_last_byte(1));

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };
        let result = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            0,
            0,
            Some(100_000_000),
            &no_pooled_txs,
            SpecId::PRAGUE,
        );
        assert!(
            matches!(result, Err(CeloPoolRejection::ExceedsFeeCap { .. })),
            "High gas CIP-64 tx should exceed fee cap after rate conversion; got {result:?}"
        );
    }

    #[test]
    fn test_cip64_high_gas_limit_within_cap_due_to_favorable_rate() {
        // Same high-gas tx, but with a rate that shrinks the native cost below the cap.
        // gas=30_000_000, max_fee=10 FC, rate num=1000 denom=1 → native_max_fee = 10/1000 = 0
        // (integer truncation) → native cost = 0 → within any cap.
        let fc = Address::with_last_byte(0xAA);
        let mut tx = make_test_tx(Some(fc), 30_000_000, 10, 1, Address::with_last_byte(1));

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1000, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };
        let result = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            0,
            0,
            Some(100_000_000),
            &no_pooled_txs,
            SpecId::PRAGUE,
        );
        assert!(result.is_ok(), "Favorable rate should keep cost within cap; got {result:?}");
    }

    // -----------------------------------------------------------------------
    // Gap 2: post-Jovian base fee floor
    // -----------------------------------------------------------------------

    #[test]
    fn test_base_fee_floor_zero_accepts_low_fee_cip64() {
        // Post-Jovian: base_fee_floor = 0 → any max_fee >= 0 passes the floor check.
        // Uses max_fee=100 which would be rejected by the 25 Gwei pre-Jovian floor.
        let fc = Address::with_last_byte(0xAA);
        let mut tx = make_test_tx(Some(fc), 21_000, 100, 10, Address::with_last_byte(1));

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };
        // base_fee_floor = 0 → floor check: 100 < 0 is false → passes
        let result = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            0,
            0,
            None,
            &no_pooled_txs,
            SpecId::PRAGUE,
        );
        assert!(result.is_ok(), "floor=0 should accept any fee; got {result:?}");
    }

    #[test]
    fn test_base_fee_floor_exact_at_limit_accepted() {
        // max_fee == floor exactly → boundary is exclusive (old_fee < floor_fc rejects),
        // so equal is accepted.
        let fc = Address::with_last_byte(0xAA);
        let mut tx = make_test_tx(Some(fc), 21_000, 1000, 10, Address::with_last_byte(1));

        // rate 1:1 → base_fee_floor_fc = 1000 = max_fee → 1000 < 1000 is false → accepted
        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };
        let result = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            1000,
            0,
            None,
            &no_pooled_txs,
            SpecId::PRAGUE,
        );
        assert!(result.is_ok(), "max_fee == floor should be accepted; got {result:?}");
    }

    // -----------------------------------------------------------------------
    // Cumulative per-currency balance tracking tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cumulative_balance_rejects_overdraft() {
        // The sender already has a pending CIP-64 tx spending 10_000 of a
        // 15_000 balance; another 10_000 tx must fail the cumulative check
        // even though it passes the per-tx balance check on its own.
        let fc = Address::with_last_byte(0xAA);
        let sender = Address::with_last_byte(1);
        let balance = U256::from(15_000u64);

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(balance),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };

        // Pooled expenditure 10_000, nothing at this tx's nonce.
        let pooled = |_: Address, _: Address, _: u64| (U256::from(10_000u64), U256::ZERO);

        // gas=100, max_fee=100 → required_fc = 10_000; 10_000 + 10_000 > 15_000 → reject
        let mut tx = make_test_tx(Some(fc), 100, 100, 10, sender);
        let r = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            0,
            0,
            None,
            &pooled,
            SpecId::PRAGUE,
        );
        assert!(
            matches!(r, Err(CeloPoolRejection::InsufficientBalance { cumulative: true, .. })),
            "tx exceeding pooled expenditure + balance must be rejected; got {r:?}"
        );
    }

    /// Regression test for <https://github.com/celo-org/celo-kona/issues/250>:
    /// the cost of a tx that was *replaced* in the pool (fee bump) must not
    /// count against later txs from the same sender/currency.
    ///
    /// Sequence (one sender, one currency, balance = 5_000_000, gas = 21_000):
    ///   1. tx1 (nonce N, max_fee 100 → cost 2_100_000) admitted into an empty pool.
    ///   2. tx2 (nonce N, max_fee 120 → cost 2_520_000) admitted; the pool replaces tx1 with it, so
    ///      tx1's cost is no longer outstanding.
    ///   3. tx3 (nonce N+1, max_fee 100 → cost 2_100_000): true outstanding obligation is tx2 + tx3
    ///      = 4_620_000 ≤ 5_000_000, so it MUST be admitted — op-geth admits it
    ///      (`ExistingExpenditure` reads the live pending list, which no longer contains tx1).
    ///
    /// The pooled-expenditure closure models what the live pool returns at
    /// each step (the end-to-end version of this sequence runs against a real
    /// pool in `integration_tests`). The old reservation cache double-counted
    /// tx1 and falsely rejected tx3 with `required: 6_720_000`.
    #[test]
    fn test_issue_250_replaced_tx_does_not_count_against_later_txs() {
        let fc = Address::with_last_byte(0xAA);
        let sender = Address::with_last_byte(1);
        let balance = U256::from(5_000_000u64);
        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(balance),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };
        let validate = |max_fee: u128, pooled: &dyn Fn(Address, Address, u64) -> (U256, U256)| {
            let mut tx = make_test_tx(Some(fc), 21_000, max_fee, 1, sender);
            apply_exchange_rates_to_pool_tx(
                &mock,
                &mut tx,
                Address::ZERO,
                0,
                0,
                None,
                pooled,
                SpecId::PRAGUE,
            )
        };

        // 1. Empty pool.
        let r1 = validate(100, &no_pooled_txs);
        assert!(r1.is_ok(), "tx1 must be admitted; got {r1:?}");
        // 2. Replacement: the pool holds tx1 (2_100_000) at the same nonce, so only the fee bump
        //    counts.
        let pool_holds_tx1 =
            |_: Address, _: Address, _: u64| (U256::from(2_100_000u64), U256::from(2_100_000u64));
        let r2 = validate(120, &pool_holds_tx1);
        assert!(r2.is_ok(), "tx2 (replacement) must be admitted; got {r2:?}");
        // 3. The pool replaced tx1 with tx2 — only tx2 is outstanding, at a different nonce than
        //    tx3.
        let pool_holds_tx2 =
            |_: Address, _: Address, _: u64| (U256::from(2_520_000u64), U256::ZERO);
        let r3 = validate(100, &pool_holds_tx2);
        assert!(r3.is_ok(), "tx3 is payable and must be admitted; got {r3:?}");
    }

    /// A replacement only pays for its fee *bump*: with pooled txs close to
    /// the balance, replacing one of them at a higher fee must be admitted as
    /// long as `spent + (new_cost − replaced_cost)` fits — while the same tx
    /// NOT replacing anything (no same-nonce, same-currency pooled tx) must
    /// be rejected. Mirrors op-geth's `ExistingCost` credit.
    #[test]
    fn test_replacement_counts_only_fee_bump() {
        let fc = Address::with_last_byte(0xAA);
        let sender = Address::with_last_byte(1);
        // Pooled: two txs of 2_100_000 (spent = 4_200_000); balance 4_700_000.
        // Replacement of one of them costs 2_520_000:
        //   bump-aware: 4_200_000 + 2_520_000 − 2_100_000 = 4_620_000 ≤ 4_700_000 → admit
        //   no credit:  4_200_000 + 2_520_000             = 6_720_000 > 4_700_000 → reject
        let balance = U256::from(4_700_000u64);
        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(balance),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };
        let validate = |pooled: &dyn Fn(Address, Address, u64) -> (U256, U256)| {
            let mut tx = make_test_tx(Some(fc), 21_000, 120, 1, sender);
            apply_exchange_rates_to_pool_tx(
                &mock,
                &mut tx,
                Address::ZERO,
                0,
                0,
                None,
                pooled,
                SpecId::PRAGUE,
            )
        };

        let replacing =
            |_: Address, _: Address, _: u64| (U256::from(4_200_000u64), U256::from(2_100_000u64));
        let r = validate(&replacing);
        assert!(r.is_ok(), "replacement must only pay its fee bump; got {r:?}");

        let not_replacing = |_: Address, _: Address, _: u64| (U256::from(4_200_000u64), U256::ZERO);
        let r = validate(&not_replacing);
        assert!(
            matches!(r, Err(CeloPoolRejection::InsufficientBalance { cumulative: true, .. })),
            "without a replacement credit the same tx must be rejected; got {r:?}"
        );
    }

    /// The admission check must query pooled expenditure for exactly the
    /// validated tx's (sender, fee_currency, nonce) — the per-(sender,
    /// currency) independence of the cumulative check lives in the pool read,
    /// so passing the wrong key would leak expenditure across senders or
    /// currencies.
    #[test]
    fn test_pooled_costs_queried_with_sender_currency_and_nonce() {
        let fc = Address::with_last_byte(0xAA);
        let sender = Address::with_last_byte(1);
        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };

        let queried = std::cell::Cell::new(None);
        let pooled = |sender: Address, fc: Address, nonce: u64| {
            queried.set(Some((sender, fc, nonce)));
            (U256::ZERO, U256::ZERO)
        };

        let mut tx = make_test_tx(Some(fc), 100, 100, 10, sender);
        let expected_nonce = tx.nonce();
        let r = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            0,
            0,
            None,
            &pooled,
            SpecId::PRAGUE,
        );
        assert!(r.is_ok(), "tx should be admitted; got {r:?}");
        assert_eq!(
            queried.get(),
            Some((sender, fc, expected_nonce)),
            "pooled expenditure must be queried with the tx's sender, currency, and nonce"
        );
    }

    /// Boundary: the cumulative check is exclusive — spending exactly the full
    /// balance (`spent + required − prev == balance`) must be admitted.
    #[test]
    fn test_cumulative_balance_boundary_exact_spend_admitted() {
        let fc = Address::with_last_byte(0xAA);
        let sender = Address::with_last_byte(1);
        // required_fc = 100 * 100 = 10_000; spent = 15_000, prev = 0 → needed = 25_000 == balance.
        let balance = U256::from(25_000u64);
        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(balance),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };
        let pooled = |_: Address, _: Address, _: u64| (U256::from(15_000u64), U256::ZERO);

        let mut tx = make_test_tx(Some(fc), 100, 100, 10, sender);
        let r = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            0,
            0,
            None,
            &pooled,
            SpecId::PRAGUE,
        );
        assert!(r.is_ok(), "spending exactly the full balance must be admitted; got {r:?}");
    }

    // -----------------------------------------------------------------------
    // sum_pooled_fc_costs — the accounting fold behind PooledFcCostsFn
    // -----------------------------------------------------------------------

    /// Wrap a test tx the way the pool stores it, so the fold runs over the
    /// same shape it sees in production.
    fn pooled_entry(tx: CeloPoolTx) -> Arc<ValidPoolTransaction<CeloPoolTx>> {
        use reth_transaction_pool::{
            TransactionOrigin,
            identifier::{SenderId, TransactionId},
        };
        Arc::new(ValidPoolTransaction {
            transaction_id: TransactionId::new(SenderId::from(1), tx.nonce()),
            propagate: false,
            timestamp: std::time::Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: None,
            transaction: tx,
        })
    }

    #[test]
    fn test_sum_pooled_fc_costs_sums_same_currency_only() {
        let fc = Address::with_last_byte(0xAA);
        let other_fc = Address::with_last_byte(0xBB);
        let sender = Address::with_last_byte(1);

        let txs = vec![
            // Two txs paying in `fc`: 100*100 + 200*100 = 30_000.
            pooled_entry(make_test_tx_with_nonce(Some(fc), 0, 100, 100, 10, sender)),
            pooled_entry(make_test_tx_with_nonce(Some(fc), 1, 200, 100, 10, sender)),
            // Different currency and native: both excluded from the sum.
            pooled_entry(make_test_tx_with_nonce(Some(other_fc), 2, 300, 100, 10, sender)),
            pooled_entry(make_test_tx_with_nonce(None, 3, 400, 100, 10, sender)),
        ];

        // Nonce 99 matches nothing — no replacement credit.
        let (spent, prev) = sum_pooled_fc_costs(&txs, fc, 99);
        assert_eq!(spent, U256::from(30_000u64), "only same-currency txs count");
        assert_eq!(prev, U256::ZERO, "no pooled tx at the queried nonce");
    }

    #[test]
    fn test_sum_pooled_fc_costs_credits_same_nonce_same_currency() {
        let fc = Address::with_last_byte(0xAA);
        let sender = Address::with_last_byte(1);

        let txs = vec![
            pooled_entry(make_test_tx_with_nonce(Some(fc), 0, 100, 100, 10, sender)),
            pooled_entry(make_test_tx_with_nonce(Some(fc), 1, 200, 100, 10, sender)),
        ];

        // Querying at nonce 1 credits that tx's cost (200*100) while it still
        // counts in `spent` — the caller nets it out (spent + required − prev).
        let (spent, prev) = sum_pooled_fc_costs(&txs, fc, 1);
        assert_eq!(spent, U256::from(30_000u64));
        assert_eq!(prev, U256::from(20_000u64), "same-nonce same-currency tx must be credited");
    }

    /// op-geth parity (`ExistingCost` zeroes the credit when the pooled tx at
    /// the nonce pays in a different currency): replacing a tx that pays in
    /// another currency frees nothing in *this* currency, so no credit.
    #[test]
    fn test_sum_pooled_fc_costs_no_credit_for_different_currency_at_nonce() {
        let fc = Address::with_last_byte(0xAA);
        let other_fc = Address::with_last_byte(0xBB);
        let sender = Address::with_last_byte(1);

        let txs = vec![
            pooled_entry(make_test_tx_with_nonce(Some(fc), 0, 100, 100, 10, sender)),
            pooled_entry(make_test_tx_with_nonce(Some(other_fc), 1, 200, 100, 10, sender)),
        ];

        let (spent, prev) = sum_pooled_fc_costs(&txs, fc, 1);
        assert_eq!(spent, U256::from(10_000u64), "other-currency tx not in the sum");
        assert_eq!(prev, U256::ZERO, "other-currency tx at the nonce must not be credited");
    }

    #[test]
    fn test_sum_pooled_fc_costs_empty() {
        let fc = Address::with_last_byte(0xAA);
        assert_eq!(sum_pooled_fc_costs(&[], fc, 0), (U256::ZERO, U256::ZERO));
    }

    /// Test that the eviction filter logic in `on_new_block` correctly
    /// identifies CIP-64 txs whose fee currency was deregistered while
    /// leaving native txs and txs with still-registered currencies alone.
    #[test]
    fn eviction_filter_targets_deregistered_currencies() {
        use reth_transaction_pool::PoolTransaction;

        let fc_a = Address::with_last_byte(0xA0);
        let fc_b = Address::with_last_byte(0xB0);
        let sender = Address::with_last_byte(1);

        let native_tx = make_test_tx(None, 21_000, 1_000_000_000, 100, sender);
        let cip64_a = make_test_tx(Some(fc_a), 21_000, 1_000_000_000, 100, sender);
        let cip64_b = make_test_tx(Some(fc_b), 21_000, 1_000_000_000, 100, sender);

        // Simulate: previously registered = {A, B}, new = {A} → removed = {B}
        let old: std::collections::HashSet<_> = [fc_a, fc_b].into_iter().collect();
        let new: std::collections::HashSet<_> = [fc_a].into_iter().collect();
        let removed: std::collections::HashSet<_> = old.difference(&new).copied().collect();

        // Apply the same filter used in on_new_block
        let txs = [&native_tx, &cip64_a, &cip64_b];
        let to_evict: Vec<_> = txs
            .iter()
            .filter_map(|tx| {
                let fc = tx.fee_currency()?;
                if removed.contains(&fc) { Some(*tx.hash()) } else { None }
            })
            .collect();

        assert_eq!(to_evict.len(), 1, "only CIP-64-B tx should be evicted");
        assert_eq!(to_evict[0], *cip64_b.hash());
    }

    /// Test that the uninitialized-cache filter (used after a failed
    /// startup query) evicts pooled CIP-64 txs whose currency is not in
    /// the freshly observed registry — including currencies removed
    /// before the maintainer ever saw a successful query.
    #[test]
    fn uninitialized_filter_evicts_unregistered_currencies() {
        use reth_transaction_pool::PoolTransaction;

        let fc_a = Address::with_last_byte(0xA0);
        let fc_b = Address::with_last_byte(0xB0);
        let sender = Address::with_last_byte(1);

        let native_tx = make_test_tx(None, 21_000, 1_000_000_000, 100, sender);
        let cip64_a = make_test_tx(Some(fc_a), 21_000, 1_000_000_000, 100, sender);
        let cip64_b = make_test_tx(Some(fc_b), 21_000, 1_000_000_000, 100, sender);

        // Cache uninitialized; first successful query observes only {A}.
        // B is no longer registered (was removed during the startup outage).
        let new: std::collections::HashSet<_> = [fc_a].into_iter().collect();

        let txs = [&native_tx, &cip64_a, &cip64_b];
        let to_evict: Vec<_> = txs
            .iter()
            .filter_map(|tx| {
                let fc = tx.fee_currency()?;
                if !new.contains(&fc) { Some(*tx.hash()) } else { None }
            })
            .collect();

        assert_eq!(to_evict.len(), 1, "only the unregistered CIP-64-B tx should be evicted");
        assert_eq!(to_evict[0], *cip64_b.hash());
    }

    // -----------------------------------------------------------------------
    // Integration: real reth Pool + the production expenditure reader
    // -----------------------------------------------------------------------

    /// These tests run the production admission path against a *real*
    /// `reth_transaction_pool::Pool`, with the expenditure read wired through
    /// the same `pooled_fc_costs_reader` the node builder installs — so
    /// replacement, eviction, and pruning exercise reth's actual pool
    /// mechanics instead of a closure modelling them.
    mod integration_tests {
        use super::*;
        use reth_transaction_pool::{
            CoinbaseTipOrdering, Pool, PoolConfig, TransactionOrigin, blobstore::NoopBlobStore,
            validate::ValidTransaction,
        };

        type TestPool = Pool<StubValidator, CoinbaseTipOrdering<CeloPoolTx>, NoopBlobStore>;

        /// Runs the real CIP-64 admission logic (`apply_exchange_rates_to_pool_tx`)
        /// with a mocked EVM lookup, skipping the inner eth validator's
        /// stateless checks — they are irrelevant to fee-currency accounting.
        struct StubValidator {
            lookup: MockFcLookup,
            pooled_fc_costs: Arc<OnceLock<PooledFcCostsFn>>,
        }

        impl Debug for StubValidator {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.debug_struct("StubValidator").field("lookup", &self.lookup).finish()
            }
        }

        impl TransactionValidator for StubValidator {
            type Transaction = CeloPoolTx;
            type Block = crate::primitives::CeloBlock;

            async fn validate_transaction(
                &self,
                _origin: TransactionOrigin,
                mut transaction: CeloPoolTx,
            ) -> TransactionValidationOutcome<CeloPoolTx> {
                let pooled_fc_costs = self.pooled_fc_costs.get().cloned();
                let result = apply_exchange_rates_to_pool_tx(
                    &self.lookup,
                    &mut transaction,
                    Address::ZERO,
                    0,
                    0,
                    None,
                    &|sender, fc, nonce| {
                        pooled_fc_costs
                            .as_ref()
                            .map_or((U256::ZERO, U256::ZERO), |read| read(sender, fc, nonce))
                    },
                    SpecId::PRAGUE,
                );
                match result {
                    Ok(()) => TransactionValidationOutcome::Valid {
                        balance: U256::MAX,
                        state_nonce: 0,
                        bytecode_hash: None,
                        transaction: ValidTransaction::Valid(transaction),
                        propagate: false,
                        authorities: None,
                    },
                    Err(rejection) => TransactionValidationOutcome::Invalid(
                        transaction,
                        InvalidPoolTransactionError::other(rejection),
                    ),
                }
            }
        }

        /// Pool whose FC lookup reports the given ERC20 balance for every
        /// (sender, currency), rate 1:1, debit always succeeding. The
        /// expenditure slot is filled the same way `CeloPoolBuilder::build_pool`
        /// does it: with `pooled_fc_costs_reader` over the pool itself.
        fn test_pool(balance: u64) -> TestPool {
            let slot: Arc<OnceLock<PooledFcCostsFn>> = Arc::new(OnceLock::new());
            let validator = StubValidator {
                lookup: MockFcLookup {
                    rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
                    balance: Some(U256::from(balance)),
                    debit_ok: Some(true),
                    intrinsic_gas: None,
                },
                pooled_fc_costs: slot.clone(),
            };
            let pool = Pool::new(
                validator,
                CoinbaseTipOrdering::default(),
                NoopBlobStore::default(),
                PoolConfig::default(),
            );
            let _ = slot.set(pooled_fc_costs_reader(pool.clone()));
            pool
        }

        /// End-to-end regression for issue #250: a replaced tx's cost must not
        /// count against later admissions, while genuine overdrafts are still
        /// rejected.
        #[tokio::test]
        async fn issue_250_replacement_frees_expenditure_in_live_pool() {
            let fc = Address::with_last_byte(0xAA);
            let sender = Address::with_last_byte(1);
            // Costs are gas_limit * max_fee at rate 1:1.
            let pool = test_pool(25_000);

            // Nonces 0 and 1 at 10_000 each: 20_000 of 25_000 committed.
            for nonce in [0, 1] {
                let tx = make_test_tx_with_nonce(Some(fc), nonce, 100, 100, 10, sender);
                pool.add_transaction(TransactionOrigin::External, tx)
                    .await
                    .unwrap_or_else(|e| panic!("nonce {nonce} must be admitted: {e}"));
            }

            // Sanity: a third 10_000 tx overdrafts (30_000 > 25_000).
            let overdraft = make_test_tx_with_nonce(Some(fc), 2, 100, 100, 10, sender);
            let err = pool
                .add_transaction(TransactionOrigin::External, overdraft)
                .await
                .expect_err("overdraft past pooled expenditure must be rejected");
            assert!(err.to_string().contains("cumulative"), "unexpected error: {err}");

            // Replace nonce 1 with a 15% fee bump (11_500; clears the pool's
            // 10% price-bump requirement). The replaced tx's 10_000 is
            // credited, so admission sees 10_000 + 11_500 = 21_500.
            let replacement = make_test_tx_with_nonce(Some(fc), 1, 100, 115, 12, sender);
            pool.add_transaction(TransactionOrigin::External, replacement)
                .await
                .expect("fee-bump replacement must be admitted");

            // The replaced tx's 10_000 must be gone from the live total: a
            // 3_000 tx fits (21_500 + 3_000 ≤ 25_000). The old reservation
            // cache kept it and rejected exactly this tx — issue #250.
            let tx = make_test_tx_with_nonce(Some(fc), 2, 30, 100, 10, sender);
            pool.add_transaction(TransactionOrigin::External, tx)
                .await
                .expect("payable tx after replacement must be admitted");

            // No free lunch either: only 500 headroom is left, so a 1_000 tx
            // is rejected (24_500 + 1_000 > 25_000).
            let tx = make_test_tx_with_nonce(Some(fc), 3, 10, 100, 10, sender);
            let err = pool
                .add_transaction(TransactionOrigin::External, tx)
                .await
                .expect_err("tx past the live pooled expenditure must be rejected");
            assert!(err.to_string().contains("cumulative"), "unexpected error: {err}");
        }

        /// Evicting a pooled tx (the maintainer's `remove_transactions` path)
        /// frees its expenditure immediately — no head-block wait.
        #[tokio::test]
        async fn eviction_frees_expenditure_in_live_pool() {
            let fc = Address::with_last_byte(0xAA);
            let sender = Address::with_last_byte(1);
            let pool = test_pool(25_000);

            let keep = make_test_tx_with_nonce(Some(fc), 0, 100, 100, 10, sender);
            let evict = make_test_tx_with_nonce(Some(fc), 1, 150, 100, 10, sender);
            let evict_hash = *evict.hash();
            pool.add_transaction(TransactionOrigin::External, keep).await.expect("admitted");
            pool.add_transaction(TransactionOrigin::External, evict).await.expect("admitted");

            // 25_000 committed; any further same-currency tx overdrafts.
            let blocked = make_test_tx_with_nonce(Some(fc), 2, 100, 100, 10, sender);
            pool.add_transaction(TransactionOrigin::External, blocked)
                .await
                .expect_err("overdraft while both txs are pooled");

            pool.remove_transactions(vec![evict_hash]);

            // The evicted 15_000 no longer counts: 10_000 + 10_000 ≤ 25_000.
            let tx = make_test_tx_with_nonce(Some(fc), 1, 100, 100, 10, sender);
            pool.add_transaction(TransactionOrigin::External, tx)
                .await
                .expect("tx must be admitted once the evicted cost is freed");
        }

        /// Pruning a mined tx (`prune_transactions`, the canonical-update
        /// path) frees its expenditure; the post-mining ERC20 balance is the
        /// FC lookup's concern, not the pool's.
        #[tokio::test]
        async fn mined_tx_frees_expenditure_in_live_pool() {
            let fc = Address::with_last_byte(0xAA);
            let sender = Address::with_last_byte(1);
            let pool = test_pool(25_000);

            let mined = make_test_tx_with_nonce(Some(fc), 0, 100, 100, 10, sender);
            let mined_hash = *mined.hash();
            pool.add_transaction(TransactionOrigin::External, mined).await.expect("admitted");
            let tx = make_test_tx_with_nonce(Some(fc), 1, 150, 100, 10, sender);
            pool.add_transaction(TransactionOrigin::External, tx).await.expect("admitted");

            let blocked = make_test_tx_with_nonce(Some(fc), 2, 100, 100, 10, sender);
            pool.add_transaction(TransactionOrigin::External, blocked)
                .await
                .expect_err("overdraft while both txs are pooled");

            pool.prune_transactions(vec![mined_hash]);

            // The mined 10_000 no longer counts: 15_000 + 9_000 ≤ 25_000.
            // (gas 90, not 100, so this tx's hash differs from `blocked`'s —
            // `make_test_tx_with_nonce` derives the hash from the fields alone.)
            let tx = make_test_tx_with_nonce(Some(fc), 2, 90, 100, 10, sender);
            pool.add_transaction(TransactionOrigin::External, tx)
                .await
                .expect("tx must be admitted once the mined cost is freed");
        }

        /// Expenditure is keyed by (sender, currency): other senders' txs and
        /// the same sender's txs in other currencies never count.
        #[tokio::test]
        async fn expenditure_isolated_per_sender_and_currency() {
            let fc_a = Address::with_last_byte(0xAA);
            let fc_b = Address::with_last_byte(0xBB);
            let sender_a = Address::with_last_byte(1);
            let sender_b = Address::with_last_byte(2);
            let pool = test_pool(25_000);

            // sender_a commits 20_000 in fc_a.
            for nonce in [0, 1] {
                let tx = make_test_tx_with_nonce(Some(fc_a), nonce, 100, 100, 10, sender_a);
                pool.add_transaction(TransactionOrigin::External, tx).await.expect("admitted");
            }

            // sender_b in fc_a: sender_a's spend must not count against it.
            let tx = make_test_tx_with_nonce(Some(fc_a), 0, 200, 100, 10, sender_b);
            pool.add_transaction(TransactionOrigin::External, tx)
                .await
                .expect("other sender must be unaffected");

            // sender_a in fc_b: the fc_a spend must not count against fc_b.
            let tx = make_test_tx_with_nonce(Some(fc_b), 2, 200, 100, 10, sender_a);
            pool.add_transaction(TransactionOrigin::External, tx)
                .await
                .expect("other currency must be unaffected");

            // And the reverse: the fc_b tx must not have leaked into fc_a's
            // total — 20_000 + 5_000 ≤ 25_000 still fits ...
            let tx = make_test_tx_with_nonce(Some(fc_a), 3, 50, 100, 10, sender_a);
            pool.add_transaction(TransactionOrigin::External, tx)
                .await
                .expect("fc_b spend must not leak into fc_a");
            // ... while an overdraft within fc_a alone is still caught.
            let tx = make_test_tx_with_nonce(Some(fc_a), 4, 10, 100, 10, sender_a);
            pool.add_transaction(TransactionOrigin::External, tx)
                .await
                .expect_err("fc_a overdraft must still be rejected");
        }

        /// A nonce-gapped (queued) same-currency tx counts toward expenditure.
        /// Deliberate divergence from op-geth, whose `TotalCostFor` sums only
        /// the executable pending list: a queued tx is still a real future
        /// obligation, and over-admission is the costlier failure — a tx whose
        /// `debitGasFees` fails at block building gets its whole currency
        /// blocklisted for sequencing.
        #[tokio::test]
        async fn queued_txs_count_toward_expenditure() {
            let fc = Address::with_last_byte(0xAA);
            let sender = Address::with_last_byte(1);
            let pool = test_pool(25_000);

            // Nonce 5 against state nonce 0: parked as queued, 15_000 committed.
            let gapped = make_test_tx_with_nonce(Some(fc), 5, 150, 100, 10, sender);
            pool.add_transaction(TransactionOrigin::External, gapped)
                .await
                .expect("nonce-gapped tx is admitted (queued)");

            // A pending-eligible tx must still respect the queued obligation:
            // 15_000 + 15_000 > 25_000.
            let tx = make_test_tx_with_nonce(Some(fc), 0, 150, 100, 10, sender);
            let err = pool
                .add_transaction(TransactionOrigin::External, tx)
                .await
                .expect_err("queued obligation must count toward expenditure");
            assert!(err.to_string().contains("cumulative"), "unexpected error: {err}");

            // While one that fits alongside it is admitted: 15_000 + 10_000 ≤ 25_000.
            let tx = make_test_tx_with_nonce(Some(fc), 0, 100, 100, 10, sender);
            pool.add_transaction(TransactionOrigin::External, tx).await.expect("fits");
        }

        /// A base-fee-parked same-currency tx counts toward expenditure — the
        /// motivation for summing all subpools: op-geth keeps below-base-fee
        /// txs in the pending list its `TotalCostFor` sums, so a pending-only
        /// read would let parked obligations go uncounted and over-admit.
        #[tokio::test]
        async fn basefee_parked_txs_count_toward_expenditure() {
            use reth_transaction_pool::{BlockInfo, TransactionPoolExt};

            let fc = Address::with_last_byte(0xAA);
            let sender = Address::with_last_byte(1);
            let pool = test_pool(25_000);

            // Admit a tx (15_000) at max_fee 100, then raise the pool's
            // enforced base fee above it: the tx is demoted to the basefee
            // subpool.
            let parked = make_test_tx_with_nonce(Some(fc), 0, 150, 100, 10, sender);
            pool.add_transaction(TransactionOrigin::External, parked).await.expect("admitted");
            pool.set_block_info(BlockInfo {
                last_seen_block_hash: B256::ZERO,
                last_seen_block_number: 0,
                block_gas_limit: 30_000_000,
                pending_basefee: 1_000,
                pending_blob_fee: None,
            });
            assert_eq!(
                pool.pool_size().basefee,
                1,
                "tx must be parked in the basefee subpool for this test to mean anything"
            );

            // A tx clearing the base fee must still respect the parked
            // obligation: 15_000 + 11_000 > 25_000.
            let tx = make_test_tx_with_nonce(Some(fc), 1, 10, 1_100, 1_050, sender);
            let err = pool
                .add_transaction(TransactionOrigin::External, tx)
                .await
                .expect_err("base-fee-parked obligation must count toward expenditure");
            assert!(err.to_string().contains("cumulative"), "unexpected error: {err}");

            // While one that fits alongside it is admitted: 15_000 + 10_000 ≤ 25_000.
            let tx = make_test_tx_with_nonce(Some(fc), 1, 10, 1_000, 950, sender);
            pool.add_transaction(TransactionOrigin::External, tx).await.expect("fits");
        }
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    // Differential test against a u256 oracle. The contract: `to_native`/`to_fc`
    // saturate to `u128::MAX` *only* when the true mathematical result exceeds
    // `u128`. A naive u128 implementation saturates whenever the intermediate
    // product overflows — even when the final quotient fits — so this is the
    // test that catches a regression to that form.
    fn oracle_to_native(rate: ExchangeRate, amount: u128) -> u128 {
        let r = (U256::from(amount) * U256::from(rate.denominator)) / U256::from(rate.numerator);
        u128::try_from(r).unwrap_or(u128::MAX)
    }

    fn oracle_to_fc(rate: ExchangeRate, amount: u128) -> u128 {
        let r = (U256::from(amount) * U256::from(rate.numerator)) / U256::from(rate.denominator);
        u128::try_from(r).unwrap_or(u128::MAX)
    }

    fn rate_strategy() -> impl Strategy<Value = ExchangeRate> {
        (any::<u128>(), any::<u128>())
            .prop_filter("nonzero", |(n, d)| *n > 0 && *d > 0)
            .prop_map(|(numerator, denominator)| ExchangeRate { numerator, denominator })
    }

    proptest! {
        #[test]
        fn prop_to_native_matches_oracle(
            rate in rate_strategy(),
            amount in any::<u128>(),
        ) {
            prop_assert_eq!(
                rate.to_native(Fc::new(amount)).into_inner(),
                oracle_to_native(rate, amount),
                "to_native: rate {}/{}, amount {}",
                rate.numerator, rate.denominator, amount,
            );
        }

        #[test]
        fn prop_to_fc_matches_oracle(
            rate in rate_strategy(),
            amount in any::<u128>(),
        ) {
            prop_assert_eq!(
                rate.to_fc(Native::new(amount)).into_inner(),
                oracle_to_fc(rate, amount),
                "to_fc: rate {}/{}, amount {}",
                rate.numerator, rate.denominator, amount,
            );
        }
    }
}
