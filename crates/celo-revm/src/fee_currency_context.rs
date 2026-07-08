use crate::{
    CeloContext, CeloEvm,
    constants::FEE_CURRENCY_NOT_REGISTERED_PREFIX,
    contracts::core_contracts::get_currency_info,
    units::{FcU256, NativeU256},
};
use alloy_primitives::map::HashMap;
use revm::{
    Database, Inspector,
    context_interface::ContextTr,
    handler::{EvmTr, PrecompileProvider},
    interpreter::InterpreterResult,
    primitives::{Address, U256},
};

/// Error returned by [`FeeCurrencyContext`] lookups.
///
/// Typed on the producer side. It is flattened to a string at the revm boundary
/// (`InvalidTransaction::from(err.to_string())`) because revm's `InvalidTransaction`
/// and op-revm's `OpTransactionError` are closed enums with no Celo variant, so a
/// fully-typed error cannot cross that boundary without forking op-revm. The
/// `Display` text carries [`FEE_CURRENCY_NOT_REGISTERED_PREFIX`] so the EVM layer
/// can still classify it (see `alloy-celo-evm`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeeCurrencyError {
    /// The fee currency is not present in the per-block fee-currency context
    /// (its directory config could not be read, so it was dropped while loading).
    NotRegistered(Address),
}

impl core::fmt::Display for FeeCurrencyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotRegistered(addr) => {
                write!(f, "{FEE_CURRENCY_NOT_REGISTERED_PREFIX}: {addr}")
            }
        }
    }
}

/// Complete fee currency information for a registered currency.
/// Both exchange rate and intrinsic gas are required - partial data is rejected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FeeCurrencyInfo {
    /// Exchange rate as (numerator, denominator) for converting from native to fee currency
    pub exchange_rate: (U256, U256),
    /// Additional intrinsic gas cost for transactions using this fee currency
    pub intrinsic_gas: u64,
}

#[derive(Debug, Clone, Default)]
pub struct FeeCurrencyContext {
    currencies: HashMap<Address, FeeCurrencyInfo>,
    pub updated_at_block: Option<U256>,
}

impl FeeCurrencyContext {
    pub const fn new(
        currencies: HashMap<Address, FeeCurrencyInfo>,
        updated_at_block: Option<U256>,
    ) -> Self {
        Self {
            currencies,
            updated_at_block,
        }
    }

    /// Initialize with values read from the EVM
    pub fn new_from_evm<DB, INSP, P>(evm: &mut CeloEvm<DB, INSP, P>) -> Self
    where
        DB: Database,
        INSP: Inspector<CeloContext<DB>>,
        P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
    {
        let currencies = get_currency_info(evm);
        let current_block_number = evm.ctx().block().number;
        Self::new(currencies, Some(current_block_number))
    }

    /// Look up a registered currency, erroring for unregistered addresses.
    /// Callers handle the native case (see [`non_native_fee_currency`]) first.
    fn registered(&self, currency: Address) -> Result<&FeeCurrencyInfo, FeeCurrencyError> {
        self.currencies
            .get(&currency)
            .ok_or(FeeCurrencyError::NotRegistered(currency))
    }

    pub fn currency_intrinsic_gas_cost(
        &self,
        currency: Option<Address>,
    ) -> Result<u64, FeeCurrencyError> {
        let Some(currency) = non_native_fee_currency(currency) else {
            return Ok(0);
        };
        Ok(self.registered(currency)?.intrinsic_gas)
    }

    /// Allow the contract to overshoot 2 times the deducted intrinsic gas
    /// during execution.
    /// If the feeCurrency is None, then the max allowed intrinsic gas cost
    /// is 0 (i.e. not allowed) for a fee-currency specific EVM call within the STF.
    pub fn max_allowed_currency_intrinsic_gas_cost(
        &self,
        currency: Address,
    ) -> Result<u64, FeeCurrencyError> {
        self.currency_intrinsic_gas_cost(Some(currency))
            .map(|cost| cost.saturating_mul(3))
    }

    pub fn currency_exchange_rate(
        &self,
        currency: Option<Address>,
    ) -> Result<(U256, U256), FeeCurrencyError> {
        let Some(currency) = non_native_fee_currency(currency) else {
            return Ok((U256::ONE, U256::ONE));
        };
        Ok(self.registered(currency)?.exchange_rate)
    }

    /// Convert a native-CELO amount to its fee-currency equivalent at the
    /// rate registered for `currency`.
    ///
    /// When `currency` is `None` or `Address::ZERO` the input is treated as
    /// native CELO and wrapped unchanged in `FcU256` to keep the return shape
    /// uniform — callers on that branch must know they're reading a native
    /// value back out, since the type tag is no longer load-bearing there.
    pub fn celo_to_currency(
        &self,
        currency: Option<Address>,
        amount: NativeU256,
    ) -> Result<FcU256, FeeCurrencyError> {
        let Some(currency) = non_native_fee_currency(currency) else {
            return Ok(FcU256::new(amount.into_inner()));
        };
        let (numerator, denominator) = self.registered(currency)?.exchange_rate;
        Ok(FcU256::new(
            amount.into_inner().saturating_mul(numerator) / denominator,
        ))
    }

    /// Convert a fee-currency amount to its native-CELO equivalent. Same
    /// no-currency / zero-address passthrough as [`Self::celo_to_currency`].
    pub fn currency_to_celo(
        &self,
        currency: Option<Address>,
        amount: FcU256,
    ) -> Result<NativeU256, FeeCurrencyError> {
        let Some(currency) = non_native_fee_currency(currency) else {
            return Ok(NativeU256::new(amount.into_inner()));
        };
        let (numerator, denominator) = self.registered(currency)?.exchange_rate;
        Ok(NativeU256::new(
            amount.into_inner().saturating_mul(denominator) / numerator,
        ))
    }
}

/// Normalize a fee-currency field to `Some(addr)` only for a real ERC20 fee
/// currency: `None` and `Address::ZERO` both denote native CELO (the zero
/// address cannot host an ERC20 contract). This is the same rule as
/// [`crate::CeloTxTr::is_fee_in_celo`], in `Option` form for lookups.
pub fn non_native_fee_currency(currency: Option<Address>) -> Option<Address> {
    currency.filter(|c| *c != Address::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CeloBuilder, DefaultCelo, contracts::core_contracts::tests::make_celo_test_db};
    use alloy_primitives::{U256, address};
    use revm::{Context, context_interface::ContextTr, handler::EvmTr};

    #[test]
    fn test_new_from_evm() {
        let ctx = Context::celo().with_db(make_celo_test_db());
        let mut evm = ctx.build_celo();
        let fee_currency_context = FeeCurrencyContext::new_from_evm(&mut evm);

        let test_currency = address!("0x1111111111111111111111111111111111111111");

        // Verify that the currency has BOTH exchange rate and intrinsic gas
        let exchange_rate = fee_currency_context
            .currency_exchange_rate(Some(test_currency))
            .unwrap();
        assert_eq!(exchange_rate, (U256::from(20), U256::from(10)));

        let intrinsic_gas_cost = fee_currency_context
            .currency_intrinsic_gas_cost(Some(test_currency))
            .unwrap();
        assert_eq!(intrinsic_gas_cost, 50_000);

        // Verify that updated_at_block is set to the current block number
        assert_eq!(
            fee_currency_context.updated_at_block.unwrap(),
            evm.ctx().block().number
        );
    }
}
