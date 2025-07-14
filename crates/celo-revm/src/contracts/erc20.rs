//! ERC20 token interface for fee currency handling

use super::core_contracts::{self, CoreContractError};
use crate::{CeloContext, evm::CeloEvm};
use alloy_sol_types::{SolCall, sol};
use revm::{
    Database, Inspector,
    primitives::{Address, Bytes, U256},
};

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
            address, // gatewayFeeRecipient (unused)
            address communityFund,
            uint256 refund,
            uint256 tipTxFee,
            uint256, // gatewayFee (unused)
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

    // Use the existing call function from core_contracts
    let output_bytes = core_contracts::call(evm, token_address, calldata)?;

    // Decode the balance
    IFeeCurrencyERC20::balanceOfCall::abi_decode_returns(&output_bytes)
        .map_err(|e| CoreContractError::from(e))
}

/// Encode a debitGasFees call for deducting gas fees
pub fn encode_debit_gas_fees_call(from: Address, value: U256) -> Bytes {
    IFeeCurrencyERC20::debitGasFeesCall { from, value }
        .abi_encode()
        .into()
}

/// Encode a creditGasFees call for distributing gas fees
pub fn encode_credit_gas_fees_call(
    from: Address,
    fee_recipient: Address,
    gateway_fee_recipient: Address,
    community_fund: Address,
    refund: U256,
    tip_tx_fee: U256,
    gateway_fee: U256,
    base_tx_fee: U256,
) -> Bytes {
    IFeeCurrencyERC20::creditGasFeesCall {
        from,
        feeRecipient: fee_recipient,
        _2: gateway_fee_recipient,
        communityFund: community_fund,
        refund,
        tipTxFee: tip_tx_fee,
        _6: gateway_fee,
        baseTxFee: base_tx_fee,
    }
    .abi_encode()
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn test_encode_balance_of() {
        let account = address!("0x1234567890123456789012345678901234567890");
        let calldata = IFeeCurrencyERC20::balanceOfCall { account }.abi_encode();

        // balanceOf selector is 0x70a08231
        assert_eq!(&calldata[0..4], &[0x70, 0xa0, 0x82, 0x31]);

        // Check that address is properly encoded
        let expected_address_offset = 4 + 12; // selector + padding
        assert_eq!(
            &calldata[expected_address_offset..expected_address_offset + 20],
            account.as_slice()
        );
    }

    #[test]
    fn test_decode_balance_of() {
        // Create a mock return value of 1000 tokens
        let balance = U256::from(1000u64);
        let return_data = balance.to_be_bytes::<32>();

        let decoded = IFeeCurrencyERC20::balanceOfCall::abi_decode_returns(&return_data).unwrap();
        assert_eq!(decoded, balance);
    }

    #[test]
    fn test_encode_debit_gas_fees() {
        let from = address!("0x1234567890123456789012345678901234567890");
        let value = U256::from(1000u64);
        let encoded = encode_debit_gas_fees_call(from, value);

        // debitGasFees selector is 0x58cf9672
        assert_eq!(&encoded[0..4], &[0x58, 0xcf, 0x96, 0x72]);
    }
}
