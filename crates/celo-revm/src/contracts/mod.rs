//! Contract interface modules for Celo

pub mod core_contracts;
pub mod erc20;

// Re-export commonly used items for convenience
pub use core_contracts::{
    CoreContractError, call, get_currencies, get_exchange_rates, get_intrinsic_gas,
    get_revert_message,
};
pub use erc20::{
    IFeeCurrencyERC20, encode_credit_gas_fees_call, encode_debit_gas_fees_call, get_balance,
};
