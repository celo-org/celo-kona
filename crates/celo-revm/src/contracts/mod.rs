//! Contract interface modules for Celo

pub mod core_contracts;
pub mod erc20;

// Re-export commonly used items for convenience
pub use core_contracts::{CoreContractError, get_currency_info, get_revert_message};
pub use erc20::{IFeeCurrencyERC20, get_balance};
