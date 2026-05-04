use crate::{CeloContext, CeloEvm, contracts::core_contracts::get_currency_info};
use alloy_primitives::map::HashMap;
use revm::{
    Database, Inspector,
    context_interface::ContextTr,
    handler::{EvmTr, PrecompileProvider},
    interpreter::InterpreterResult,
    primitives::{Address, U256},
};

use std::{format, string::String};

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
    pub fn new(
        currencies: HashMap<Address, FeeCurrencyInfo>,
        updated_at_block: Option<U256>,
    ) -> Self {
        Self {
            currencies,
            updated_at_block,
        }
    }

    /// Initialize with values read from the EVM
    pub fn new_from_evm<DB, INSP, P>(evm: &mut CeloEvm<DB, INSP, P>) -> FeeCurrencyContext
    where
        DB: Database,
        INSP: Inspector<CeloContext<DB>>,
        P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
    {
        let currencies = get_currency_info(evm);
        let current_block_number = evm.ctx().block().number;
        FeeCurrencyContext::new(currencies, Some(current_block_number))
    }

    pub fn currency_intrinsic_gas_cost(&self, currency: Option<Address>) -> Result<u64, String> {
        if currency.is_none_or(|currency| currency == Address::ZERO) {
            return Ok(0);
        }

        let currency_addr = currency.unwrap();
        match self.currencies.get(&currency_addr) {
            Some(info) => Ok(info.intrinsic_gas),
            None => Err(format!("fee currency not registered: {currency_addr}")),
        }
    }

    /// Allow the contract to overshoot 2 times the deducted intrinsic gas
    /// during execution.
    /// If the feeCurrency is None, then the max allowed intrinsic gas cost
    /// is 0 (i.e. not allowed) for a fee-currency specific EVM call within the STF.
    pub fn max_allowed_currency_intrinsic_gas_cost(
        &self,
        currency: Address,
    ) -> Result<u64, String> {
        self.currency_intrinsic_gas_cost(Some(currency))
            .map(|cost| cost.saturating_mul(3))
    }

    pub fn currency_exchange_rate(
        &self,
        currency: Option<Address>,
    ) -> Result<(U256, U256), String> {
        if currency.is_none() || currency.unwrap() == Address::ZERO {
            return Ok((U256::ONE, U256::ONE));
        }

        let currency_addr = currency.unwrap();
        match self.currencies.get(&currency_addr) {
            Some(info) => Ok(info.exchange_rate),
            None => Err(format!("fee currency not registered: {currency_addr}")),
        }
    }

    pub fn celo_to_currency(
        &self,
        currency: Option<Address>,
        amount: U256,
    ) -> Result<U256, String> {
        if currency.is_none() || currency.unwrap() == Address::ZERO {
            return Ok(amount);
        }

        let currency_addr = currency.unwrap();
        match self.currencies.get(&currency_addr) {
            Some(info) => Ok(amount.saturating_mul(info.exchange_rate.0) / info.exchange_rate.1),
            None => Err(format!("fee currency not registered: {currency_addr}")),
        }
    }

    pub fn currency_to_celo(
        &self,
        currency: Option<Address>,
        amount: U256,
    ) -> Result<U256, String> {
        if currency.is_none() || currency.unwrap() == Address::ZERO {
            return Ok(amount);
        }

        let currency_addr = currency.unwrap();
        match self.currencies.get(&currency_addr) {
            Some(info) => Ok(amount.saturating_mul(info.exchange_rate.1) / info.exchange_rate.0),
            None => Err(format!("fee currency not registered: {currency_addr}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CeloBuilder, DefaultCelo, contracts::core_contracts::tests::make_celo_test_db};
    use alloy_primitives::{U256, address};
    use revm::{Context, context_interface::ContextTr, handler::EvmTr};

    fn ctx_with_currency(currency: Address, rate_num: u64, rate_den: u64) -> FeeCurrencyContext {
        let mut currencies = HashMap::default();
        currencies.insert(
            currency,
            FeeCurrencyInfo {
                exchange_rate: (U256::from(rate_num), U256::from(rate_den)),
                intrinsic_gas: 12_345,
            },
        );
        FeeCurrencyContext::new(currencies, None)
    }

    #[test]
    fn currency_exchange_rate_native_paths() {
        let ctx = FeeCurrencyContext::default();
        // None → identity rate.
        assert_eq!(
            ctx.currency_exchange_rate(None).unwrap(),
            (U256::ONE, U256::ONE)
        );
        // Zero address → identity rate. The `||` short-circuit must hold; flipping
        // to `&&` would force `unwrap()` on None (panic) — covered by the case above.
        assert_eq!(
            ctx.currency_exchange_rate(Some(Address::ZERO)).unwrap(),
            (U256::ONE, U256::ONE)
        );
    }

    #[test]
    fn currency_exchange_rate_registered() {
        let c = address!("0x2222222222222222222222222222222222222222");
        let ctx = ctx_with_currency(c, 7, 3);
        assert_eq!(
            ctx.currency_exchange_rate(Some(c)).unwrap(),
            (U256::from(7), U256::from(3))
        );
    }

    #[test]
    fn currency_exchange_rate_unregistered_errors() {
        let ctx = FeeCurrencyContext::default();
        let c = address!("0x3333333333333333333333333333333333333333");
        assert!(ctx.currency_exchange_rate(Some(c)).is_err());
    }

    #[test]
    fn celo_to_currency_native_pass_through() {
        let ctx = FeeCurrencyContext::default();
        let amount = U256::from(1_000u64);
        assert_eq!(ctx.celo_to_currency(None, amount).unwrap(), amount);
        assert_eq!(
            ctx.celo_to_currency(Some(Address::ZERO), amount).unwrap(),
            amount
        );
    }

    #[test]
    fn celo_to_currency_scales_by_rate() {
        // rate 5/2: 100 native CELO → 250 currency. Picked so that
        // `* / 2` differs from both `* % 2` (=0) and the unmutated value.
        let c = address!("0x4444444444444444444444444444444444444444");
        let ctx = ctx_with_currency(c, 5, 2);
        let out = ctx.celo_to_currency(Some(c), U256::from(100u64)).unwrap();
        assert_eq!(out, U256::from(250u64));
    }

    #[test]
    fn currency_to_celo_native_pass_through() {
        let ctx = FeeCurrencyContext::default();
        let amount = U256::from(1_000u64);
        assert_eq!(ctx.currency_to_celo(None, amount).unwrap(), amount);
        assert_eq!(
            ctx.currency_to_celo(Some(Address::ZERO), amount).unwrap(),
            amount
        );
    }

    #[test]
    fn currency_to_celo_inverts_rate() {
        // Same rate 5/2 going the other way: 250 currency → 100 native CELO.
        let c = address!("0x5555555555555555555555555555555555555555");
        let ctx = ctx_with_currency(c, 5, 2);
        let out = ctx.currency_to_celo(Some(c), U256::from(250u64)).unwrap();
        assert_eq!(out, U256::from(100u64));
    }

    #[test]
    fn celo_to_currency_unregistered_errors() {
        let ctx = FeeCurrencyContext::default();
        let c = address!("0x6666666666666666666666666666666666666666");
        assert!(ctx.celo_to_currency(Some(c), U256::from(1u64)).is_err());
        assert!(ctx.currency_to_celo(Some(c), U256::from(1u64)).is_err());
    }

    #[test]
    fn currency_intrinsic_gas_cost_native_is_zero() {
        let ctx = FeeCurrencyContext::default();
        assert_eq!(ctx.currency_intrinsic_gas_cost(None).unwrap(), 0);
        assert_eq!(
            ctx.currency_intrinsic_gas_cost(Some(Address::ZERO))
                .unwrap(),
            0
        );
    }

    #[test]
    fn max_allowed_currency_intrinsic_gas_cost_is_3x() {
        let c = address!("0x7777777777777777777777777777777777777777");
        let ctx = ctx_with_currency(c, 1, 1);
        // ctx_with_currency sets intrinsic_gas to 12_345.
        assert_eq!(
            ctx.max_allowed_currency_intrinsic_gas_cost(c).unwrap(),
            12_345 * 3
        );
    }

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
