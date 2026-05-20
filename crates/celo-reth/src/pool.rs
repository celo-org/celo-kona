//! Celo fee-currency-aware transaction pool types.
//!
//! Provides [`CeloPoolTx`], a wrapper around [`OpPooledTransaction`] that converts
//! CIP-64 fee-currency gas prices to native equivalents. This ensures the pool's
//! pending/queued classification and replacement logic work correctly for transactions
//! that pay fees in non-native currencies.

use crate::{primitives::CeloTransactionSigned, signed_tx::CeloConsensusTx};
use alloy_consensus::Transaction;
use alloy_eips::{
    Typed2718, eip2930::AccessList, eip4844::BlobTransactionValidationError,
    eip7594::BlobTransactionSidecarVariant,
};
use alloy_primitives::{Address, B256, Bytes, TxHash, TxKind, U256};
use celo_alloy_consensus::CeloPooledTransaction;
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
    validate::ValidTransaction,
};
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
    /// Convert a fee-currency amount to native equivalent.
    ///
    /// Saturates to `u128::MAX` only when the true mathematical result exceeds
    /// `u128`.
    ///
    /// # Panics
    ///
    /// Panics if `numerator` is zero. This is a logic error — the constructor
    /// rejects zero numerator/denominator rates.
    pub fn to_native(&self, amount: u128) -> u128 {
        assert!(self.numerator != 0, "ExchangeRate numerator must not be zero");
        mul_div_saturating(amount, self.denominator, self.numerator)
    }

    /// Convert a native amount to fee-currency equivalent.
    ///
    /// Saturates to `u128::MAX` only when the true mathematical result exceeds
    /// `u128`.
    ///
    /// # Panics
    ///
    /// Panics if `denominator` is zero. This is a logic error — the constructor
    /// rejects zero numerator/denominator rates.
    pub fn to_fc(&self, native_amount: u128) -> u128 {
        assert!(self.denominator != 0, "ExchangeRate denominator must not be zero");
        mul_div_saturating(native_amount, self.numerator, self.denominator)
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
    /// Native-equivalent max_fee_per_gas.
    ///
    /// For non-CIP-64 txs: same as `inner.max_fee_per_gas()`.
    /// For CIP-64 txs: initially FC-denominated; updated to native equivalent by
    /// [`Self::apply_exchange_rate`].
    native_max_fee_per_gas: u128,
    /// Native-equivalent max_priority_fee_per_gas.
    ///
    /// For non-CIP-64 txs: same as `inner.max_priority_fee_per_gas()`.
    /// For CIP-64 txs: initially FC-denominated; updated to native equivalent by
    /// [`Self::apply_exchange_rate`].
    native_max_priority_fee_per_gas: Option<u128>,
    /// Cached fee currency address (avoids deep-cloning the tx envelope on each access).
    fee_currency: Option<Address>,
    /// Cost checked against the sender's native CELO balance.
    ///
    /// - Non-CIP-64 txs: same as `inner.cost()` (`gas_limit * max_fee + value`).
    /// - CIP-64 txs: just `value`. Gas is paid in the fee currency and is checked separately
    ///   against the sender's ERC20 balance, so it must not be added to the native-CELO cost —
    ///   doing so would reject otherwise-valid txs whose sender has plenty of ERC20 balance but
    ///   only `value` worth of CELO.
    native_cost: U256,
}

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
    /// Create a new [`CeloPoolTx`] with raw (unconverted) fee values.
    pub fn new(inner: InnerPoolTx) -> Self {
        let native_max_fee_per_gas = inner.max_fee_per_gas();
        let native_max_priority_fee_per_gas = inner.max_priority_fee_per_gas();
        let fee_currency = extract_fee_currency(&inner);
        // For CIP-64 txs the raw `inner.cost()` is `gas_limit * max_fee + value`
        // in *fee-currency* units — treating that as native CELO would reject
        // valid txs whose sender funded gas with ERC20 but holds only `value`
        // in CELO. Use just `value`: gas is paid in the fee currency and is
        // checked separately against the sender's ERC20 balance, so it must
        // not be added to the native-CELO requirement.
        let native_cost = if fee_currency.is_some() { inner.value() } else { *inner.cost() };
        Self {
            inner,
            native_max_fee_per_gas,
            native_max_priority_fee_per_gas,
            fee_currency,
            native_cost,
        }
    }

    /// Apply an exchange rate to convert fee-currency values to native equivalents.
    ///
    /// No-op for native (non-CIP-64) transactions — their fee fields are already
    /// denominated in native CELO. `native_cost` is intentionally left unchanged:
    /// CIP-64 gas is paid in the fee currency, so the native-CELO requirement is
    /// just `value` and was seeded correctly by [`Self::new`].
    pub fn apply_exchange_rate(&mut self, rate: ExchangeRate) {
        if self.fee_currency.is_none() {
            return;
        }
        self.native_max_fee_per_gas = rate.to_native(self.inner.max_fee_per_gas());
        self.native_max_priority_fee_per_gas =
            self.inner.max_priority_fee_per_gas().map(|v| rate.to_native(v));
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
        self.native_max_fee_per_gas
    }
    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.native_max_priority_fee_per_gas
    }
    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        self.inner.max_fee_per_blob_gas()
    }
    fn priority_fee_or_price(&self) -> u128 {
        // The `None` branch is unreachable for CIP-64 txs (always EIP-1559 style with
        // priority fee set). For non-CIP-64 txs, `native_max_priority_fee_per_gas` is
        // copied from inner unchanged, so the fallback is only hit for legacy txs.
        self.native_max_priority_fee_per_gas.unwrap_or_else(|| self.inner.priority_fee_or_price())
    }
    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        base_fee.map_or(self.native_max_fee_per_gas, |base_fee| {
            let tip = self.native_max_fee_per_gas.saturating_sub(base_fee as u128);
            if let Some(max_prio) = self.native_max_priority_fee_per_gas &&
                tip > max_prio
            {
                return max_prio + base_fee as u128;
            }
            self.native_max_fee_per_gas
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
        let Self {
            inner,
            native_max_fee_per_gas,
            native_max_priority_fee_per_gas,
            fee_currency,
            native_cost,
        } = self;
        inner.size() +
            core::mem::size_of_val(native_max_fee_per_gas) +
            core::mem::size_of_val(native_max_priority_fee_per_gas) +
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
        let native_max_fee = self.native_max_fee_per_gas;
        let native_max_priority_fee = self.native_max_priority_fee_per_gas.unwrap_or(0);
        self.inner.clone_into_consensus().map(|tx| {
            CeloConsensusTx::with_native_fees(
                tx.into_envelope(),
                native_max_fee,
                native_max_priority_fee,
            )
        })
    }

    fn consensus_ref(&self) -> Recovered<&Self::Consensus> {
        // `CeloConsensusTx` carries native-equivalent fees synthesised from
        // `CeloPoolTx` fields, so satisfying the borrow signature would require
        // materialising the wrapper somewhere stable on `Self`. As of the current
        // reth/op-reth dep tree no caller invokes this method (verified via grep
        // across reth-transaction-pool and op-reth). If a future bump introduces
        // a real caller, this will panic loudly — at which point switch to either
        // a `OnceLock` cache or eager synthesis in `new`/`apply_exchange_rate`.
        unimplemented!(
            "CeloPoolTx::consensus_ref is not implemented; use clone_into_consensus or into_consensus"
        )
    }

    fn into_consensus(self) -> Recovered<Self::Consensus> {
        // Carry the pool-validator-computed native-equivalent fee values onto the
        // consensus wrapper. This is the whole point of `CeloConsensusTx`:
        // op-reth's payload builder calls `effective_tip_per_gas(base_fee_in_wei)`
        // on the consensus tx *after* `into_consensus()`, and for CIP-64 the
        // envelope's own `max_fee_per_gas` is fee-currency-denominated and can't
        // be compared to the native base fee. Attaching the cached native fees
        // here lets that trait method return the correct tip for CIP-64.
        let native_max_fee = self.native_max_fee_per_gas;
        let native_max_priority_fee = self.native_max_priority_fee_per_gas.unwrap_or(0);
        self.inner.into_consensus().map(|tx| {
            CeloConsensusTx::with_native_fees(
                tx.into_envelope(),
                native_max_fee,
                native_max_priority_fee,
            )
        })
    }

    fn into_consensus_with2718(
        self,
    ) -> reth_primitives_traits::WithEncoded<Recovered<Self::Consensus>> {
        let native_max_fee = self.native_max_fee_per_gas;
        let native_max_priority_fee = self.native_max_priority_fee_per_gas.unwrap_or(0);
        let with_encoded = self.inner.into_consensus_with2718();
        // Preserve the cached 2718 encoding while re-wrapping the inner
        // CeloConsensusTx with native-equivalent fees. The cached fees are
        // non-consensus (invisible to encoding/hashing), so the bytes remain valid.
        with_encoded.map(|rec| {
            rec.map(|tx| {
                CeloConsensusTx::with_native_fees(
                    tx.into_envelope(),
                    native_max_fee,
                    native_max_priority_fee,
                )
            })
        })
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
        &self.native_cost
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

/// Blanket implementation for any `StateProviderFactory`.
impl<P: StateProviderFactory> FcLookup for P {
    fn lookup_rate_and_balance(
        &self,
        fee_currency: Address,
        fee_currency_directory: Address,
        balance_check: Option<(Address, U256)>,
    ) -> FcLookupResult {
        lookup_rate_and_balance_impl(self, fee_currency, fee_currency_directory, balance_check)
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

fn lookup_rate_and_balance_impl(
    provider: &dyn StateProviderFactory,
    fee_currency: Address,
    fee_currency_directory: Address,
    balance_check: Option<(Address, U256)>,
) -> FcLookupResult {
    use alloy_sol_types::SolCall;
    use celo_revm::{
        CeloBuilder, DefaultCelo,
        contracts::{core_contracts::getExchangeRateCall, erc20::IFeeCurrencyERC20},
    };
    use reth_revm::database::StateProviderDatabase;
    use revm::{Context, context_interface::result::ExecutionResult};

    let state = match provider.latest() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(target: "celo::pool", %e, ?fee_currency, "Failed to get latest state for FC lookup");
            return FcLookupResult { rate: None, balance: None, debit_ok: None };
        }
    };
    let db = StateProviderDatabase::new(state);
    let mut evm = Context::celo().with_db(db).build_celo();

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
        return FcLookupResult { rate, balance: None, debit_ok: None };
    }

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

    FcLookupResult { rate, balance, debit_ok }
}

// ---------------------------------------------------------------------------
// CeloExchangeRateApplier
// ---------------------------------------------------------------------------

/// Type alias for the base fee floor computation closure.
pub type BaseFeeFloorFn = Arc<dyn Fn(&dyn alloy_consensus::BlockHeader, u64) -> u64 + Send + Sync>;

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
    pub fn new(
        inner: V,
        provider: P,
        fee_currency_directory: Address,
        base_fee_floor: u64,
        base_fee_floor_fn: BaseFeeFloorFn,
        minimum_priority_fee: u128,
        tx_fee_cap: Option<u128>,
    ) -> Self {
        Self {
            inner,
            provider,
            fee_currency_directory,
            base_fee_floor: Arc::new(std::sync::atomic::AtomicU64::new(base_fee_floor)),
            base_fee_floor_fn,
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
    /// The transaction fee (`gas_limit * max_fee_per_gas`) exceeds the configured fee cap.
    ExceedsFeeCap {
        max_tx_fee_wei: u128,
        tx_fee_cap_wei: u128,
        /// Set when the tx uses a CIP-64 fee currency.
        fee_currency: Option<Address>,
    },
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

/// Apply exchange rates to a [`ValidTransaction`] if it is a CIP-64 tx.
/// Also checks ERC20 balance (including cumulative tracking across multiple txs
/// from the same sender/currency), minimum tip, and simulates debitGasFees.
fn apply_exchange_rates_to_valid_tx(
    lookup: &dyn FcLookup,
    valid_tx: &mut ValidTransaction<CeloPoolTx>,
    fee_currency_directory: Address,
    base_fee_floor: u64,
    minimum_priority_fee: u128,
    tx_fee_cap: Option<u128>,
    cumulative_fc_costs: &CumulativeFcCosts,
) -> Result<(), CeloPoolRejection> {
    let tx = match valid_tx {
        ValidTransaction::Valid(tx) => tx,
        ValidTransaction::ValidWithSidecar { transaction, .. } => transaction,
    };
    // Tracks whether a cumulative FC reservation was made. If set, the reservation
    // must be rolled back on any subsequent rejection (debit_ok, fee cap) to avoid
    // leaving stale reserved balance in `cumulative_fc_costs`.
    let mut reserved_cumulative: Option<(Address, Address, U256)> = None;
    if let Some(fc) = tx.fee_currency() {
        let max_fee_fc = tx.inner.max_fee_per_gas();
        let max_priority_fee_fc = tx.inner.max_priority_fee_per_gas();

        // Look up exchange rate, check ERC20 balance, and simulate debit
        // in a single EVM instance.
        let required_fc = U256::from(tx.inner.gas_limit()).saturating_mul(U256::from(max_fee_fc));
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

        // Check: fee cap must be >= base fee floor converted to FC.
        let base_fee_floor_fc = rate.to_fc(base_fee_floor as u128);
        if max_fee_fc < base_fee_floor_fc {
            tracing::warn!(
                target: "celo::pool",
                ?fc,
                max_fee_fc,
                base_fee_floor_fc,
                "Rejecting CIP-64 tx: fee cap below base fee floor"
            );
            CeloPoolMetrics::cip64_rejection("below_base_fee_floor");
            return Err(CeloPoolRejection::BelowBaseFeeFloor {
                currency: fc,
                max_fee_fc,
                base_fee_floor_fc,
            });
        }

        // Check: effective tip must meet the minimum tip converted to FC.
        // The effective tip is min(max_fee - base_fee_floor_fc, priority_fee),
        // matching op-geth's EffectiveGasTipIntCmp(minTip, baseFeeFloor).
        let min_tip_fc = rate.to_fc(minimum_priority_fee);
        let actual_priority = max_priority_fee_fc.unwrap_or(0);
        let effective_tip = max_fee_fc.saturating_sub(base_fee_floor_fc).min(actual_priority);
        if effective_tip < min_tip_fc {
            tracing::warn!(
                target: "celo::pool",
                ?fc,
                effective_tip,
                min_tip_fc,
                "Rejecting CIP-64 tx: effective tip below minimum"
            );
            CeloPoolMetrics::cip64_rejection("below_min_tip");
            return Err(CeloPoolRejection::BelowMinTip {
                currency: fc,
                min_tip_fc,
                actual: effective_tip,
            });
        }

        tx.apply_exchange_rate(rate);
        tracing::info!(
            target: "celo::pool",
            ?fc,
            numerator = rate.numerator,
            denominator = rate.denominator,
            max_fee_fc,
            max_fee_native = tx.native_max_fee_per_gas,
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
        let fee_cost = U256::from(tx.gas_limit()).saturating_mul(U256::from(tx.max_fee_per_gas()));
        let max_tx_fee_wei: u128 = fee_cost.try_into().unwrap_or_else(|_| {
            tracing::warn!(
                target: "celo::pool",
                %fee_cost,
                "Fee cost exceeds u128::MAX, clamping — tx likely has extreme fee values"
            );
            u128::MAX
        });
        if max_tx_fee_wei > cap {
            if tx.fee_currency().is_some() {
                CeloPoolMetrics::cip64_rejection("exceeds_fee_cap");
            }
            rollback_cumulative_fc_cost(&reserved_cumulative, cumulative_fc_costs);
            return Err(CeloPoolRejection::ExceedsFeeCap {
                max_tx_fee_wei,
                tx_fee_cap_wei: cap,
                fee_currency: tx.fee_currency(),
            });
        }
    }

    if tx.fee_currency().is_some() {
        CeloPoolMetrics::cip64_accepted();
    }

    // All checks passed — the cumulative reservation (if any) was already
    // committed atomically with the balance check above.
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
        transaction: Self::Transaction,
    ) -> impl core::future::Future<Output = TransactionValidationOutcome<Self::Transaction>> + Send
    {
        let fut = self.inner.validate_transaction(origin, transaction);
        async move {
            let result = fut.await;
            match result {
                TransactionValidationOutcome::Valid {
                    mut transaction,
                    balance,
                    state_nonce,
                    bytecode_hash,
                    propagate,
                    authorities,
                } => {
                    let base_fee_floor =
                        self.base_fee_floor.load(std::sync::atomic::Ordering::Acquire);
                    if let Err(rejection) = apply_exchange_rates_to_valid_tx(
                        &self.provider,
                        &mut transaction,
                        self.fee_currency_directory,
                        base_fee_floor,
                        self.minimum_priority_fee,
                        self.tx_fee_cap,
                        &self.cumulative_fc_costs,
                    ) {
                        TransactionValidationOutcome::Invalid(
                            transaction.into_transaction(),
                            InvalidPoolTransactionError::other(rejection),
                        )
                    } else {
                        TransactionValidationOutcome::Valid {
                            transaction,
                            balance,
                            state_nonce,
                            bytecode_hash,
                            propagate,
                            authorities,
                        }
                    }
                }
                other => other,
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
        use celo_revm::{CeloBuilder, DefaultCelo, contracts::core_contracts::getCurrenciesCall};
        use reth_revm::database::StateProviderDatabase;
        use revm::{Context, context_interface::result::ExecutionResult};

        let state = self.provider.latest().inspect_err(|e| {
            tracing::warn!(target: "celo::pool", %e, "Failed to get latest state for currency query");
        }).ok()?;
        let db = StateProviderDatabase::new(state);
        let mut evm = Context::celo().with_db(db).build_celo();

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

    #[test]
    fn test_exchange_rate_to_native() {
        // 1 fc = 500 native (denominator/numerator = 1000/2 = 500)
        let rate = ExchangeRate { numerator: 2, denominator: 1000 };
        assert_eq!(rate.to_native(100), 50_000);
        assert_eq!(rate.to_native(0), 0);

        // 1 fc = 0.5 native (numerator > denominator)
        let rate = ExchangeRate { numerator: 2000, denominator: 1000 };
        assert_eq!(rate.to_native(100), 50);
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
        let _ = rate.to_native(500);
    }

    #[test]
    fn test_exchange_rate_to_fc() {
        // 1 fc = 500 native (denominator/numerator = 1000/2 = 500)
        // So 1 native = 1/500 fc => native * numerator / denominator
        let rate = ExchangeRate { numerator: 2, denominator: 1000 };
        // to_fc(500) = 500 * 2 / 1000 = 1
        assert_eq!(rate.to_fc(500), 1);
        assert_eq!(rate.to_fc(0), 0);

        // 1 fc = 0.5 native => 1 native = 2 fc
        let rate = ExchangeRate { numerator: 2000, denominator: 1000 };
        // to_fc(100) = 100 * 2000 / 1000 = 200
        assert_eq!(rate.to_fc(100), 200);
    }

    #[test]
    fn test_exchange_rate_to_fc_roundtrip() {
        let rate = ExchangeRate { numerator: 3, denominator: 1000 };
        let native = 1_000_000u128;
        let fc = rate.to_fc(native);
        let back = rate.to_native(fc);
        // Round-trip may lose precision due to integer division, but should be close
        assert!(back <= native);
        assert!(native - back < rate.denominator / rate.numerator + 1);
    }

    #[test]
    #[should_panic(expected = "denominator must not be zero")]
    fn test_exchange_rate_to_fc_zero_denominator_panics() {
        let rate = ExchangeRate { numerator: 1000, denominator: 0 };
        let _ = rate.to_fc(500);
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

    // -----------------------------------------------------------------------
    // MockFcLookup + apply_exchange_rates_to_valid_tx integration tests
    // -----------------------------------------------------------------------

    struct MockFcLookup {
        rate: Option<ExchangeRate>,
        /// Raw ERC20 balance to return. `None` means query failed.
        balance: Option<U256>,
        debit_ok: Option<bool>,
    }

    impl FcLookup for MockFcLookup {
        fn lookup_rate_and_balance(
            &self,
            _fee_currency: Address,
            _fee_currency_directory: Address,
            _balance_check: Option<(Address, U256)>,
        ) -> FcLookupResult {
            FcLookupResult { rate: self.rate, balance: self.balance, debit_ok: self.debit_ok }
        }
    }

    fn wrap_valid(tx: CeloPoolTx) -> ValidTransaction<CeloPoolTx> {
        ValidTransaction::Valid(tx)
    }

    fn empty_cumulative_costs() -> CumulativeFcCosts {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[test]
    fn test_apply_rates_successful_conversion() {
        let fc = Address::with_last_byte(0xAA);
        let tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 100, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 2 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
        };

        let result = apply_exchange_rates_to_valid_tx(
            &mock,
            &mut valid,
            Address::ZERO,
            0,
            0,
            None,
            &empty_cumulative_costs(),
        );
        assert!(result.is_ok());

        let tx = match &valid {
            ValidTransaction::Valid(t) => t,
            _ => panic!(),
        };
        // 1_000_000_000 * 2/1 = 2_000_000_000
        assert_eq!(tx.max_fee_per_gas(), 2_000_000_000);
    }

    #[test]
    fn test_apply_rates_unregistered_currency() {
        let fc = Address::with_last_byte(0xAA);
        let tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 100, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        let mock = MockFcLookup { rate: None, balance: None, debit_ok: None };
        let result = apply_exchange_rates_to_valid_tx(
            &mock,
            &mut valid,
            Address::ZERO,
            0,
            0,
            None,
            &empty_cumulative_costs(),
        );
        assert!(matches!(result, Err(CeloPoolRejection::UnregisteredCurrency(_))));
    }

    #[test]
    fn test_apply_rates_insufficient_balance() {
        let fc = Address::with_last_byte(0xAA);
        let tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 100, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::ZERO),
            debit_ok: None,
        };
        let result = apply_exchange_rates_to_valid_tx(
            &mock,
            &mut valid,
            Address::ZERO,
            0,
            0,
            None,
            &empty_cumulative_costs(),
        );
        assert!(matches!(result, Err(CeloPoolRejection::InsufficientBalance { .. })));
    }

    #[test]
    fn test_apply_rates_below_base_fee_floor() {
        let fc = Address::with_last_byte(0xAA);
        // max_fee_per_gas = 100 in FC terms
        let tx = make_test_tx(Some(fc), 21_000, 100, 10, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        // rate: 1:1, base_fee_floor = 25 Gwei
        // base_fee_floor_fc = to_fc(25_000_000_000) = 25_000_000_000
        // 100 < 25_000_000_000 → reject
        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
        };
        let result = apply_exchange_rates_to_valid_tx(
            &mock,
            &mut valid,
            Address::ZERO,
            25_000_000_000,
            0,
            None,
            &empty_cumulative_costs(),
        );
        assert!(matches!(result, Err(CeloPoolRejection::BelowBaseFeeFloor { .. })));
    }

    #[test]
    fn test_apply_rates_below_min_tip() {
        let fc = Address::with_last_byte(0xAA);
        // priority_fee = 5 in FC terms
        let tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 5, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        // rate: 1:1, min_priority_fee = 100
        // min_tip_fc = to_fc(100) = 100
        // 5 < 100 → reject
        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
        };
        let result = apply_exchange_rates_to_valid_tx(
            &mock,
            &mut valid,
            Address::ZERO,
            0,
            100,
            None,
            &empty_cumulative_costs(),
        );
        assert!(matches!(result, Err(CeloPoolRejection::BelowMinTip { .. })));
    }

    #[test]
    fn test_apply_rates_debit_simulation_failed() {
        let fc = Address::with_last_byte(0xAA);
        let tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 100, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(false),
        };
        let result = apply_exchange_rates_to_valid_tx(
            &mock,
            &mut valid,
            Address::ZERO,
            0,
            0,
            None,
            &empty_cumulative_costs(),
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
        let tx = make_test_tx(Some(fc), 21_000, 1000, 100, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        // rate: 1 FC = 0.001 native (numerator=1000, denominator=1)
        // native_max_fee = 1000 * 1/1000 = 1
        // native_cost = 21_000 * 1 = 21_000 wei
        // Cap = 100_000 wei → within cap → accepted
        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1000, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
        };
        let result = apply_exchange_rates_to_valid_tx(
            &mock,
            &mut valid,
            Address::ZERO,
            0,
            0,
            Some(100_000),
            &empty_cumulative_costs(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_fee_cap_cip64_exceeds_cap_after_conversion() {
        let fc = Address::with_last_byte(0xAA);
        // CIP-64 tx: gas=21000, max_fee=1_000_000_000 FC
        let tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 100, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        // rate: 1:1 → native_cost = 21_000 * 1_000_000_000 = 21_000_000_000_000
        // Cap = 1_000_000_000_000 (1000 Gwei) → exceeds → rejected
        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
        };
        let result = apply_exchange_rates_to_valid_tx(
            &mock,
            &mut valid,
            Address::ZERO,
            0,
            0,
            Some(1_000_000_000_000),
            &empty_cumulative_costs(),
        );
        assert!(matches!(result, Err(CeloPoolRejection::ExceedsFeeCap { .. })));
    }

    #[test]
    fn test_fee_cap_native_tx_exceeds_cap() {
        // Native tx: gas=21_000, max_fee=1_000_000_000, value=0
        // cost - value = 21_000 * 1_000_000_000 = 21_000_000_000_000
        let tx = make_test_tx(None, 21_000, 1_000_000_000, 100, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        let mock = MockFcLookup { rate: None, balance: None, debit_ok: None };
        let result = apply_exchange_rates_to_valid_tx(
            &mock,
            &mut valid,
            Address::ZERO,
            0,
            0,
            Some(1_000_000_000_000),
            &empty_cumulative_costs(),
        );
        assert!(matches!(result, Err(CeloPoolRejection::ExceedsFeeCap { .. })));
    }

    #[test]
    fn test_fee_cap_disabled_with_zero_or_none() {
        // Native tx with large cost, but cap disabled (0 or None) → both pass
        for cap in [Some(0), None] {
            let tx = make_test_tx(None, 21_000, 1_000_000_000, 100, Address::with_last_byte(1));
            let mut valid = wrap_valid(tx);

            let mock = MockFcLookup { rate: None, balance: None, debit_ok: None };
            let result = apply_exchange_rates_to_valid_tx(
                &mock,
                &mut valid,
                Address::ZERO,
                0,
                0,
                cap,
                &empty_cumulative_costs(),
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
        let tx = make_test_tx(Some(fc), 21_000, 1_000_000_100, 200, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
        };
        let result = apply_exchange_rates_to_valid_tx(
            &mock,
            &mut valid,
            Address::ZERO,
            1_000_000_000,
            150,
            None,
            &empty_cumulative_costs(),
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
        let tx = make_test_tx(Some(fc), 21_000, 2_000_000_000, 200, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
        };
        let result = apply_exchange_rates_to_valid_tx(
            &mock,
            &mut valid,
            Address::ZERO,
            1_000_000_000,
            100,
            None,
            &empty_cumulative_costs(),
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
        let tx = make_test_tx(Some(fc), 30_000_000, 10, 1, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
        };
        let result = apply_exchange_rates_to_valid_tx(
            &mock,
            &mut valid,
            Address::ZERO,
            0,
            0,
            Some(100_000_000),
            &empty_cumulative_costs(),
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
        let tx = make_test_tx(Some(fc), 30_000_000, 10, 1, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1000, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
        };
        let result = apply_exchange_rates_to_valid_tx(
            &mock,
            &mut valid,
            Address::ZERO,
            0,
            0,
            Some(100_000_000),
            &empty_cumulative_costs(),
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
        let tx = make_test_tx(Some(fc), 21_000, 100, 10, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
        };
        // base_fee_floor = 0 → floor check: 100 < 0 is false → passes
        let result = apply_exchange_rates_to_valid_tx(
            &mock,
            &mut valid,
            Address::ZERO,
            0,
            0,
            None,
            &empty_cumulative_costs(),
        );
        assert!(result.is_ok(), "floor=0 should accept any fee; got {result:?}");
    }

    #[test]
    fn test_base_fee_floor_exact_at_limit_accepted() {
        // max_fee == floor exactly → boundary is exclusive (old_fee < floor_fc rejects),
        // so equal is accepted.
        let fc = Address::with_last_byte(0xAA);
        let tx = make_test_tx(Some(fc), 21_000, 1000, 10, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        // rate 1:1 → base_fee_floor_fc = 1000 = max_fee → 1000 < 1000 is false → accepted
        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
        };
        let result = apply_exchange_rates_to_valid_tx(
            &mock,
            &mut valid,
            Address::ZERO,
            1000,
            0,
            None,
            &empty_cumulative_costs(),
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
        };

        let cumulative = empty_cumulative_costs();

        // First tx: 10_000 <= 15_000 → pass
        let tx1 = make_test_tx(Some(fc), 100, 100, 10, sender);
        let mut valid1 = wrap_valid(tx1);
        let r1 = apply_exchange_rates_to_valid_tx(
            &mock,
            &mut valid1,
            Address::ZERO,
            0,
            0,
            None,
            &cumulative,
        );
        assert!(r1.is_ok(), "First tx should pass; got {r1:?}");

        // Second tx: cumulative 10_000 + 10_000 = 20_000 > 15_000 → reject
        let tx2 = make_test_tx(Some(fc), 100, 100, 10, sender);
        let mut valid2 = wrap_valid(tx2);
        let r2 = apply_exchange_rates_to_valid_tx(
            &mock,
            &mut valid2,
            Address::ZERO,
            0,
            0,
            None,
            &cumulative,
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
        };

        let cumulative = empty_cumulative_costs();

        // (sender_a, fc_a), (sender_b, fc_a) — different senders, same currency
        // (sender_a, fc_a), (sender_a, fc_b) — same sender, different currencies
        for (sender, fc, label) in [
            (sender_a, fc_a, "sender_a/fc_a"),
            (sender_b, fc_a, "sender_b/fc_a"),
            (sender_a, fc_b, "sender_a/fc_b"),
        ] {
            let tx = make_test_tx(Some(fc), 100, 100, 10, sender);
            let mut valid = wrap_valid(tx);
            let r = apply_exchange_rates_to_valid_tx(
                &mock,
                &mut valid,
                Address::ZERO,
                0,
                0,
                None,
                &cumulative,
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
        };

        let cumulative = empty_cumulative_costs();

        // First tx passes (10_000 <= 15_000)
        let tx1 = make_test_tx(Some(fc), 100, 100, 10, sender);
        let mut valid1 = wrap_valid(tx1);
        let r1 = apply_exchange_rates_to_valid_tx(
            &mock,
            &mut valid1,
            Address::ZERO,
            0,
            0,
            None,
            &cumulative,
        );
        assert!(r1.is_ok());

        // Second tx would fail (cumulative 20_000 > 15_000)
        let tx2 = make_test_tx(Some(fc), 100, 100, 10, sender);
        let mut valid2 = wrap_valid(tx2);
        let r2 = apply_exchange_rates_to_valid_tx(
            &mock,
            &mut valid2,
            Address::ZERO,
            0,
            0,
            None,
            &cumulative,
        );
        assert!(matches!(r2, Err(CeloPoolRejection::InsufficientBalance { .. })));

        // Clear (simulates on_new_head_block)
        cumulative.lock().unwrap_or_else(|e| e.into_inner()).clear();

        // Now the same tx passes again
        let tx3 = make_test_tx(Some(fc), 100, 100, 10, sender);
        let mut valid3 = wrap_valid(tx3);
        let r3 = apply_exchange_rates_to_valid_tx(
            &mock,
            &mut valid3,
            Address::ZERO,
            0,
            0,
            None,
            &cumulative,
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
        };
        let cumulative = empty_cumulative_costs();
        let tx1 = make_test_tx(Some(fc), 100, 100, 10, sender);
        let mut valid1 = wrap_valid(tx1);
        let r1 = apply_exchange_rates_to_valid_tx(
            &mock_fail,
            &mut valid1,
            Address::ZERO,
            0,
            0,
            None,
            &cumulative,
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
        };
        let tx2 = make_test_tx(Some(fc), 100, 100, 10, sender);
        let mut valid2 = wrap_valid(tx2);
        let r2 = apply_exchange_rates_to_valid_tx(
            &mock_ok,
            &mut valid2,
            Address::ZERO,
            0,
            0,
            None,
            &cumulative,
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
        let tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 100, sender);
        let mut valid = wrap_valid(tx);
        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance: Some(U256::MAX),
            debit_ok: Some(true),
        };
        let cumulative = empty_cumulative_costs();
        // native_cost = 21_000 * 1_000_000_000 = 2.1e13; cap = 1e12 → rejected
        let result = apply_exchange_rates_to_valid_tx(
            &mock,
            &mut valid,
            Address::ZERO,
            0,
            0,
            Some(1_000_000_000_000),
            &cumulative,
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
                rate.to_native(amount),
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
                rate.to_fc(amount),
                oracle_to_fc(rate, amount),
                "to_fc: rate {}/{}, amount {}",
                rate.numerator, rate.denominator, amount,
            );
        }
    }
}
