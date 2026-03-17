//! Celo fee-currency-aware transaction pool types.
//!
//! Provides [`CeloPoolTx`], a wrapper around [`OpPooledTransaction`] that converts
//! CIP-64 fee-currency gas prices to native equivalents. This ensures the pool's
//! pending/queued classification and replacement logic work correctly for transactions
//! that pay fees in non-native currencies.

use crate::primitives::CeloTransactionSigned;
use alloy_consensus::Transaction;
use alloy_eips::{
    eip2930::AccessList, eip4844::BlobTransactionValidationError,
    eip7594::BlobTransactionSidecarVariant, Typed2718,
};
use alloy_primitives::{Address, B256, Bytes, TxHash, TxKind, U256};
use celo_alloy_consensus::{CeloPooledTransaction, CeloTxEnvelope};
use reth_optimism_txpool::{
    conditional::MaybeConditionalTransaction, estimated_da_size::DataAvailabilitySized,
    interop::MaybeInteropTransaction, OpPooledTransaction, OpPooledTx,
};
use reth_primitives_traits::{InMemorySize, Recovered, SealedBlock};
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::{
    EthBlobTransactionSidecar, EthPoolTransaction, PoolTransaction,
    TransactionValidationOutcome, TransactionValidator,
    error::{InvalidPoolTransactionError, PoolTransactionError},
    validate::ValidTransaction,
};
use std::borrow::Cow;
use std::fmt::Debug;
use std::sync::Arc;

/// Inner OP pool transaction type.
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
    pub fn to_native(&self, amount: u128) -> u128 {
        if self.numerator == 0 {
            return amount;
        }
        amount
            .checked_mul(self.denominator)
            .map(|v| v / self.numerator)
            .unwrap_or(u128::MAX)
    }

    /// Convert a native amount to fee-currency equivalent.
    pub fn to_fc(&self, native_amount: u128) -> u128 {
        if self.denominator == 0 {
            return native_amount;
        }
        native_amount
            .checked_mul(self.numerator)
            .map(|v| v / self.denominator)
            .unwrap_or(u128::MAX)
    }
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
#[derive(Debug, Clone)]
pub struct CeloPoolTx {
    inner: InnerPoolTx,
    /// Native-equivalent max_fee_per_gas. Same as original for non-CIP-64 txs.
    native_max_fee_per_gas: u128,
    /// Native-equivalent max_priority_fee_per_gas.
    native_max_priority_fee_per_gas: Option<u128>,
    /// Cached fee currency address (avoids deep-cloning the tx envelope on each access).
    fee_currency: Option<Address>,
    /// For CIP-64 txs after exchange rate is applied: `gas_limit * native_max_fee + value`.
    /// For non-CIP-64 txs: same as `inner.cost()`.
    /// The pool checks this against native balance, so for CIP-64 txs it must not
    /// include the fee-currency gas cost.
    native_cost: U256,
}

/// Extract the fee currency address from a pool transaction.
/// Only clones the consensus tx for CIP-64 type (0x7b); other types return `None` immediately.
fn extract_fee_currency(inner: &InnerPoolTx) -> Option<Address> {
    // Check type byte first to avoid cloning for non-CIP-64 transactions
    if inner.ty() != celo_alloy_consensus::CeloTxType::Cip64 as u8 {
        return None;
    }
    match inner.clone_into_consensus().into_parts().0 {
        CeloTxEnvelope::Cip64(signed) => signed.tx().fee_currency,
        _ => None,
    }
}

impl CeloPoolTx {
    /// Create a new [`CeloPoolTx`] with raw (unconverted) fee values.
    pub fn new(inner: InnerPoolTx) -> Self {
        let native_max_fee_per_gas = inner.max_fee_per_gas();
        let native_max_priority_fee_per_gas = inner.max_priority_fee_per_gas();
        let fee_currency = extract_fee_currency(&inner);
        let native_cost = *inner.cost();
        Self {
            inner,
            native_max_fee_per_gas,
            native_max_priority_fee_per_gas,
            fee_currency,
            native_cost,
        }
    }

    /// Apply an exchange rate to convert fee-currency values to native equivalents.
    pub fn apply_exchange_rate(&mut self, rate: ExchangeRate) {
        self.native_max_fee_per_gas = rate.to_native(self.inner.max_fee_per_gas());
        self.native_max_priority_fee_per_gas = self
            .inner
            .max_priority_fee_per_gas()
            .map(|v| rate.to_native(v));
        // For CIP-64 txs, recompute cost using native-equivalent fees:
        // gas_limit * native_max_fee + value (gas is paid in FC, only value is in native CELO)
        if self.fee_currency.is_some() {
            self.native_cost = U256::from(self.inner.gas_limit())
                .saturating_mul(U256::from(self.native_max_fee_per_gas))
                .saturating_add(self.inner.value());
        }
    }

    /// Returns the fee currency address if this is a CIP-64 transaction.
    pub fn fee_currency(&self) -> Option<Address> {
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
        self.native_max_priority_fee_per_gas
            .unwrap_or_else(|| self.inner.priority_fee_or_price())
    }
    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        base_fee.map_or(self.native_max_fee_per_gas, |base_fee| {
            let tip = self.native_max_fee_per_gas.saturating_sub(base_fee as u128);
            if let Some(max_prio) = self.native_max_priority_fee_per_gas {
                if tip > max_prio {
                    return max_prio + base_fee as u128;
                }
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
        self.inner.size() + core::mem::size_of::<u128>() * 2 + core::mem::size_of::<U256>()
    }
}

// ---------------------------------------------------------------------------
// PoolTransaction
// ---------------------------------------------------------------------------

impl PoolTransaction for CeloPoolTx {
    type TryFromConsensusError =
        <CeloPooledTransaction as TryFrom<CeloTransactionSigned>>::Error;
    type Consensus = CeloTransactionSigned;
    type Pooled = CeloPooledTransaction;

    fn clone_into_consensus(&self) -> Recovered<Self::Consensus> {
        self.inner.clone_into_consensus()
    }

    fn into_consensus(self) -> Recovered<Self::Consensus> {
        self.inner.into_consensus()
    }

    fn into_consensus_with2718(
        self,
    ) -> reth_primitives_traits::WithEncoded<Recovered<Self::Consensus>> {
        // Note: We can't easily adjust fees inside WithEncoded, but the main
        // code path (execute_best_transactions) uses into_consensus() not this.
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
        Cow::Owned(self.inner.encoded_2718().clone())
    }
}

// ---------------------------------------------------------------------------
// FeeCurrencyDirectory reader
// ---------------------------------------------------------------------------

/// Result of looking up exchange rate, balance, and debit simulation for a fee currency.
pub(crate) struct FcLookupResult {
    pub(crate) rate: Option<ExchangeRate>,
    pub(crate) balance_ok: Option<bool>,
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
    use revm::{Context, SystemCallEvm, context_interface::result::ExecutionResult};

    let state = match provider.latest() {
        Ok(s) => s,
        Err(_) => return FcLookupResult { rate: None, balance_ok: None, debit_ok: None },
    };
    let db = StateProviderDatabase::new(state);
    let mut evm = Context::celo().with_db(db).build_celo();

    // 1. Look up exchange rate
    let rate_calldata = getExchangeRateCall { token: fee_currency }.abi_encode();
    let rate = evm
        .system_call_one(fee_currency_directory, rate_calldata.into())
        .ok()
        .and_then(|result| match result {
            ExecutionResult::Success { output, .. } => Some(output.into_data()),
            _ => None,
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

    // 2. Check ERC20 balance (only if requested)
    let balance_ok = balance_check.as_ref().and_then(|(sender, required)| {
        let bal_calldata =
            IFeeCurrencyERC20::balanceOfCall { account: *sender }.abi_encode();
        let result = evm
            .system_call_one(fee_currency, bal_calldata.into())
            .ok()?;
        let output = match result {
            ExecutionResult::Success { output, .. } => output.into_data(),
            _ => return None,
        };
        let balance =
            IFeeCurrencyERC20::balanceOfCall::abi_decode_returns(&output).ok()?;
        Some(balance >= *required)
    });

    // 3. Simulate debitGasFees (only if balance was sufficient).
    //
    // Catches tokens with custom hooks (pause, blacklist) that would pass
    // the balance check but fail at execution time. Uses Address::ZERO as
    // the caller, matching the `onlyVm` modifier that fee currency contracts
    // require (msg.sender == address(0)).
    let debit_ok = balance_check.and_then(|(sender, required)| {
        if balance_ok != Some(true) {
            return None;
        }
        let debit_calldata = IFeeCurrencyERC20::debitGasFeesCall {
            from: sender,
            value: required,
        }
        .abi_encode();
        match evm.system_call_one_with_caller(
            Address::ZERO,
            fee_currency,
            debit_calldata.into(),
        ) {
            Ok(ExecutionResult::Success { .. }) => Some(true),
            Ok(ExecutionResult::Revert { .. }) => Some(false),
            // Halt or EVM-level error: inconclusive, don't reject
            _ => None,
        }
    });

    FcLookupResult { rate, balance_ok, debit_ok }
}

// ---------------------------------------------------------------------------
// Cip64Rejection — CIP-64 pool transaction error
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// CeloExchangeRateApplier
// ---------------------------------------------------------------------------

/// Wraps a [`TransactionValidator`] and applies fee-currency exchange rates
/// to validated CIP-64 transactions, so that the pool sees native-equivalent
/// gas prices for ordering, replacement, and base fee classification.
pub struct CeloExchangeRateApplier<V, P> {
    inner: V,
    provider: P,
    fee_currency_directory: Address,
    /// Minimum base fee (in native wei) that CIP-64 txs must meet after
    /// FC→native conversion. Set to [`CELO_BASE_FEE_FLOOR`](crate::CELO_BASE_FEE_FLOOR)
    /// for mainnet/testnet, or 0 for dev chains where the floor doesn't apply.
    base_fee_floor: u64,
    /// Minimum priority fee in native wei. CIP-64 txs must have a priority fee
    /// that, when converted to FC units, is at least this value converted to FC.
    minimum_priority_fee: u128,
    /// Maximum transaction fee in wei (cost - value). `None` or `Some(0)` disables the check.
    /// For CIP-64 txs, the native-equivalent cost is used after exchange rate conversion.
    tx_fee_cap: Option<u128>,
}

impl<V: Debug, P> Debug for CeloExchangeRateApplier<V, P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CeloExchangeRateApplier")
            .field("inner", &self.inner)
            .finish()
    }
}

impl<V, P> CeloExchangeRateApplier<V, P> {
    /// Create a new [`CeloExchangeRateApplier`].
    pub fn new(
        inner: V,
        provider: P,
        fee_currency_directory: Address,
        base_fee_floor: u64,
        minimum_priority_fee: u128,
        tx_fee_cap: Option<u128>,
    ) -> Self {
        Self { inner, provider, fee_currency_directory, base_fee_floor, minimum_priority_fee, tx_fee_cap }
    }
}

/// Rejection reason for a CIP-64 pool transaction.
///
/// Implements [`PoolTransactionError`] directly so it can be passed to
/// [`InvalidPoolTransactionError::other`] without separate error structs.
#[derive(Debug)]
enum Cip64Rejection {
    /// The fee currency is not registered in the FeeCurrencyDirectory.
    UnregisteredCurrency(Address),
    /// The sender has insufficient ERC20 balance for the fee currency.
    InsufficientBalance(Address),
    /// The fee cap (in FC terms) is below the base fee floor converted to FC.
    BelowBaseFeeFloor(Address),
    /// The priority fee (in FC terms) is below the minimum tip converted to FC.
    BelowMinTip { currency: Address, min_tip_fc: u128, actual: u128 },
    /// The `debitGasFees()` simulation failed (e.g. token is paused or blacklisted).
    DebitSimulationFailed(Address),
    /// The transaction fee (cost - value) exceeds the configured fee cap.
    ExceedsFeeCap { max_tx_fee_wei: u128, tx_fee_cap_wei: u128 },
}

impl std::fmt::Display for Cip64Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnregisteredCurrency(fc) => {
                write!(f, "unregistered fee-currency address {fc}")
            }
            Self::InsufficientBalance(fc) => {
                write!(f, "insufficient ERC20 balance for fee-currency {fc}")
            }
            Self::BelowBaseFeeFloor(fc) => {
                write!(f, "fee cap below base fee floor for fee-currency {fc}")
            }
            Self::BelowMinTip { currency, min_tip_fc, actual } => {
                write!(
                    f,
                    "CIP-64 priority fee {actual} below minimum {min_tip_fc} for fee-currency {currency}"
                )
            }
            Self::DebitSimulationFailed(fc) => {
                write!(f, "debitGasFees simulation failed for fee-currency {fc}")
            }
            Self::ExceedsFeeCap { max_tx_fee_wei, tx_fee_cap_wei } => {
                write!(
                    f,
                    "tx fee ({max_tx_fee_wei} wei) exceeds the configured cap ({tx_fee_cap_wei} wei)"
                )
            }
        }
    }
}

impl std::error::Error for Cip64Rejection {}

impl PoolTransactionError for Cip64Rejection {
    fn is_bad_transaction(&self) -> bool {
        match self {
            // Insufficient balance is transient — balance may change.
            Self::InsufficientBalance(_) => false,
            // Debit simulation failure is transient — token state may change.
            Self::DebitSimulationFailed(_) => false,
            // Fee cap rejection is permanent — the tx's gas cost won't change.
            Self::ExceedsFeeCap { .. } => true,
            _ => true,
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Apply exchange rates to a [`ValidTransaction`] if it is a CIP-64 tx.
/// Also checks ERC20 balance, minimum tip, and simulates debitGasFees.
fn apply_exchange_rates_to_valid_tx(
    lookup: &dyn FcLookup,
    valid_tx: &mut ValidTransaction<CeloPoolTx>,
    fee_currency_directory: Address,
    base_fee_floor: u64,
    minimum_priority_fee: u128,
    tx_fee_cap: Option<u128>,
) -> Result<(), Cip64Rejection> {
    let tx = match valid_tx {
        ValidTransaction::Valid(tx) => tx,
        ValidTransaction::ValidWithSidecar { transaction, .. } => transaction,
    };
    if let Some(fc) = tx.fee_currency() {
        let old_fee = tx.inner.max_fee_per_gas();
        let old_priority_fee = tx.inner.max_priority_fee_per_gas();

        // Look up exchange rate, check ERC20 balance, and simulate debit
        // in a single EVM instance.
        let required_fc = U256::from(tx.inner.gas_limit())
            .saturating_mul(U256::from(old_fee));
        let sender = tx.sender();
        let result = lookup.lookup_rate_and_balance(
            fc,
            fee_currency_directory,
            Some((sender, required_fc)),
        );

        let rate = match result.rate {
            Some(r) => r,
            None => {
                tracing::warn!(
                    target: "celo::pool",
                    ?fc,
                    "Rejecting CIP-64 tx: unregistered fee currency"
                );
                return Err(Cip64Rejection::UnregisteredCurrency(fc));
            }
        };

        // Check: priority fee must meet the minimum tip converted to FC.
        let min_tip_fc = rate.to_fc(minimum_priority_fee);
        let actual_tip = old_priority_fee.unwrap_or(0);
        if actual_tip < min_tip_fc {
            tracing::warn!(
                target: "celo::pool",
                ?fc,
                actual_tip,
                min_tip_fc,
                "Rejecting CIP-64 tx: priority fee below minimum tip"
            );
            return Err(Cip64Rejection::BelowMinTip {
                currency: fc,
                min_tip_fc,
                actual: actual_tip,
            });
        }

        // Check: fee cap must be >= base fee floor converted to FC.
        let base_fee_floor_fc = rate.to_fc(base_fee_floor as u128);
        if old_fee < base_fee_floor_fc {
            tracing::warn!(
                target: "celo::pool",
                ?fc,
                old_fee,
                base_fee_floor_fc,
                "Rejecting CIP-64 tx: fee cap below base fee floor"
            );
            return Err(Cip64Rejection::BelowBaseFeeFloor(fc));
        }

        tx.apply_exchange_rate(rate);
        tracing::info!(
            target: "celo::pool",
            ?fc,
            numerator = rate.numerator,
            denominator = rate.denominator,
            old_max_fee = old_fee,
            new_max_fee = tx.native_max_fee_per_gas,
            "Applied exchange rate to CIP-64 pool tx"
        );

        // Check ERC20 balance result (query already done above).
        // If balance_ok is None (query failed), we allow the tx through —
        // it will be caught during execution.
        if let Some(false) = result.balance_ok {
            tracing::warn!(
                target: "celo::pool",
                ?fc,
                ?sender,
                ?required_fc,
                "Rejecting CIP-64 tx: insufficient fee currency balance"
            );
            return Err(Cip64Rejection::InsufficientBalance(fc));
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
            return Err(Cip64Rejection::DebitSimulationFailed(fc));
        }
    }

    // Fee cap check: applies to both CIP-64 (using native-equivalent cost) and native txs.
    // For CIP-64 txs, native_cost was recomputed by apply_exchange_rate above.
    if let Some(cap) = tx_fee_cap {
        if cap > 0 {
            let fee_cost = tx.cost().saturating_sub(tx.value());
            let max_tx_fee_wei: u128 = fee_cost.try_into().unwrap_or(u128::MAX);
            if max_tx_fee_wei > cap {
                return Err(Cip64Rejection::ExceedsFeeCap {
                    max_tx_fee_wei,
                    tx_fee_cap_wei: cap,
                });
            }
        }
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
                    if let Err(rejection) =
                        apply_exchange_rates_to_valid_tx(&self.provider, &mut transaction, self.fee_currency_directory, self.base_fee_floor, self.minimum_priority_fee, self.tx_fee_cap)
                    {
                        let tx = match transaction {
                            ValidTransaction::Valid(tx) => tx,
                            ValidTransaction::ValidWithSidecar { transaction, .. } => {
                                transaction
                            }
                        };
                        TransactionValidationOutcome::Invalid(
                            tx,
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
    registered_currencies: std::collections::HashSet<Address>,
}

impl<Pool, P> CeloPoolMaintainer<Pool, P> {
    /// Create a new [`CeloPoolMaintainer`].
    pub fn new(pool: Pool, provider: P, fee_currency_directory: Address) -> Self {
        Self {
            pool,
            provider,
            fee_currency_directory,
            registered_currencies: std::collections::HashSet::new(),
        }
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
        use celo_revm::{
            CeloBuilder, DefaultCelo,
            contracts::core_contracts::getCurrenciesCall,
        };
        use reth_revm::database::StateProviderDatabase;
        use revm::{Context, SystemCallEvm, context_interface::result::ExecutionResult};

        let state = self.provider.latest().ok()?;
        let db = StateProviderDatabase::new(state);
        let mut evm = Context::celo().with_db(db).build_celo();

        let calldata = getCurrenciesCall {}.abi_encode();
        let result = evm
            .system_call_one(self.fee_currency_directory, calldata.into())
            .ok()?;

        match result {
            ExecutionResult::Success { output, .. } => {
                let currencies: Vec<Address> =
                    getCurrenciesCall::abi_decode_returns(&output.into_data()).ok()?;
                Some(currencies.into_iter().collect())
            }
            _ => None,
        }
    }

    /// Run the maintainer, listening for canonical state changes.
    pub async fn run(mut self, mut events: reth_provider::CanonStateNotifications<crate::primitives::CeloPrimitives>) {
        // Initialize the cache
        if let Some(currencies) = self.query_registered_currencies() {
            self.registered_currencies = currencies;
        }

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
                    tracing::info!(
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
            return;
        };

        // Only scan pool if the registered set actually changed.
        if new_currencies == self.registered_currencies {
            return;
        }

        let removed_currencies: std::collections::HashSet<_> = self
            .registered_currencies
            .difference(&new_currencies)
            .copied()
            .collect();

        if !removed_currencies.is_empty() {
            let all_txs = self.pool.all_transactions();
            let to_evict: Vec<TxHash> = all_txs
                .pending
                .iter()
                .chain(all_txs.queued.iter())
                .filter_map(|vtx| {
                    let fc = vtx.transaction.fee_currency()?;
                    if removed_currencies.contains(&fc) {
                        Some(*vtx.hash())
                    } else {
                        None
                    }
                })
                .collect();

            if !to_evict.is_empty() {
                tracing::info!(
                    target: "celo::pool",
                    count = to_evict.len(),
                    ?removed_currencies,
                    "Evicting CIP-64 txs with deregistered fee currencies"
                );
                self.pool.remove_transactions(to_evict);
            }
        }

        self.registered_currencies = new_currencies;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::make_test_tx;

    #[test]
    fn test_exchange_rate_to_native() {
        // 1 fc = 500 native (denominator/numerator = 1000/2 = 500)
        let rate = ExchangeRate {
            numerator: 2,
            denominator: 1000,
        };
        assert_eq!(rate.to_native(100), 50_000);
        assert_eq!(rate.to_native(0), 0);

        // 1 fc = 0.5 native (numerator > denominator)
        let rate = ExchangeRate {
            numerator: 2000,
            denominator: 1000,
        };
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

        let rate = ExchangeRate { numerator: 1, denominator: 2 };
        tx.apply_exchange_rate(rate);

        // fee_currency is None, so apply_exchange_rate converts but native_cost
        // stays the same (no CIP-64 branch)
        assert_eq!(tx.max_fee_per_gas(), rate.to_native(original_fee));
        assert_eq!(tx.max_priority_fee_per_gas(), original_priority.map(|v| rate.to_native(v)));
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
    fn test_native_cost_after_exchange_rate() {
        let fc = Address::with_last_byte(0xCC);
        let mut tx = make_test_tx(Some(fc), 100, 1_000, 10, Address::with_last_byte(1));

        // rate: 1 FC = 0.5 native (numerator=2, denominator=1)
        let rate = ExchangeRate { numerator: 2, denominator: 1 };
        tx.apply_exchange_rate(rate);

        // native_max_fee = 1000 * 1 / 2 = 500
        // native_cost = gas_limit * native_max_fee + value = 100 * 500 + 0 = 50_000
        assert_eq!(*tx.cost(), U256::from(50_000));
    }

    #[test]
    fn test_cost_unchanged_for_native_tx() {
        let tx = make_test_tx(None, 100, 1_000, 10, Address::with_last_byte(1));
        // For native tx: cost = gas_limit * max_fee + value = 100 * 1000 + 0 = 100_000
        assert_eq!(*tx.cost(), U256::from(100_000));
    }

    #[test]
    fn test_exchange_rate_zero_numerator_passthrough() {
        let rate = ExchangeRate { numerator: 0, denominator: 1000 };
        // When numerator is 0, to_native returns the amount unchanged
        assert_eq!(rate.to_native(500), 500);
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
    fn test_exchange_rate_to_fc_zero_denominator_passthrough() {
        let rate = ExchangeRate { numerator: 1000, denominator: 0 };
        assert_eq!(rate.to_fc(500), 500);
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
        balance_ok: Option<bool>,
        debit_ok: Option<bool>,
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
                balance_ok: self.balance_ok,
                debit_ok: self.debit_ok,
            }
        }
    }

    fn wrap_valid(tx: CeloPoolTx) -> ValidTransaction<CeloPoolTx> {
        ValidTransaction::Valid(tx)
    }

    #[test]
    fn test_apply_rates_successful_conversion() {
        let fc = Address::with_last_byte(0xAA);
        let tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 100, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 2 }),
            balance_ok: Some(true),
            debit_ok: Some(true),
        };

        let result = apply_exchange_rates_to_valid_tx(
            &mock, &mut valid, Address::ZERO, 0, 0, None,
        );
        assert!(result.is_ok());

        let tx = match &valid { ValidTransaction::Valid(t) => t, _ => panic!() };
        // 1_000_000_000 * 2/1 = 2_000_000_000
        assert_eq!(tx.max_fee_per_gas(), 2_000_000_000);
    }

    #[test]
    fn test_apply_rates_unregistered_currency() {
        let fc = Address::with_last_byte(0xAA);
        let tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 100, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        let mock = MockFcLookup { rate: None, balance_ok: None, debit_ok: None };
        let result = apply_exchange_rates_to_valid_tx(
            &mock, &mut valid, Address::ZERO, 0, 0, None,
        );
        assert!(matches!(result, Err(Cip64Rejection::UnregisteredCurrency(_))));
    }

    #[test]
    fn test_apply_rates_insufficient_balance() {
        let fc = Address::with_last_byte(0xAA);
        let tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 100, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance_ok: Some(false),
            debit_ok: None,
        };
        let result = apply_exchange_rates_to_valid_tx(
            &mock, &mut valid, Address::ZERO, 0, 0, None,
        );
        assert!(matches!(result, Err(Cip64Rejection::InsufficientBalance(_))));
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
            balance_ok: Some(true),
            debit_ok: Some(true),
        };
        let result = apply_exchange_rates_to_valid_tx(
            &mock, &mut valid, Address::ZERO, 25_000_000_000, 0, None,
        );
        assert!(matches!(result, Err(Cip64Rejection::BelowBaseFeeFloor(_))));
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
            balance_ok: Some(true),
            debit_ok: Some(true),
        };
        let result = apply_exchange_rates_to_valid_tx(
            &mock, &mut valid, Address::ZERO, 0, 100, None,
        );
        assert!(matches!(result, Err(Cip64Rejection::BelowMinTip { .. })));
    }

    #[test]
    fn test_apply_rates_debit_simulation_failed() {
        let fc = Address::with_last_byte(0xAA);
        let tx = make_test_tx(Some(fc), 21_000, 1_000_000_000, 100, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        let mock = MockFcLookup {
            rate: Some(ExchangeRate { numerator: 1, denominator: 1 }),
            balance_ok: Some(true),
            debit_ok: Some(false),
        };
        let result = apply_exchange_rates_to_valid_tx(
            &mock, &mut valid, Address::ZERO, 0, 0, None,
        );
        assert!(matches!(result, Err(Cip64Rejection::DebitSimulationFailed(_))));
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
            balance_ok: Some(true),
            debit_ok: Some(true),
        };
        let result = apply_exchange_rates_to_valid_tx(
            &mock, &mut valid, Address::ZERO, 0, 0, Some(100_000),
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
            balance_ok: Some(true),
            debit_ok: Some(true),
        };
        let result = apply_exchange_rates_to_valid_tx(
            &mock, &mut valid, Address::ZERO, 0, 0, Some(1_000_000_000_000),
        );
        assert!(matches!(result, Err(Cip64Rejection::ExceedsFeeCap { .. })));
    }

    #[test]
    fn test_fee_cap_native_tx_exceeds_cap() {
        // Native tx: gas=21_000, max_fee=1_000_000_000, value=0
        // cost - value = 21_000 * 1_000_000_000 = 21_000_000_000_000
        let tx = make_test_tx(None, 21_000, 1_000_000_000, 100, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        let mock = MockFcLookup { rate: None, balance_ok: None, debit_ok: None };
        let result = apply_exchange_rates_to_valid_tx(
            &mock, &mut valid, Address::ZERO, 0, 0, Some(1_000_000_000_000),
        );
        assert!(matches!(result, Err(Cip64Rejection::ExceedsFeeCap { .. })));
    }

    #[test]
    fn test_fee_cap_disabled_with_zero() {
        // Native tx with large cost, but cap = 0 → disabled
        let tx = make_test_tx(None, 21_000, 1_000_000_000, 100, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        let mock = MockFcLookup { rate: None, balance_ok: None, debit_ok: None };
        let result = apply_exchange_rates_to_valid_tx(
            &mock, &mut valid, Address::ZERO, 0, 0, Some(0),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_fee_cap_disabled_with_none() {
        let tx = make_test_tx(None, 21_000, 1_000_000_000, 100, Address::with_last_byte(1));
        let mut valid = wrap_valid(tx);

        let mock = MockFcLookup { rate: None, balance_ok: None, debit_ok: None };
        let result = apply_exchange_rates_to_valid_tx(
            &mock, &mut valid, Address::ZERO, 0, 0, None,
        );
        assert!(result.is_ok());
    }
}
