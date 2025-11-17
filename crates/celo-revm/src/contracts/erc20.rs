//! ERC20 token interface for fee currency handling

use super::core_contracts::{self, CoreContractError};
use crate::{CeloContext, evm::CeloEvm};
use alloy_sol_types::{SolCall, sol};
use revm::{
    Database, Inspector,
    primitives::{Address, Log, U256},
};
use std::vec::Vec;

// Define the ERC20 interface for fee currencies with Celo-specific extensions
sol! {
    interface IFeeCurrencyERC20 {
        // Standard ERC20 functions
        function balanceOf(address account) external view returns (uint256);
        function transfer(address to, uint256 amount) external returns (bool);
        function transferFrom(address from, address to, uint256 amount) external returns (bool);

        // Celo-specific gas fee functions
        function debitGasFees(address from, uint256 value) external;
        function creditGasFees(
            address from,
            address feeRecipient,
            address gatewayFeeRecipient,
            address communityFund,
            uint256 refund,
            uint256 tipTxFee,
            uint256 gatewayFee,
            uint256 baseTxFee
        ) external;
    }
}

/// Get the balance of an account for a given ERC20 token
pub fn get_balance<DB, INSP>(
    evm: &mut CeloEvm<DB, INSP>,
    token_address: Address,
    account: Address,
) -> Result<U256, CoreContractError>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
{
    // Prepare the balanceOf call
    let calldata = IFeeCurrencyERC20::balanceOfCall { account }
        .abi_encode()
        .into();

    // Use the read-only call function to ensure no state changes
    let (output_bytes, _, _) = core_contracts::call_read_only(evm, token_address, calldata, None)?;

    // Decode the balance
    IFeeCurrencyERC20::balanceOfCall::abi_decode_returns(&output_bytes)
        .map_err(CoreContractError::from)
}

/// Call debitGasFees to deduct gas fees from the fee currency.
/// State changes remain in the EVM's journal for the main transaction to see.
pub fn debit_gas_fees<DB, INSP>(
    evm: &mut CeloEvm<DB, INSP>,
    fee_currency_address: Address,
    from: Address,
    value: U256,
    gas_limit: u64,
) -> Result<(Vec<Log>, u64), CoreContractError>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
{
    let calldata = IFeeCurrencyERC20::debitGasFeesCall { from, value }
        .abi_encode()
        .into();

    // debitGasFees returns void, so we just need to check that the call succeeded
    let (_, logs, gas_used) =
        core_contracts::call(evm, fee_currency_address, calldata, Some(gas_limit))?;
    Ok((logs, gas_used))
}

/// Call creditGasFees to distribute gas fees.
/// State changes remain in the EVM's journal for the main transaction to see.
#[allow(clippy::too_many_arguments)]
pub fn credit_gas_fees<DB, INSP>(
    evm: &mut CeloEvm<DB, INSP>,
    fee_currency_address: Address,
    from: Address,
    fee_recipient: Address,
    community_fund: Address,
    refund: U256,
    tip_tx_fee: U256,
    base_tx_fee: U256,
    gas_limit: u64,
) -> Result<(Vec<Log>, u64), CoreContractError>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
{
    let calldata = IFeeCurrencyERC20::creditGasFeesCall {
        from,
        feeRecipient: fee_recipient,
        gatewayFeeRecipient: Address::ZERO,
        communityFund: community_fund,
        refund,
        tipTxFee: tip_tx_fee,
        gatewayFee: U256::ZERO,
        baseTxFee: base_tx_fee,
    }
    .abi_encode()
    .into();

    // creditGasFees returns void, so we just need to check that the call succeeded
    let (_, logs, gas_used) =
        core_contracts::call(evm, fee_currency_address, calldata, Some(gas_limit))?;
    Ok((logs, gas_used))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn test_debit_gas_fees_calldata_structure() {
        let from = address!("0x1111111111111111111111111111111111111111");
        let value = U256::from(500u64);
        let calldata = IFeeCurrencyERC20::debitGasFeesCall { from, value };

        // Verify the call can be decoded back
        let encoded = calldata.abi_encode();
        let decoded = IFeeCurrencyERC20::debitGasFeesCall::abi_decode(&encoded).unwrap();

        assert_eq!(decoded.from, from);
        assert_eq!(decoded.value, value);
    }

    #[test]
    fn test_credit_gas_fees_calldata_structure() {
        let from = address!("0x1111111111111111111111111111111111111111");
        let fee_recipient = address!("0x2222222222222222222222222222222222222222");
        let gateway_fee_recipient = address!("0x3333333333333333333333333333333333333333");
        let community_fund = address!("0x4444444444444444444444444444444444444444");
        let refund = U256::from(100u64);
        let tip_tx_fee = U256::from(200u64);
        let gateway_fee = U256::from(50u64);
        let base_tx_fee = U256::from(150u64);

        let calldata = IFeeCurrencyERC20::creditGasFeesCall {
            from,
            feeRecipient: fee_recipient,
            gatewayFeeRecipient: gateway_fee_recipient,
            communityFund: community_fund,
            refund,
            tipTxFee: tip_tx_fee,
            gatewayFee: gateway_fee,
            baseTxFee: base_tx_fee,
        };

        // Verify the call can be decoded back
        let encoded = calldata.abi_encode();
        let decoded = IFeeCurrencyERC20::creditGasFeesCall::abi_decode(&encoded).unwrap();

        assert_eq!(decoded.from, from);
        assert_eq!(decoded.feeRecipient, fee_recipient);
        assert_eq!(decoded.gatewayFeeRecipient, gateway_fee_recipient);
        assert_eq!(decoded.communityFund, community_fund);
        assert_eq!(decoded.refund, refund);
        assert_eq!(decoded.tipTxFee, tip_tx_fee);
        assert_eq!(decoded.gatewayFee, gateway_fee);
        assert_eq!(decoded.baseTxFee, base_tx_fee);
    }
}
