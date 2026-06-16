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
use celo_revm::units::{Fc, FcU256, Native, NativeU256};
use op_revm::OpSpecId;
use reth_optimism_txpool::{
    OpPooledTransaction, OpPooledTx, conditional::MaybeConditionalTransaction,
    estimated_da_size::DataAvailabilitySized, interop::MaybeInteropTransaction,
};
use reth_primitives_traits::{InMemorySize, Recovered, SealedBlock};
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::{
    EthBlobTransactionSidecar, EthPoolTransaction, PoolTransaction, TransactionValidationOutcome,
    TransactionValidator,
    error::{InvalidPoolTransactionError, PoolTransactionError},
};
use revm::{interpreter::gas::calculate_initial_tx_gas, primitives::hardfork::SpecId};
use std::{
    borrow::Cow,
    collections::HashMap,
    fmt::Debug,
    sync::{Arc, Mutex},
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

    /// `U256`-backed variant of [`Self::to_native`].
    ///
    /// Used by the RPC layer where rate-scaling needs `checked_mul` on `U256`
    /// to avoid panicking on adversarial on-chain rates. Returns `None` if the
    /// intermediate `amount * denominator` overflows `U256`.
    ///
    /// # Panics
    ///
    /// Panics if `numerator` is zero.
    pub fn to_native_u256(&self, amount: FcU256) -> Option<NativeU256> {
        assert!(self.numerator != 0, "ExchangeRate numerator must not be zero");
        amount
            .into_inner()
            .checked_mul(U256::from(self.denominator))
            .map(|v| NativeU256::new(v / U256::from(self.numerator)))
    }

    /// `U256`-backed variant of [`Self::to_fc`].
    ///
    /// # Panics
    ///
    /// Panics if `denominator` is zero.
    pub fn to_fc_u256(&self, native_amount: NativeU256) -> Option<FcU256> {
        assert!(self.denominator != 0, "ExchangeRate denominator must not be zero");
        native_amount
            .into_inner()
            .checked_mul(U256::from(self.numerator))
            .map(|v| FcU256::new(v / U256::from(self.denominator)))
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
    inner
        .transaction()
        .as_cip64()
        .and_then(|signed| signed.tx().fee_currency)
        .filter(|addr| *addr != Address::ZERO)
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
/// The spec MUST be the chain's active fork rather than the `DefaultCelo`
/// BEDROCK default: fee-currency contracts compiled with a recent Solidity emit
/// post-Merge opcodes (PUSH0 = Shanghai, MCOPY/TLOAD = Cancun, i.e. any standard
/// modern OpenZeppelin ERC20). At BEDROCK those halt with `NotActivated`, so the
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

/// Cumulative fee-currency costs per (sender, fee_currency) pair.
///
/// Tracks the total ERC20 cost of all CIP-64 transactions that have passed
/// pool validation for each sender/currency combination. This prevents a sender
/// from submitting multiple CIP-64 transactions that individually pass the
/// per-tx balance check but collectively exceed their ERC20 balance.
///
/// # Lifecycle
///
/// - **Reserved** atomically with the per-tx balance check (optimistic lock).
/// - **Rolled back** via [`rollback_cumulative_fc_cost`] if admission-time checks fail *after* the
///   reservation (debit simulation, fee cap).
/// - **Not decremented** on post-admission eviction or replacement within a block interval —
///   revalidating every pool event is cost-prohibitive, and subsequent rejections are merely
///   conservative, not wrong. With ~1s block times the staleness window is brief.
/// - **Cleared wholesale** on each new head block, when balances may have changed.
type CumulativeFcCosts = Arc<Mutex<HashMap<(Address, Address), U256>>>;

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
    /// Cumulative per-(sender, fee_currency) ERC20 costs for pending CIP-64 txs.
    cumulative_fc_costs: CumulativeFcCosts,
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
            cumulative_fc_costs: Arc::new(Mutex::new(HashMap::new())),
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
        /// True when rejection is due to cumulative cost across pending txs.
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

/// Undo a cumulative FC reservation that was committed optimistically.
///
/// Called on rejection paths (debit simulation, fee cap) after the reservation
/// was already committed atomically with the balance check.
fn rollback_cumulative_fc_cost(
    reserved: &Option<(Address, Address, U256)>,
    cumulative_fc_costs: &CumulativeFcCosts,
) {
    if let Some((sender, fc, amount)) = reserved {
        let mut costs = cumulative_fc_costs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = costs.get_mut(&(*sender, *fc)) {
            *entry = entry.saturating_sub(*amount);
            // Remove zero entries to keep the map compact and so tests that
            // check `.get(key) == None` still pass after a full rollback.
            if entry.is_zero() {
                costs.remove(&(*sender, *fc));
            }
        }
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
/// On success returns `Some((sender, fee_currency, required_fc))` if a
/// cumulative reservation was staged (the caller must roll it back via
/// [`rollback_cumulative_fc_cost`] if the inner validator subsequently
/// rejects the tx), or `None` if no reservation was made.
#[allow(clippy::too_many_arguments)] // flat pool-admission config; a struct adds no clarity here
fn apply_exchange_rates_to_pool_tx(
    lookup: &dyn FcLookup,
    tx: &mut CeloPoolTx,
    fee_currency_directory: Address,
    base_fee_floor: u64,
    minimum_priority_fee: u128,
    tx_fee_cap: Option<u128>,
    cumulative_fc_costs: &CumulativeFcCosts,
    eth_spec: SpecId,
) -> Result<Option<(Address, Address, U256)>, CeloPoolRejection> {
    // Tracks whether a cumulative FC reservation was made. If set, the reservation
    // must be rolled back on any subsequent rejection (debit_ok, fee cap) to avoid
    // leaving stale reserved balance in `cumulative_fc_costs`.
    let mut reserved_cumulative: Option<(Address, Address, U256)> = None;
    if let Some(fc) = tx.fee_currency() {
        let max_fee_fc = Fc::new(tx.inner.max_fee_per_gas());
        let max_priority_fee_fc: Option<Fc> = tx.inner.max_priority_fee_per_gas().map(Fc::new);

        // Look up exchange rate, check ERC20 balance, and simulate debit
        // in a single EVM instance. `required_fc` stays as raw U256 because
        // FcLookup::lookup_rate_and_balance, CeloPoolRejection::InsufficientBalance,
        // and the cumulative-cost map all speak U256 — the `_fc` suffix is the only
        // denomination marker the type system can't reach.
        let required_fc =
            U256::from(tx.inner.gas_limit()).saturating_mul(U256::from(max_fee_fc.into_inner()));
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

        // Check ERC20 balance result (query already done above).
        // If balance is None (query failed), we allow the tx through —
        // it will be caught during execution.
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

            // Cumulative balance check: ensure the total cost across all pending
            // CIP-64 txs from this sender in this currency doesn't exceed balance.
            //
            // The check and reservation are atomic (single critical section) so
            // concurrent validation threads can't both observe the same old
            // cumulative value, both pass the guard, and then both commit —
            // which would admit txs whose combined cost exceeds the balance.
            //
            // If a later check (debit_ok, fee cap) rejects the tx, we roll
            // back the reservation via `rollback_cumulative_fc_cost`.
            {
                let mut costs = cumulative_fc_costs.lock().unwrap_or_else(|e| e.into_inner());
                let entry = costs.entry((sender, fc)).or_default();
                let cumulative_required = entry.saturating_add(required_fc);
                if cumulative_required > balance {
                    tracing::warn!(
                        target: "celo::pool",
                        ?fc,
                        ?sender,
                        ?required_fc,
                        cumulative = %*entry,
                        ?balance,
                        "Rejecting CIP-64 tx: cumulative fee currency cost exceeds balance"
                    );
                    CeloPoolMetrics::cip64_rejection("cumulative_balance_exceeded");
                    return Err(CeloPoolRejection::InsufficientBalance {
                        currency: fc,
                        sender,
                        required: cumulative_required,
                        balance,
                        cumulative: true,
                    });
                }
                *entry = entry.saturating_add(required_fc);
            }
            reserved_cumulative = Some((sender, fc, required_fc));
        } else {
            // The currency is registered (rate found) and a balance check was requested, so a
            // missing balance here means the balanceOf query failed (reverted / undecodable).
            // Fail closed: such a token cannot pay fees (the execution-time debit would fail
            // too), and admitting it would be a fail-open divergence from upstream's
            // deterministic balance rejection. Safe now that the pool EVM runs at the chain's
            // active spec (see `build_pool_evm`): a modern currency's balanceOf no longer
            // returns None merely because PUSH0 halted at the BEDROCK default.
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
            rollback_cumulative_fc_cost(&reserved_cumulative, cumulative_fc_costs);
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
            rollback_cumulative_fc_cost(&reserved_cumulative, cumulative_fc_costs);
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

    // All checks passed — the cumulative reservation (if any) was already
    // committed atomically with the balance check above. Hand it back to
    // the caller so it can be rolled back if the inner validator rejects.
    Ok(reserved_cumulative)
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
        let prepared = apply_exchange_rates_to_pool_tx(
            &lookup,
            &mut transaction,
            self.fee_currency_directory,
            base_fee_floor,
            self.minimum_priority_fee,
            self.tx_fee_cap,
            &self.cumulative_fc_costs,
            eth_spec,
        );
        let cumulative_fc_costs = self.cumulative_fc_costs.clone();
        // Split into Ok/Err up-front so both branches of the async block
        // share a single concrete future type.
        let staged = match prepared {
            Ok(reserved) => Ok((self.inner.validate_transaction(origin, transaction), reserved)),
            Err(rejection) => Err((transaction, rejection)),
        };
        async move {
            match staged {
                Err((tx, rejection)) => TransactionValidationOutcome::Invalid(
                    tx,
                    InvalidPoolTransactionError::other(rejection),
                ),
                Ok((inner_fut, reserved)) => {
                    let result = inner_fut.await;
                    // If the inner validator rejects after we staged a
                    // cumulative-FC reservation, roll it back so the next
                    // submission from this sender/currency isn't penalised
                    // until the next head-block clear.
                    if !matches!(result, TransactionValidationOutcome::Valid { .. }) {
                        rollback_cumulative_fc_cost(&reserved, &cumulative_fc_costs);
                    }
                    result
                }
            }
        }
    }

    fn on_new_head_block(&self, new_tip_block: &SealedBlock<Self::Block>) {
        self.inner.on_new_head_block(new_tip_block);

        // Clear cumulative fee-currency costs — balances may have changed.
        // Note: a validation task that started before this clear can still insert
        // a stale reservation afterwards. This is benign — the stale entry only
        // makes the cumulative total too high, causing a momentary false rejection
        // (never a false admission), and it self-heals on the next head block.
        self.cumulative_fc_costs.lock().unwrap_or_else(|e| e.into_inner()).clear();

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
        // `getCurrencies` query has no fork-gated semantics, so a modern default
        // spec suffices (and stays forward-compatible if the directory is ever
        // redeployed with PUSH0-emitting bytecode). The maintainer never rejects
        // txs, so unlike the validator it needs no per-head spec tracking.
        let mut evm = build_pool_evm(db, OpSpecId::default());

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
    use crate::test_utils::make_test_tx;

    /// A fee-currency contract compiled with a recent Solidity emits PUSH0
    /// (0x5f, EIP-3855, a Shanghai opcode). The pool's system-call EVM must run
    /// at the chain's active fork, otherwise that bytecode halts `NotActivated`
    /// and the pool's balance/debit/rate pre-checks silently no-op. Build a
    /// minimal contract whose code is `PUSH0; POP; STOP` and confirm it halts at
    /// BEDROCK (pre-Shanghai, the `DefaultCelo` default) but runs at the active
    /// spec once `build_pool_evm` raises it.
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
    fn test_to_native_u256_normal_case() {
        let rate = ExchangeRate { numerator: 2, denominator: 1000 };
        let got = rate.to_native_u256(FcU256::new(U256::from(100u64))).expect("no overflow");
        assert_eq!(got.into_inner(), U256::from(50_000u64));
    }

    #[test]
    fn test_to_fc_u256_normal_case() {
        let rate = ExchangeRate { numerator: 2, denominator: 1000 };
        let got = rate.to_fc_u256(NativeU256::new(U256::from(500u64))).expect("no overflow");
        assert_eq!(got.into_inner(), U256::from(1u64));
    }

    #[test]
    fn test_to_native_u256_returns_none_on_overflow() {
        // numerator = denominator = 1, amount = U256::MAX, so amount * denominator overflows U256.
        let rate = ExchangeRate { numerator: 1, denominator: u128::MAX };
        let huge = FcU256::new(U256::MAX);
        assert!(
            rate.to_native_u256(huge).is_none(),
            "checked_mul must catch overflow on adversarial rate"
        );
    }

    #[test]
    fn test_to_fc_u256_returns_none_on_overflow() {
        let rate = ExchangeRate { numerator: u128::MAX, denominator: 1 };
        let huge = NativeU256::new(U256::MAX);
        assert!(rate.to_fc_u256(huge).is_none());
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

    fn empty_cumulative_costs() -> CumulativeFcCosts {
        Arc::new(Mutex::new(HashMap::new()))
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
            &empty_cumulative_costs(),
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
            &empty_cumulative_costs(),
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
            &empty_cumulative_costs(),
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
            &empty_cumulative_costs(),
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
            &empty_cumulative_costs(),
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
            &empty_cumulative_costs(),
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
        // at the BEDROCK default, so fail-closed wrongly rejected valid txs.
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
            &empty_cumulative_costs(),
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
            &empty_cumulative_costs(),
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
            &empty_cumulative_costs(),
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
            &empty_cumulative_costs(),
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
            &empty_cumulative_costs(),
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
            &empty_cumulative_costs(),
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
            &empty_cumulative_costs(),
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
                &empty_cumulative_costs(),
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
            &empty_cumulative_costs(),
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
            &empty_cumulative_costs(),
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
            &empty_cumulative_costs(),
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
            &empty_cumulative_costs(),
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
            &empty_cumulative_costs(),
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
            &empty_cumulative_costs(),
            SpecId::PRAGUE,
        );
        assert!(result.is_ok(), "max_fee == floor should be accepted; got {result:?}");
    }

    // -----------------------------------------------------------------------
    // Cumulative per-currency balance tracking tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cumulative_balance_rejects_overdraft() {
        // Two CIP-64 txs from same sender/currency, each requiring 60% of balance.
        // First passes, second fails cumulative check.
        let fc = Address::with_last_byte(0xAA);
        let sender = Address::with_last_byte(1);
        // gas=100, max_fee=100 → required_fc = 10_000 per tx
        let balance = U256::from(15_000u64); // 60% each = 10_000, cumulative = 20_000 > 15_000

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(balance),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };

        let cumulative = empty_cumulative_costs();

        // First tx: 10_000 <= 15_000 → pass
        let mut tx1 = make_test_tx(Some(fc), 100, 100, 10, sender);
        let r1 = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx1,
            Address::ZERO,
            0,
            0,
            None,
            &cumulative,
            SpecId::PRAGUE,
        );
        assert!(r1.is_ok(), "First tx should pass; got {r1:?}");

        // Second tx: cumulative 10_000 + 10_000 = 20_000 > 15_000 → reject
        let mut tx2 = make_test_tx(Some(fc), 100, 100, 10, sender);
        let r2 = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx2,
            Address::ZERO,
            0,
            0,
            None,
            &cumulative,
            SpecId::PRAGUE,
        );
        assert!(
            matches!(r2, Err(CeloPoolRejection::InsufficientBalance { cumulative: true, .. })),
            "Second tx should fail cumulative check; got {r2:?}"
        );
    }

    #[test]
    fn test_cumulative_balance_independent_across_senders_and_currencies() {
        // Cumulative costs are tracked per (sender, currency) pair. Verify
        // independence along both axes: different senders with the same currency,
        // and the same sender with different currencies.
        let fc_a = Address::with_last_byte(0xAA);
        let fc_b = Address::with_last_byte(0xBB);
        let sender_a = Address::with_last_byte(1);
        let sender_b = Address::with_last_byte(2);
        let balance = U256::from(15_000u64);

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(balance),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };

        let cumulative = empty_cumulative_costs();

        // (sender_a, fc_a), (sender_b, fc_a) — different senders, same currency
        // (sender_a, fc_a), (sender_a, fc_b) — same sender, different currencies
        for (sender, fc, label) in [
            (sender_a, fc_a, "sender_a/fc_a"),
            (sender_b, fc_a, "sender_b/fc_a"),
            (sender_a, fc_b, "sender_a/fc_b"),
        ] {
            let mut tx = make_test_tx(Some(fc), 100, 100, 10, sender);
            let r = apply_exchange_rates_to_pool_tx(
                &mock,
                &mut tx,
                Address::ZERO,
                0,
                0,
                None,
                &cumulative,
                SpecId::PRAGUE,
            );
            assert!(r.is_ok(), "{label} should pass independently; got {r:?}");
        }
    }

    #[test]
    fn test_cumulative_balance_clears_on_new_head() {
        // After clearing the cumulative map, same sender can submit again.
        let fc = Address::with_last_byte(0xAA);
        let sender = Address::with_last_byte(1);
        let balance = U256::from(15_000u64);

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(balance),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };

        let cumulative = empty_cumulative_costs();

        // First tx passes (10_000 <= 15_000)
        let mut tx1 = make_test_tx(Some(fc), 100, 100, 10, sender);
        let r1 = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx1,
            Address::ZERO,
            0,
            0,
            None,
            &cumulative,
            SpecId::PRAGUE,
        );
        assert!(r1.is_ok());

        // Second tx would fail (cumulative 20_000 > 15_000)
        let mut tx2 = make_test_tx(Some(fc), 100, 100, 10, sender);
        let r2 = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx2,
            Address::ZERO,
            0,
            0,
            None,
            &cumulative,
            SpecId::PRAGUE,
        );
        assert!(matches!(r2, Err(CeloPoolRejection::InsufficientBalance { .. })));

        // Clear (simulates on_new_head_block)
        cumulative.lock().unwrap_or_else(|e| e.into_inner()).clear();

        // Now the same tx passes again
        let mut tx3 = make_test_tx(Some(fc), 100, 100, 10, sender);
        let r3 = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx3,
            Address::ZERO,
            0,
            0,
            None,
            &cumulative,
            SpecId::PRAGUE,
        );
        assert!(r3.is_ok(), "After clear, tx should pass again; got {r3:?}");
    }

    /// If a CIP-64 tx is rejected by a check that runs *after* the cumulative
    /// reservation was staged (debit simulation, fee cap), the cumulative map
    /// must NOT retain the reserved amount — otherwise subsequent valid txs
    /// from the same sender/currency would be spuriously rejected as
    /// `InsufficientBalance` until the next head block clears the map.
    #[test]
    fn test_cumulative_not_reserved_on_debit_sim_rejection() {
        let fc = Address::with_last_byte(0xAA);
        let sender = Address::with_last_byte(1);
        // gas=100, max_fee=100 → required_fc = 10_000 per tx
        let balance = U256::from(30_000u64);

        // First tx: debit simulation fails → rejected
        let mock_fail = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(balance),
            debit_ok: Some(false),
            intrinsic_gas: None,
        };
        let cumulative = empty_cumulative_costs();
        let mut tx1 = make_test_tx(Some(fc), 100, 100, 10, sender);
        let r1 = apply_exchange_rates_to_pool_tx(
            &mock_fail,
            &mut tx1,
            Address::ZERO,
            0,
            0,
            None,
            &cumulative,
            SpecId::PRAGUE,
        );
        assert!(matches!(r1, Err(CeloPoolRejection::DebitSimulationFailed { .. })));
        assert_eq!(
            cumulative.lock().unwrap_or_else(|e| e.into_inner()).get(&(sender, fc)).copied(),
            None,
            "debit-sim rejection must not leave a stale cumulative reservation"
        );

        // Second tx with the SAME balance: debit_ok=true → must succeed,
        // even though a naive pre-check that committed the reservation up front
        // would have left 10_000 reserved from the first tx.
        let mock_ok = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(balance),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };
        let mut tx2 = make_test_tx(Some(fc), 100, 100, 10, sender);
        let r2 = apply_exchange_rates_to_pool_tx(
            &mock_ok,
            &mut tx2,
            Address::ZERO,
            0,
            0,
            None,
            &cumulative,
            SpecId::PRAGUE,
        );
        assert!(r2.is_ok(), "Follow-up tx should pass (no stale reservation); got {r2:?}");
    }

    #[test]
    fn test_cumulative_not_reserved_on_fee_cap_rejection() {
        // CIP-64 tx priced above the fee cap — fee cap check runs after the
        // cumulative reservation is staged, so we need to verify the map is
        // not mutated when that rejection fires.
        let fc = Address::with_last_byte(0xAA);
        let sender = Address::with_last_byte(1);
        let mut tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 100, sender);
        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
            intrinsic_gas: None,
        };
        let cumulative = empty_cumulative_costs();
        // native_cost = 21_000 * 1_000_000_000 = 2.1e13; cap = 1e12 → rejected
        let result = apply_exchange_rates_to_pool_tx(
            &mock,
            &mut tx,
            Address::ZERO,
            0,
            0,
            Some(1_000_000_000_000),
            &cumulative,
            SpecId::PRAGUE,
        );
        assert!(matches!(result, Err(CeloPoolRejection::ExceedsFeeCap { .. })));
        assert_eq!(
            cumulative.lock().unwrap_or_else(|e| e.into_inner()).get(&(sender, fc)).copied(),
            None,
            "fee-cap rejection must not leave a stale cumulative reservation"
        );
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
