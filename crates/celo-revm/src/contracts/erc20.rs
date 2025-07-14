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

    // Use the existing call function from core_contracts
    let output_bytes = core_contracts::call(evm, token_address, calldata)?;

    // Decode the balance
    IFeeCurrencyERC20::balanceOfCall::abi_decode_returns(&output_bytes)
        .map_err(|e| CoreContractError::from(e))
}

/// Call debitGasFees to deduct gas fees from the fee currency
pub fn debit_gas_fees<DB, INSP>(
    evm: &mut CeloEvm<DB, INSP>,
    fee_currency_address: Address,
    from: Address, 
    value: U256
) -> Result<(), CoreContractError>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
{
    let calldata = encode_debit_gas_fees_call(from, value);

    // debitGasFees returns void, so we just need to check that the call succeeded
    core_contracts::mutable_call(evm, fee_currency_address, calldata)?;
    Ok(())
}

/// Call creditGasFees to distribute gas fees
pub fn credit_gas_fees<DB, INSP>(
    evm: &mut CeloEvm<DB, INSP>,
    fee_currency_address: Address,
    from: Address,
    fee_recipient: Address,
    gateway_fee_recipient: Address,
    community_fund: Address,
    refund: U256,
    tip_tx_fee: U256,
    gateway_fee: U256,
    base_tx_fee: U256,
) -> Result<(), CoreContractError>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
{
    let calldata = encode_credit_gas_fees_call(
        from,
        fee_recipient,
        gateway_fee_recipient,
        community_fund,
        refund,
        tip_tx_fee,
        gateway_fee,
        base_tx_fee,
    );

    // creditGasFees returns void, so we just need to check that the call succeeded
    core_contracts::mutable_call(evm, fee_currency_address, calldata)?;
    Ok(())
}

/// Encode a debitGasFees call (for testing purposes)
pub fn encode_debit_gas_fees_call(from: Address, value: U256) -> Bytes {
    IFeeCurrencyERC20::debitGasFeesCall { from, value }.abi_encode().into()
}

/// Encode a creditGasFees call (for testing purposes)
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
        gatewayFeeRecipient: gateway_fee_recipient,
        communityFund: community_fund,
        refund,
        tipTxFee: tip_tx_fee,
        gatewayFee: gateway_fee,
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
        
        // Check that the 'from' address is properly encoded
        let expected_from_offset = 4 + 12; // selector + padding
        assert_eq!(
            &encoded[expected_from_offset..expected_from_offset + 20],
            from.as_slice()
        );
        
        // Check that the value is properly encoded in the last 32 bytes
        let value_bytes = value.to_be_bytes::<32>();
        assert_eq!(&encoded[36..68], &value_bytes);
    }

    #[test]
    fn test_encode_credit_gas_fees() {
        let from = address!("0x1234567890123456789012345678901234567890");
        let fee_recipient = address!("0x2345678901234567890123456789012345678901");
        let gateway_fee_recipient = address!("0x3456789012345678901234567890123456789012");
        let community_fund = address!("0x4567890123456789012345678901234567890123");
        let refund = U256::from(100u64);
        let tip_tx_fee = U256::from(200u64);
        let gateway_fee = U256::from(50u64);
        let base_tx_fee = U256::from(150u64);

        let encoded = encode_credit_gas_fees_call(
            from,
            fee_recipient,
            gateway_fee_recipient,
            community_fund,
            refund,
            tip_tx_fee,
            gateway_fee,
            base_tx_fee,
        );

        // creditGasFees selector is 0x6a30b253
        assert_eq!(&encoded[0..4], &[106, 48, 178, 83]);

        // Check that the 'from' address is properly encoded
        let expected_from_offset = 4 + 12; // selector + padding
        assert_eq!(
            &encoded[expected_from_offset..expected_from_offset + 20],
            from.as_slice()
        );
    }

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

    #[test]
    fn test_debit_gas_fees_zero_value() {
        let from = address!("0x1234567890123456789012345678901234567890");
        let value = U256::ZERO;
        let encoded = encode_debit_gas_fees_call(from, value);

        // Should encode properly even with zero value
        assert_eq!(&encoded[0..4], &[0x58, 0xcf, 0x96, 0x72]);
        
        // Value should be all zeros in the last 32 bytes
        let zero_bytes = [0u8; 32];
        assert_eq!(&encoded[36..68], &zero_bytes);
    }

    #[test]
    fn test_credit_gas_fees_zero_values() {
        let from = address!("0x1234567890123456789012345678901234567890");
        let fee_recipient = address!("0x2345678901234567890123456789012345678901");
        let gateway_fee_recipient = Address::ZERO;
        let community_fund = Address::ZERO;
        let refund = U256::ZERO;
        let tip_tx_fee = U256::ZERO;
        let gateway_fee = U256::ZERO;
        let base_tx_fee = U256::ZERO;

        let encoded = encode_credit_gas_fees_call(
            from,
            fee_recipient,
            gateway_fee_recipient,
            community_fund,
            refund,
            tip_tx_fee,
            gateway_fee,
            base_tx_fee,
        );

        // Should encode properly even with zero values - just check length
        assert!(encoded.len() >= 4);
    }
}
