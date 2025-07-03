use alloy_primitives::map::HashMap;
use revm::primitives::{Address, U256};

use std::{string::String, format};

#[derive(Debug, Clone, Default)]
pub struct FeeCurrencyContext {
    exchange_rates: HashMap<Address, (U256, U256)>,
    intrinsic_gas: HashMap<Address, U256>,
}

impl FeeCurrencyContext {
    pub fn new(exchange_rates: HashMap<Address, (U256, U256)>, intrinsic_gas: HashMap<Address, U256>) -> Self {
        Self { exchange_rates, intrinsic_gas }
    }

    pub fn currency_intrinsic_gas_cost(&self, currency: Option<Address>) -> Result<U256, String> {
        if currency == None || currency.unwrap() == Address::ZERO {
            return Ok(U256::ZERO);
        }

        let currency_addr = currency.unwrap();
        match self.intrinsic_gas.get(&currency_addr) {
            Some(gas_cost) => Ok(*gas_cost),
            None => Err(format!("fee currency not registered: {}", currency_addr)),
        }
    }

    pub fn currency_exchange_rate(&self, currency: Option<Address>) -> Result<(U256, U256), String> {
        if currency == None || currency.unwrap() == Address::ZERO {
            return Ok((U256::ONE, U256::ONE));
        }

        let currency_addr = currency.unwrap();
        match self.exchange_rates.get(&currency_addr) {
            Some(exchange_rate) => Ok(*exchange_rate),
            None => Err(format!("fee currency not registered: {}", currency_addr)),
        }
    }

    pub fn celo_to_currency(&self, currency: Option<Address>, amount: U256) -> Result<U256, String> {
        if currency == None || currency.unwrap() == Address::ZERO {
            return Ok(amount);
        }

        let currency_addr = currency.unwrap();
        match self.exchange_rates.get(&currency_addr) {
            Some(rate) => Ok(amount.saturating_mul(rate.0) / rate.1),
            None => Err(format!("fee currency not registered: {}", currency_addr)),
        }
    }

    pub fn currency_to_celo(&self, currency: Option<Address>, amount: U256) -> Result<U256, String> {
        if currency == None || currency.unwrap() == Address::ZERO {
            return Ok(amount);
        }

        let currency_addr = currency.unwrap();
        match self.exchange_rates.get(&currency_addr) {
            Some(rate) => Ok(amount.saturating_mul(rate.1) / rate.0),
            None => Err(format!("fee currency not registered: {}", currency_addr)),
        }
    }
}