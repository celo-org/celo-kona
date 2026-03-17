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
}

/// Extract the fee currency address from a pool transaction without cloning.
fn extract_fee_currency(inner: &InnerPoolTx) -> Option<Address> {
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
        Self {
            inner,
            native_max_fee_per_gas,
            native_max_priority_fee_per_gas,
            fee_currency,
        }
    }

    /// Apply an exchange rate to convert fee-currency values to native equivalents.
    pub fn apply_exchange_rate(&mut self, rate: ExchangeRate) {
        self.native_max_fee_per_gas = rate.to_native(self.inner.max_fee_per_gas());
        self.native_max_priority_fee_per_gas = self
            .inner
            .max_priority_fee_per_gas()
            .map(|v| rate.to_native(v));
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
        self.inner.size() + core::mem::size_of::<u128>() * 2
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
        self.inner.cost()
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

/// Look up the exchange rate for a fee currency by calling the FeeCurrencyDirectory
/// contract's `getExchangeRate(address)` method via a temporary EVM.
fn lookup_exchange_rate(
    provider: &dyn StateProviderFactory,
    fee_currency: Address,
    fee_currency_directory: Address,
) -> Option<ExchangeRate> {
    use alloy_sol_types::SolCall;
    use celo_revm::{
        CeloBuilder, DefaultCelo,
        contracts::core_contracts::getExchangeRateCall,
    };
    use reth_revm::database::StateProviderDatabase;
    use revm::{Context, SystemCallEvm, context_interface::result::ExecutionResult};

    let state = provider.latest().ok()?;
    let db = StateProviderDatabase::new(state);

    let ctx = Context::celo().with_db(db);
    let mut evm = ctx.build_celo();

    let calldata = getExchangeRateCall { token: fee_currency }.abi_encode();
    let result = evm
        .system_call_one(fee_currency_directory, calldata.into())
        .ok()?;

    let output = match result {
        ExecutionResult::Success { output, .. } => output.into_data(),
        _ => return None,
    };

    let rate = getExchangeRateCall::abi_decode_returns(&output).ok()?;

    let numerator = u128::try_from(rate.numerator).ok()?;
    let denominator = u128::try_from(rate.denominator).ok()?;

    if numerator == 0 || denominator == 0 {
        return None;
    }

    Some(ExchangeRate {
        numerator,
        denominator,
    })
}

// ---------------------------------------------------------------------------
// UnregisteredFeeCurrency error
// ---------------------------------------------------------------------------

/// Error for CIP-64 transactions with an unregistered fee currency.
#[derive(Debug)]
struct UnregisteredFeeCurrency(Address);

impl std::fmt::Display for UnregisteredFeeCurrency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unregistered fee-currency address {}", self.0)
    }
}

impl std::error::Error for UnregisteredFeeCurrency {}

impl PoolTransactionError for UnregisteredFeeCurrency {
    fn is_bad_transaction(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

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
    pub fn new(inner: V, provider: P, fee_currency_directory: Address) -> Self {
        Self { inner, provider, fee_currency_directory }
    }
}

/// Apply exchange rates to a [`ValidTransaction`] if it is a CIP-64 tx.
///
/// Returns `Err` with the fee currency address if the exchange rate is not found
/// (i.e. the fee currency is not registered in the FeeCurrencyDirectory).
fn apply_exchange_rates_to_valid_tx(
    provider: &dyn StateProviderFactory,
    valid_tx: &mut ValidTransaction<CeloPoolTx>,
    fee_currency_directory: Address,
) -> Result<(), Address> {
    let tx = match valid_tx {
        ValidTransaction::Valid(tx) => tx,
        ValidTransaction::ValidWithSidecar { transaction, .. } => transaction,
    };
    if let Some(fc) = tx.fee_currency() {
        if let Some(rate) = lookup_exchange_rate(provider, fc, fee_currency_directory) {
            let old_fee = tx.inner.max_fee_per_gas();
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
        } else {
            tracing::warn!(
                target: "celo::pool",
                ?fc,
                "Rejecting CIP-64 tx: unregistered fee currency"
            );
            return Err(fc);
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
        let provider = &self.provider;
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
                    if let Err(fc) =
                        apply_exchange_rates_to_valid_tx(provider, &mut transaction, self.fee_currency_directory)
                    {
                        let tx = match transaction {
                            ValidTransaction::Valid(tx) => tx,
                            ValidTransaction::ValidWithSidecar { transaction, .. } => {
                                transaction
                            }
                        };
                        TransactionValidationOutcome::Invalid(
                            tx,
                            InvalidPoolTransactionError::other(
                                UnregisteredFeeCurrency(fc),
                            ),
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
