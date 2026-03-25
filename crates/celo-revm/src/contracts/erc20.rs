//! ERC20 token interface for fee currency handling

use super::core_contracts::{self, CoreContractError};
use crate::{CeloContext, evm::CeloEvm};
use alloy_sol_types::{SolCall, sol};
use revm::{
    Database, Inspector,
    handler::PrecompileProvider,
    interpreter::InterpreterResult,
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
pub fn get_balance<DB, INSP, P>(
    evm: &mut CeloEvm<DB, INSP, P>,
    token_address: Address,
    account: Address,
) -> Result<U256, CoreContractError>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
{
    // Prepare the balanceOf call
    let calldata = IFeeCurrencyERC20::balanceOfCall { account }
        .abi_encode()
        .into();

    // Use the read-only call function to ensure no state changes
    let (output_bytes, _, _, _) =
        core_contracts::call_read_only(evm, token_address, calldata, None)?;

    // Decode the balance
    IFeeCurrencyERC20::balanceOfCall::abi_decode_returns(&output_bytes)
        .map_err(CoreContractError::from)
}

/// Call debitGasFees to deduct gas fees from the fee currency.
/// State changes remain in the EVM's journal for the main transaction to see.
/// Returns (logs, gas_used, gas_refunded) where gas_used is net after refunds.
pub fn debit_gas_fees<DB, INSP, P>(
    evm: &mut CeloEvm<DB, INSP, P>,
    fee_currency_address: Address,
    from: Address,
    value: U256,
    gas_limit: u64,
) -> Result<(Vec<Log>, u64, u64), CoreContractError>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
{
    let calldata = IFeeCurrencyERC20::debitGasFeesCall { from, value }
        .abi_encode()
        .into();

    // debitGasFees returns void, so we just need to check that the call succeeded
    let (_, logs, gas_used, gas_refunded) =
        core_contracts::call(evm, fee_currency_address, calldata, Some(gas_limit))?;
    Ok((logs, gas_used, gas_refunded))
}

/// Call creditGasFees to distribute gas fees.
/// State changes remain in the EVM's journal for the main transaction to see.
/// Returns (logs, gas_used, gas_refunded) where gas_used is net after refunds.
#[allow(clippy::too_many_arguments)]
pub fn credit_gas_fees<DB, INSP, P>(
    evm: &mut CeloEvm<DB, INSP, P>,
    fee_currency_address: Address,
    from: Address,
    fee_recipient: Address,
    community_fund: Address,
    refund: U256,
    tip_tx_fee: U256,
    base_tx_fee: U256,
    gas_limit: u64,
) -> Result<(Vec<Log>, u64, u64), CoreContractError>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
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
    let (_, logs, gas_used, gas_refunded) =
        core_contracts::call(evm, fee_currency_address, calldata, Some(gas_limit))?;

    Ok((logs, gas_used, gas_refunded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, keccak256};

    /// Mento StableTokenV2 (USDm) implementation bytecode deployed on Celo mainnet at
    /// 0x4B9B0E94197B7b2b11D311239e1420106cE7a2a2 (implementation behind the
    /// USDm proxy at 0x765DE816845861e75A25fCA122bb6898B8B1282a).
    /// Compiled with Solidity 0.8.24, which emits the PUSH0 opcode (EIP-3855).
    ///
    /// Fetched via: `cast code -r https://forno.celo.org 0x4B9B0E94197B7b2b11D311239e1420106cE7a2a2`
    const MENTO_STABLE_TOKEN_V2_HEX: &str = include_str!("testdata/mento_stable_token_v2.hex");

    /// Set up an InMemoryDB with the Mento StableTokenV2 (USDm) bytecode deployed
    /// at `contract_addr`, with `account_addr` having the given token `balance`.
    fn make_mento_usdm_db(
        contract_addr: Address,
        account_addr: Address,
        balance: U256,
    ) -> revm::database::InMemoryDB {
        use revm::{
            database::InMemoryDB,
            state::{AccountInfo, Bytecode},
        };

        let bytecode_bytes = alloy_primitives::hex::decode(MENTO_STABLE_TOKEN_V2_HEX.trim())
            .expect("invalid hex in mento_stable_token_v2.hex");
        let bytecode = Bytecode::new_raw(bytecode_bytes.into());

        // _balances mapping is at storage slot 5 in the StableTokenV2 contract.
        // Storage key = keccak256(abi.encode(account, uint256(5)))
        let balance_slot_key = keccak256(
            [
                account_addr.into_word().as_slice(),
                &U256::from(5).to_be_bytes::<32>(),
            ]
            .concat(),
        );

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            contract_addr,
            AccountInfo {
                balance: U256::ZERO,
                nonce: 0,
                code_hash: bytecode.hash_slow(),
                code: Some(bytecode),
                account_id: None,
            },
        );
        // Set _balances[account_addr] = balance
        db.insert_account_storage(
            contract_addr,
            U256::from_be_bytes(balance_slot_key.0),
            balance,
        )
        .unwrap();
        // Set _totalSupply (slot 6) >= balance so _burn doesn't underflow
        db.insert_account_storage(contract_addr, U256::from(6), balance)
            .unwrap();
        db
    }

    /// Regression test for a bug found in op-geth (celo-org/op-geth#484) where
    /// fee currency contract calls used a pre-Shanghai EVM configuration,
    /// causing PUSH0 (EIP-3855) to fail as an invalid opcode. Contracts
    /// compiled with Solidity >= 0.8.20 emit PUSH0 by default, so calls to
    /// fee currency contracts using such bytecode would fail.
    ///
    /// In celo-kona the spec is set explicitly via CfgEnv (not derived from
    /// block timestamp), so this bug doesn't apply the same way. This test
    /// serves as a regression guard to ensure fee currency gas deduction
    /// works correctly with real-world PUSH0-using contract bytecode.
    ///
    /// Uses the actual Mento StableTokenV2 (USDm) implementation bytecode
    /// deployed on Celo mainnet, compiled with Solidity 0.8.24.
    ///
    /// See also: <https://github.com/celo-org/celo-blockchain-planning/issues/1357>
    #[test]
    fn test_debit_gas_fees_push0_contract() {
        use crate::{CeloBuilder, DefaultCelo};
        use op_revm::OpSpecId;
        use revm::Context;

        let contract_addr = address!("0x765DE816845861e75A25fCA122bb6898B8B1282a");
        let account_addr = address!("0xBFce5EF4F16522D9059dA2776e4619F893A955cf");
        let initial_balance = U256::from(10_000_000_000_000_000u64); // 1e16
        let debit_amount = U256::from(1_000_000_000_000_000u64); // 1e15

        let db = make_mento_usdm_db(contract_addr, account_addr, initial_balance);

        // Canyon maps to SpecId::SHANGHAI which enables PUSH0.
        let ctx = Context::celo()
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::CANYON)
            .with_db(db);
        let mut evm = ctx.build_celo();

        debit_gas_fees(
            &mut evm,
            contract_addr,
            account_addr,
            debit_amount,
            u64::MAX,
        )
        .expect("debitGasFees should succeed with PUSH0-using contract on Canyon (Shanghai)");

        // Verify the balance was actually reduced via a second debitGasFees call.
        // Debiting the remaining balance should succeed, but debiting 1 more should fail.
        let remaining = initial_balance - debit_amount;
        debit_gas_fees(&mut evm, contract_addr, account_addr, remaining, u64::MAX)
            .expect("debitGasFees of remaining balance should succeed");
        debit_gas_fees(
            &mut evm,
            contract_addr,
            account_addr,
            U256::from(1),
            u64::MAX,
        )
        .expect_err("debitGasFees should fail with zero balance");
    }

    /// Verify that the same PUSH0-using contract fails on pre-Shanghai specs,
    /// confirming the test above is actually exercising the PUSH0 path.
    #[test]
    fn test_debit_gas_fees_push0_contract_fails_pre_shanghai() {
        use crate::{CeloBuilder, DefaultCelo};
        use op_revm::OpSpecId;
        use revm::Context;

        let contract_addr = address!("0x765DE816845861e75A25fCA122bb6898B8B1282a");
        let account_addr = address!("0xBFce5EF4F16522D9059dA2776e4619F893A955cf");
        let initial_balance = U256::from(10_000_000_000_000_000u64);
        let debit_amount = U256::from(1_000_000_000_000_000u64);

        let db = make_mento_usdm_db(contract_addr, account_addr, initial_balance);

        // Bedrock maps to SpecId::MERGE which does NOT have PUSH0.
        let ctx = Context::celo()
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::BEDROCK)
            .with_db(db);
        let mut evm = ctx.build_celo();

        let result = debit_gas_fees(
            &mut evm,
            contract_addr,
            account_addr,
            debit_amount,
            u64::MAX,
        );
        assert!(
            result.is_err(),
            "debitGasFees should fail with PUSH0-using contract on Bedrock (pre-Shanghai)"
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
}
