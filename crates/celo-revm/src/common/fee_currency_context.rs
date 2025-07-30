use crate::{
    CeloContext, CeloEvm,
    contracts::core_contracts::{
        CoreContractError, get_currencies, get_exchange_rates, get_intrinsic_gas,
    },
};
use alloy_primitives::map::HashMap;
use revm::{
    Database, Inspector,
    context_interface::ContextTr,
    handler::EvmTr,
    primitives::{Address, U256},
};

use std::{format, string::String};

#[derive(Debug, Clone, Default)]
pub struct FeeCurrencyContext {
    exchange_rates: HashMap<Address, (U256, U256)>,
    intrinsic_gas: HashMap<Address, U256>,
    pub updated_at_block: Option<u64>,
}

impl FeeCurrencyContext {
    pub fn new(
        exchange_rates: HashMap<Address, (U256, U256)>,
        intrinsic_gas: HashMap<Address, U256>,
        updated_at_block: Option<u64>,
    ) -> Self {
        Self {
            exchange_rates,
            intrinsic_gas,
            updated_at_block,
        }
    }

    /// Initialize with values read from the EVM
    pub fn new_from_evm<DB, INSP>(
        evm: &mut CeloEvm<DB, INSP>,
    ) -> Result<FeeCurrencyContext, CoreContractError>
    where
        DB: Database,
        INSP: Inspector<CeloContext<DB>>,
    {
        let currencies = &get_currencies(evm)?;
        let exchange_rates = get_exchange_rates(evm, currencies)?;
        let intrinsic_gas = get_intrinsic_gas(evm, currencies)?;
        let current_block_number = evm.ctx().block().number;
        Ok(FeeCurrencyContext::new(
            exchange_rates,
            intrinsic_gas,
            Some(current_block_number),
        ))
    }

    pub fn currency_intrinsic_gas_cost(&self, currency: Option<Address>) -> Result<U256, String> {
        if currency.is_none() || currency.unwrap() == Address::ZERO {
            return Ok(U256::ZERO);
        }

        let currency_addr = currency.unwrap();
        match self.intrinsic_gas.get(&currency_addr) {
            Some(gas_cost) => Ok(*gas_cost),
            None => Err(format!("fee currency not registered: {}", currency_addr)),
        }
    }

    pub fn currency_exchange_rate(
        &self,
        currency: Option<Address>,
    ) -> Result<(U256, U256), String> {
        if currency.is_none() || currency.unwrap() == Address::ZERO {
            return Ok((U256::ONE, U256::ONE));
        }

        let currency_addr = currency.unwrap();
        match self.exchange_rates.get(&currency_addr) {
            Some(exchange_rate) => Ok(*exchange_rate),
            None => Err(format!("fee currency not registered: {}", currency_addr)),
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
        match self.exchange_rates.get(&currency_addr) {
            Some(rate) => Ok(amount.saturating_mul(rate.0) / rate.1),
            None => Err(format!("fee currency not registered: {}", currency_addr)),
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
        match self.exchange_rates.get(&currency_addr) {
            Some(rate) => Ok(amount.saturating_mul(rate.1) / rate.0),
            None => Err(format!("fee currency not registered: {}", currency_addr)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CeloBuilder, DefaultCelo, core_contracts::tests::make_celo_test_db};
    use alloy_primitives::{U256, address};
    use revm::Context;
    use revm_context::ContextTr;
    use revm_handler::EvmTr;

    #[test]
    fn test_new_from_evm() {
        let ctx = Context::celo().with_db(make_celo_test_db());
        let mut evm = ctx.build_celo();
        let fee_currency_context = FeeCurrencyContext::new_from_evm(&mut evm).unwrap();

        let exchange_rate = fee_currency_context
            .currency_exchange_rate(Some(address!("0x1111111111111111111111111111111111111111")))
            .unwrap();
        assert_eq!(exchange_rate, (U256::from(20), U256::from(10)));

        let intrinsic_gas_cost = fee_currency_context
            .currency_intrinsic_gas_cost(Some(address!(
                "0x1111111111111111111111111111111111111111"
            )))
            .unwrap();
        assert_eq!(intrinsic_gas_cost, U256::from(50000));

        // Verify that updated_at_block is set to the current block number
        assert_eq!(
            fee_currency_context.updated_at_block.unwrap(),
            evm.ctx().block().number
        );
    }
}
