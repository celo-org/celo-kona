//! # System calls for interacting with Celo core contracts
//!
//! System calls are executed without calling `finalize()`, using a "keep by default" approach
//! where state changes (accounts, storage) remain in the EVM's journal. This avoids needing
//! `set_storage` (which is not in upstream revm) to manually merge state back after system calls.
//!
//! When the main transaction reverts, fee debit changes persist because the main
//! transaction is executed as a subcall with automatic checkpoint/revert handling (see
//! [`make_call_frame`](https://github.com/bluealloy/revm/blob/main/crates/handler/src/frame.rs)).
//!
//! # Key Behaviors
//! - **State changes**: Remain in the journal (accounts, storage, etc.)
//! - **Logs**: Extracted and cleared from journal during `ExecutionResult` creation
//! - **Transient storage**: Explicitly cleared after each system call (EIP-1153 requirement)
//! - **transaction_id**: Restored to keep warmed addresses from system call

use crate::{
    CeloContext, constants::get_addresses, evm::CeloEvm, fee_currency_context::FeeCurrencyInfo,
};
use alloy_primitives::{
    Address, Bytes, U256, hex,
    map::{DefaultHashBuilder, HashMap},
};
use alloy_sol_types::{SolCall, SolType, sol, sol_data};
use revm::{
    Database,
    context_interface::ContextTr,
    handler::{EvmTr, PrecompileProvider, SystemCallEvm},
    inspector::Inspector,
    interpreter::InterpreterResult,
    primitives::Log,
};
use revm_context_interface::{
    ContextSetters,
    result::{ExecutionResult, Output},
};
use std::{
    format,
    string::{String, ToString},
    vec::Vec,
};
use tracing::debug;

#[derive(thiserror::Error, Debug)]
pub enum CoreContractError {
    #[error("Core contract missing at address {0}")]
    CoreContractMissing(Address),
    #[error("sol type error: {0}")]
    AlloySolTypes(#[from] alloy_sol_types::Error),
    #[error("core contract execution failed: {0}")]
    ExecutionFailed(String),
    #[error("Evm error: {0}")]
    Evm(String),
}

sol! {
    struct CurrencyConfig {
        address oracle;
        uint256 intrinsicGas;
    }

    function getCurrencies() external view returns (address[] memory currencies);
    function getExchangeRate(address token) view returns(uint256 numerator, uint256 denominator);
    function getCurrencyConfig(address token) public view returns (CurrencyConfig memory);
}

/// The 4-byte selector for the standard Solidity error `Error(string)`.
const ERROR_STRING_SELECTOR: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];

/// Extract the revert message from the output of an [ExecutionResult::Revert]
pub fn get_revert_message(output: Bytes) -> String {
    // Check if the output is long enough to contain the selector
    // and if it starts with the Error(string) selector.
    if output.len() >= ERROR_STRING_SELECTOR.len() && output.starts_with(&ERROR_STRING_SELECTOR) {
        // The actual ABI-encoded string data follows the selector.
        let abi_encoded_string_data = &output[ERROR_STRING_SELECTOR.len()..];

        // Attempt to decode the data as a single string.
        match <sol_data::String as SolType>::abi_decode(abi_encoded_string_data) {
            Ok(decoded_string) => decoded_string,
            Err(decoding_error) => {
                format!("could not decode: {output:?}, {decoding_error:?}")
            }
        }
    } else {
        format!("no revert message: {output:?}")
    }
}

/// Call a core contract function in read-only mode (checkpoint/revert to discard state changes).
/// Returns (output, logs, gas_used, gas_refunded) where gas_used is net after refunds.
pub fn call_read_only<DB, INSP, P>(
    evm: &mut CeloEvm<DB, INSP, P>,
    address: Address,
    calldata: Bytes,
    gas_limit: Option<u64>,
) -> Result<(Bytes, Vec<Log>, u64, u64), CoreContractError>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
{
    let checkpoint = evm.ctx().journal_mut().checkpoint();
    let result = call(evm, address, calldata, gas_limit);
    evm.ctx().journal_mut().checkpoint_revert(checkpoint);
    result
}

/// Call a core contract function. State changes remain in the EVM's journal.
/// Returns (output, logs, gas_used, gas_refunded) where gas_used is net after refunds.
pub fn call<DB, INSP, P>(
    evm: &mut CeloEvm<DB, INSP, P>,
    address: Address,
    calldata: Bytes,
    gas_limit: Option<u64>,
) -> Result<(Bytes, Vec<Log>, u64, u64), CoreContractError>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
{
    // Preserve the tx and transaction_id to restore afterwards
    let prev_tx = evm.ctx().tx().clone();
    let prev_transaction_id = evm.ctx().journal_ref().transaction_id;

    let call_result = if let Some(limit) = gas_limit {
        evm.transact_system_call_with_gas_limit(address, calldata, limit)
    } else {
        evm.system_call_one(address, calldata)
    };

    // Restore the original transaction context
    evm.ctx().set_tx(prev_tx);
    // Clear transient storage (EIP-1153)
    evm.ctx().journal_mut().transient_storage.clear();
    // Restore transaction_id for correct warm/cold accounting
    evm.ctx().journal_mut().transaction_id = prev_transaction_id;

    let exec_result = match call_result {
        Err(e) => return Err(CoreContractError::Evm(e.to_string())),
        Ok(o) => o,
    };

    // Check success
    match exec_result {
        ExecutionResult::Success {
            output: Output::Call(bytes),
            gas_used,
            gas_refunded,
            logs,
            ..
        } => Ok((bytes, logs, gas_used, gas_refunded)),
        ExecutionResult::Halt { reason, .. } => Err(CoreContractError::ExecutionFailed(format!(
            "halt: {reason:?}"
        ))),
        ExecutionResult::Revert { output, .. } => Err(CoreContractError::ExecutionFailed(format!(
            "revert: {}",
            get_revert_message(output)
        ))),
        _ => Err(CoreContractError::ExecutionFailed(
            "unexpected result".into(),
        )),
    }
}

/// Fetches the list of registered fee currencies from the FeeCurrencyDirectory contract.
fn get_currencies<DB, INSP, P>(
    evm: &mut CeloEvm<DB, INSP, P>,
    fee_currency_directory: Address,
) -> Vec<Address>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
{
    let call_result = call_read_only(
        evm,
        fee_currency_directory,
        getCurrenciesCall {}.abi_encode().into(),
        None,
    );

    let output_bytes = match call_result {
        Ok((bytes, _, _, _)) => bytes,
        Err(e) => {
            debug!(target: "celo_core_contracts", "get_currencies: failed to call 0x{:x}: {}", fee_currency_directory, e);
            return Vec::new();
        }
    };

    if output_bytes.is_empty() {
        debug!(target: "celo_core_contracts", "get_currencies: core contract missing at address 0x{:x}", fee_currency_directory);
        return Vec::new();
    }

    // Decode the output
    match getCurrenciesCall::abi_decode_returns(output_bytes.as_ref()) {
        Ok(decoded_return) => decoded_return,
        Err(e) => {
            debug!(target: "celo_core_contracts", "get_currencies: failed to decode (bytes: 0x{}): {}", hex::encode(output_bytes), e);
            Vec::new()
        }
    }
}

/// Fetches complete currency info (exchange rate + intrinsic gas) for all registered currencies.
/// A currency is only included if BOTH pieces of data are successfully fetched.
/// This ensures no partial/inconsistent currency data can exist.
pub fn get_currency_info<DB, INSP, P>(
    evm: &mut CeloEvm<DB, INSP, P>,
) -> HashMap<Address, FeeCurrencyInfo>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
{
    let fee_currency_directory = get_addresses(evm.ctx_ref().cfg().chain_id).fee_currency_directory;
    let currencies = get_currencies(evm, fee_currency_directory);
    let mut currency_info =
        HashMap::with_capacity_and_hasher(currencies.len(), DefaultHashBuilder::default());

    for token in currencies {
        // Fetch exchange rate
        let exchange_rate = match get_exchange_rate(evm, fee_currency_directory, token) {
            Some(rate) => rate,
            None => continue,
        };

        // Fetch intrinsic gas
        let intrinsic_gas = match get_intrinsic_gas(evm, fee_currency_directory, token) {
            Some(gas) => gas,
            None => continue,
        };

        // Only insert if BOTH succeeded
        _ = currency_info.insert(
            token,
            FeeCurrencyInfo {
                exchange_rate,
                intrinsic_gas,
            },
        );
    }

    currency_info
}

/// Fetches the exchange rate for a single token. Returns None on any failure.
fn get_exchange_rate<DB, INSP, P>(
    evm: &mut CeloEvm<DB, INSP, P>,
    fee_currency_directory: Address,
    token: Address,
) -> Option<(U256, U256)>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
{
    let call_result = call_read_only(
        evm,
        fee_currency_directory,
        getExchangeRateCall { token }.abi_encode().into(),
        None,
    );

    let output_bytes = match call_result {
        Ok((bytes, _, _, _)) => bytes,
        Err(e) => {
            debug!(target: "celo_core_contracts", "get_exchange_rate: failed to get exchange rate for token 0x{:x}: {}", token, e);
            return None;
        }
    };

    let rate = match getExchangeRateCall::abi_decode_returns(output_bytes.as_ref()) {
        Ok(decoded_return) => decoded_return,
        Err(e) => {
            debug!(target: "celo_core_contracts", "get_exchange_rate: failed to decode exchange rate for token 0x{:x} (bytes: 0x{:x}): {}", token, output_bytes, e);
            return None;
        }
    };

    // Validate that neither numerator nor denominator is zero
    if rate.numerator.is_zero() || rate.denominator.is_zero() {
        debug!(target: "celo_core_contracts", "get_exchange_rate: invalid exchange rate for token 0x{:x} (numerator: {}, denominator: {})", token, rate.numerator, rate.denominator);
        return None;
    }

    Some((rate.numerator, rate.denominator))
}

/// Fetches the intrinsic gas for a single token. Returns None on any failure.
fn get_intrinsic_gas<DB, INSP, P>(
    evm: &mut CeloEvm<DB, INSP, P>,
    fee_currency_directory: Address,
    token: Address,
) -> Option<u64>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
{
    let call_result = call_read_only(
        evm,
        fee_currency_directory,
        getCurrencyConfigCall { token }.abi_encode().into(),
        None,
    );

    let output_bytes = match call_result {
        Ok((bytes, _, _, _)) => bytes,
        Err(e) => {
            debug!(target: "celo_core_contracts", "get_intrinsic_gas: failed to get intrinsic gas for token 0x{:x}: {}", token, e);
            return None;
        }
    };

    let curr_conf = match getCurrencyConfigCall::abi_decode_returns(output_bytes.as_ref()) {
        Ok(decoded_return) => decoded_return,
        Err(e) => {
            debug!(target: "celo_core_contracts", "get_intrinsic_gas: failed to decode intrinsic gas for token 0x{:x} (bytes: 0x{}): {}", token, hex::encode(output_bytes), e);
            return None;
        }
    };

    // Convert U256 to u64, capping at u64::MAX if the value is too large
    let intrinsic_gas_value = curr_conf.intrinsicGas.try_into().unwrap_or_else(|_| {
        debug!(target: "celo_core_contracts", "get_intrinsic_gas: intrinsic gas exceeds u64::MAX for token 0x{:x}, capping at u64::MAX", token);
        u64::MAX
    });

    Some(intrinsic_gas_value)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{CeloBuilder, DefaultCelo};
    use alloy_primitives::{address, hex, keccak256};
    use revm::{
        Context,
        database::InMemoryDB,
        primitives::{Address, Bytes, U256},
        state::{AccountInfo, Bytecode},
    };

    pub(crate) fn make_celo_test_db() -> InMemoryDB {
        let oracle_address = address!("0x1111111111111111111111111111111111111112");
        let fee_currency_address = address!("0x1111111111111111111111111111111111111111");
        let mut db = InMemoryDB::default();

        // MockOracle contract code
        {
            let contract_data: Bytes = hex!("0x608060405234801561001057600080fd5b50600436106100365760003560e01c806358a5514f1461003b578063efb7601d1461007a575b600080fd5b61007861004936600461012d565b60009190915560015542600255600380546001600160a01b0319166001600160a01b0392909216919091179055565b005b61008d610088366004610160565b6100a6565b6040805192835260208301919091520160405180910390f35b60035460009081906001600160a01b038481169116146101025760405162461bcd60e51b8152602060048201526013602482015272151bdad95b881b9bdd081cdd5c1c1bdc9d1959606a1b604482015260640160405180910390fd5b60005460015491509150915091565b80356001600160a01b038116811461012857600080fd5b919050565b60008060006060848603121561014257600080fd5b61014b84610111565b95602085013595506040909401359392505050565b60006020828403121561017257600080fd5b61017b82610111565b939250505056fea2646970667358221220532d5a8180e3477753af960cd2ec6ffab9b57df9b867e656e78ca4ec2164930664736f6c63430008130033").into();
            let bytecode = Bytecode::new_raw(contract_data);

            let account_info = AccountInfo {
                balance: U256::from(0),
                nonce: 0,
                code_hash: bytecode.hash_slow(),
                account_id: None,
                code: Some(bytecode),
            };
            db.insert_account_info(oracle_address, account_info);
        }
        db.insert_account_storage(
            oracle_address,
            U256::from(0),
            U256::from(20), // numerator
        )
        .unwrap();
        db.insert_account_storage(
            oracle_address,
            U256::from(1),
            U256::from(10), // denominator
        )
        .unwrap();
        db.insert_account_storage(
            oracle_address,
            U256::from(3),
            fee_currency_address.into_word().into(),
        )
        .unwrap();

        // FeeCurrencyDirectory contract code
        {
            let contract_data: Bytes = hex!("0x608060405234801561001057600080fd5b50600436106100b45760003560e01c8063715018a611610071578063715018a6146101905780638129fc1c146101985780638da5cb5b146101a0578063eab43d97146101c9578063efb7601d14610245578063f2fde38b1461026d57600080fd5b8063158ef93e146100b957806316be73a8146100db578063216ab7df146100f057806354255be0146101035780636036cba31461012957806361c661de1461017b575b600080fd5b6000546100c69060ff1681565b60405190151581526020015b60405180910390f35b6100ee6100e9366004610939565b610280565b005b6100ee6100fe366004610963565b61045d565b6001806000806040805194855260208501939093529183015260608201526080016100d2565b61015c61013736600461099f565b600160208190526000918252604090912080549101546001600160a01b039091169082565b604080516001600160a01b0390931683526020830191909152016100d2565b61018361062a565b6040516100d291906109c1565b6100ee61068c565b6100ee6106c8565b60005461010090046001600160a01b03166040516001600160a01b0390911681526020016100d2565b6102216101d736600461099f565b604080518082018252600080825260209182018190526001600160a01b03938416815260018083529083902083518085019094528054909416835292909201549181019190915290565b6040805182516001600160a01b0316815260209283015192810192909252016100d2565b61025861025336600461099f565b610731565b604080519283526020830191909152016100d2565b6100ee61027b36600461099f565b610823565b6000546001600160a01b036101009091041633146102b95760405162461bcd60e51b81526004016102b090610a0e565b60405180910390fd5b60025481106103005760405162461bcd60e51b8152602060048201526013602482015272496e646578206f7574206f6620626f756e647360681b60448201526064016102b0565b816001600160a01b03166002828154811061031d5761031d610a43565b6000918252602090912001546001600160a01b03161461037f5760405162461bcd60e51b815260206004820152601a60248201527f496e64657820646f6573206e6f74206d6174636820746f6b656e00000000000060448201526064016102b0565b6001600160a01b0382166000908152600160208190526040822080546001600160a01b03191681558101919091556002805490916103bc91610a59565b815481106103cc576103cc610a43565b600091825260209091200154600280546001600160a01b0390921691839081106103f8576103f8610a43565b9060005260206000200160006101000a8154816001600160a01b0302191690836001600160a01b03160217905550600280548061043757610437610a80565b600082815260209020810160001990810180546001600160a01b03191690550190555050565b6000546001600160a01b0361010090910416331461048d5760405162461bcd60e51b81526004016102b090610a0e565b6001600160a01b0382166104e35760405162461bcd60e51b815260206004820152601d60248201527f4f7261636c6520616464726573732063616e6e6f74206265207a65726f00000060448201526064016102b0565b600081116105335760405162461bcd60e51b815260206004820152601c60248201527f496e7472696e736963206761732063616e6e6f74206265207a65726f0000000060448201526064016102b0565b6001600160a01b0383811660009081526001602052604090205416156105a55760405162461bcd60e51b815260206004820152602160248201527f43757272656e637920616c726561647920696e20746865206469726563746f726044820152607960f81b60648201526084016102b0565b6040805180820182526001600160a01b039384168152602080820193845294841660008181526001968790529283209151825495166001600160a01b031995861617825592519085015560028054948501815590527f405787fa12a823e0f2b7631cc41b3ba8828b3321ca811111fa75cd3aa3bb5ace90920180549091169091179055565b6060600280548060200260200160405190810160405280929190818152602001828054801561068257602002820191906000526020600020905b81546001600160a01b03168152600190910190602001808311610664575b5050505050905090565b6000546001600160a01b036101009091041633146106bc5760405162461bcd60e51b81526004016102b090610a0e565b6106c660006108c4565b565b60005460ff161561071b5760405162461bcd60e51b815260206004820152601c60248201527f636f6e747261637420616c726561647920696e697469616c697a65640000000060448201526064016102b0565b6000805460ff191660011790556106c6336108c4565b6001600160a01b03818116600090815260016020526040812054909182911661079c5760405162461bcd60e51b815260206004820152601d60248201527f43757272656e6379206e6f7420696e20746865206469726563746f727900000060448201526064016102b0565b6001600160a01b038381166000818152600160205260409081902054905163efb7601d60e01b815260048101929092529091169063efb7601d906024016040805180830381865afa1580156107f5573d6000803e3d6000fd5b505050506040513d601f19601f820116820180604052508101906108199190610a96565b9094909350915050565b6000546001600160a01b036101009091041633146108535760405162461bcd60e51b81526004016102b090610a0e565b6001600160a01b0381166108b85760405162461bcd60e51b815260206004820152602660248201527f4f776e61626c653a206e6577206f776e657220697320746865207a65726f206160448201526564647265737360d01b60648201526084016102b0565b6108c1816108c4565b50565b600080546001600160a01b03838116610100818102610100600160a81b0319851617855560405193049190911692909183917f8be0079c531659141344cd1fd0a4f28419497f9722a3daafe3b4186f6b6457e091a35050565b80356001600160a01b038116811461093457600080fd5b919050565b6000806040838503121561094c57600080fd5b6109558361091d565b946020939093013593505050565b60008060006060848603121561097857600080fd5b6109818461091d565b925061098f6020850161091d565b9150604084013590509250925092565b6000602082840312156109b157600080fd5b6109ba8261091d565b9392505050565b6020808252825182820181905260009190848201906040850190845b81811015610a025783516001600160a01b0316835292840192918401916001016109dd565b50909695505050505050565b6020808252818101527f4f776e61626c653a2063616c6c6572206973206e6f7420746865206f776e6572604082015260600190565b634e487b7160e01b600052603260045260246000fd5b81810381811115610a7a57634e487b7160e01b600052601160045260246000fd5b92915050565b634e487b7160e01b600052603160045260246000fd5b60008060408385031215610aa957600080fd5b50508051602090910151909290915056fea2646970667358221220127159ea8f76efe84815c2177266f0115f42dfbdd3b1fd1624548e208504750e64736f6c63430008130033").into();
            let bytecode = Bytecode::new_raw(contract_data);

            let account_info = AccountInfo {
                balance: U256::from(0),
                nonce: 0,
                code_hash: bytecode.hash_slow(),
                account_id: None,
                code: Some(bytecode),
            };
            db.insert_account_info(get_addresses(0).fee_currency_directory, account_info);
        }

        // Add currencies: Address[] at slot 2
        let currencies_slot_number = U256::from(2);
        db.insert_account_storage(
            get_addresses(0).fee_currency_directory,
            currencies_slot_number, // slot
            U256::from(1),          // value: lenght of array
        )
        .unwrap();

        // Calculate start of array content
        let currencies_slot_number_u8arr: [u8; 32] = currencies_slot_number.to_be_bytes();
        let currencies_data_start_b256 = keccak256(currencies_slot_number_u8arr);
        let currencies_data_start = U256::from_be_bytes(currencies_data_start_b256.0);

        db.insert_account_storage(
            get_addresses(0).fee_currency_directory,
            currencies_data_start,
            fee_currency_address.into_word().into(),
        )
        .unwrap();

        fn calc_map_addr(slot: u8, key: Address) -> U256 {
            let concatted_bytes = [key.into_word().into(), U256::from(slot).to_be_bytes()].concat();
            U256::from_be_bytes(keccak256(concatted_bytes).0)
        }

        // Add oracle address in FeeCurrencyDirectory
        let struct_start_slot = calc_map_addr(1, fee_currency_address);
        db.insert_account_storage(
            get_addresses(0).fee_currency_directory,
            struct_start_slot,
            oracle_address.into_word().into(),
        )
        .unwrap();

        // Set intrinsic gas in FeeCurrencyDirectory
        db.insert_account_storage(
            get_addresses(0).fee_currency_directory,
            struct_start_slot + U256::from(1),
            U256::from(50_000),
        )
        .unwrap();

        db
    }

    /// Fee currency address used in test DBs.
    pub(crate) const TEST_FEE_CURRENCY: Address =
        address!("0x1111111111111111111111111111111111111111");

    /// FeeCurrency ERC20 runtime bytecode from celo-dev-genesis.json (0xce16).
    /// Supports standard ERC20 + Celo's debitGasFees/creditGasFees (onlyVm).
    const FEE_CURRENCY_BYTECODE: &[u8] = &hex!(
        "608060405234801561001057600080fd5b50600436106100df5760003560e01c806358cf96721161008c57806395d89b411161006657806395d89b41146101ca578063a457c2d7146101d2578063a9059cbb146101e5578063dd62ed3e146101f857600080fd5b806358cf96721461016c5780636a30b2531461018157806370a082311461019457600080fd5b806323b872dd116100bd57806323b872dd14610137578063313ce5671461014a578063395093511461015957600080fd5b806306fdde03146100e4578063095ea7b31461010257806318160ddd14610125575b600080fd5b6100ec61023e565b6040516100f99190610c15565b60405180910390f35b610115610110366004610cb1565b6102d0565b60405190151581526020016100f9565b6002545b6040519081526020016100f9565b610115610145366004610cdb565b6102e8565b604051601281526020016100f9565b610115610167366004610cb1565b61030e565b61017f61017a366004610cb1565b61035a565b005b61017f61018f366004610d17565b61041e565b6101296101a2366004610d8f565b73ffffffffffffffffffffffffffffffffffffffff1660009081526020819052604090205490565b6100ec610510565b6101156101e0366004610cb1565b61051f565b6101156101f3366004610cb1565b6105fb565b610129610206366004610daa565b73ffffffffffffffffffffffffffffffffffffffff918216600090815260016020908152604080832093909416825291909152205490565b60606003805461024d90610ddd565b80601f016020809104026020016040519081016040528092919081815260200182805461027990610ddd565b80156102c65780601f1061029b576101008083540402835291602001916102c6565b820191906000526020600020905b8154815290600101906020018083116102a957829003601f168201915b5050505050905090565b6000336102de818585610609565b5060019392505050565b6000336102f68582856107bc565b610301858585610893565b60019150505b9392505050565b33600081815260016020908152604080832073ffffffffffffffffffffffffffffffffffffffff871684529091528120549091906102de9082908690610355908790610e5f565b610609565b33156103c7576040517f08c379a000000000000000000000000000000000000000000000000000000000815260206004820152601060248201527f4f6e6c7920564d2063616e2063616c6c0000000000000000000000000000000060448201526064015b60405180910390fd5b73ffffffffffffffffffffffffffffffffffffffff8216600090815260208190526040812080548392906103fc908490610e77565b9250508190555080600260008282546104159190610e77565b90915550505050565b3315610486576040517f08c379a000000000000000000000000000000000000000000000000000000000815260206004820152601060248201527f4f6e6c7920564d2063616e2063616c6c0000000000000000000000000000000060448201526064016103be565b73ffffffffffffffffffffffffffffffffffffffff8816600090815260208190526040812080548692906104bb908490610e5f565b909155506104cc9050888683610b46565b6104d69085610e5f565b93506104e3888885610b46565b6104ed9085610e5f565b935083600260008282546105019190610e5f565b90915550505050505050505050565b60606004805461024d90610ddd565b33600081815260016020908152604080832073ffffffffffffffffffffffffffffffffffffffff87168452909152812054909190838110156105e3576040517f08c379a000000000000000000000000000000000000000000000000000000000815260206004820152602560248201527f45524332303a2064656372656173656420616c6c6f77616e63652062656c6f7760448201527f207a65726f00000000000000000000000000000000000000000000000000000060648201526084016103be565b6105f08286868403610609565b506001949350505050565b6000336102de818585610893565b73ffffffffffffffffffffffffffffffffffffffff83166106ab576040517f08c379a0000000000000000000000000000000000000000000000000000000008152602060048201526024808201527f45524332303a20617070726f76652066726f6d20746865207a65726f2061646460448201527f726573730000000000000000000000000000000000000000000000000000000060648201526084016103be565b73ffffffffffffffffffffffffffffffffffffffff821661074e576040517f08c379a000000000000000000000000000000000000000000000000000000000815260206004820152602260248201527f45524332303a20617070726f766520746f20746865207a65726f20616464726560448201527f737300000000000000000000000000000000000000000000000000000000000060648201526084016103be565b73ffffffffffffffffffffffffffffffffffffffff83811660008181526001602090815260408083209487168084529482529182902085905590518481527f8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925910160405180910390a3505050565b73ffffffffffffffffffffffffffffffffffffffff8381166000908152600160209081526040808320938616835292905220547fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff811461088d5781811015610880576040517f08c379a000000000000000000000000000000000000000000000000000000000815260206004820152601d60248201527f45524332303a20696e73756666696369656e7420616c6c6f77616e636500000060448201526064016103be565b61088d8484848403610609565b50505050565b73ffffffffffffffffffffffffffffffffffffffff8316610936576040517f08c379a000000000000000000000000000000000000000000000000000000000815260206004820152602560248201527f45524332303a207472616e736665722066726f6d20746865207a65726f20616460448201527f647265737300000000000000000000000000000000000000000000000000000060648201526084016103be565b73ffffffffffffffffffffffffffffffffffffffff82166109d9576040517f08c379a000000000000000000000000000000000000000000000000000000000815260206004820152602360248201527f45524332303a207472616e7366657220746f20746865207a65726f206164647260448201527f657373000000000000000000000000000000000000000000000000000000000060648201526084016103be565b73ffffffffffffffffffffffffffffffffffffffff831660009081526020819052604090205481811015610a8f576040517f08c379a000000000000000000000000000000000000000000000000000000000815260206004820152602660248201527f45524332303a207472616e7366657220616d6f756e742065786365656473206260448201527f616c616e6365000000000000000000000000000000000000000000000000000060648201526084016103be565b73ffffffffffffffffffffffffffffffffffffffff808516600090815260208190526040808220858503905591851681529081208054849290610ad3908490610e5f565b925050819055508273ffffffffffffffffffffffffffffffffffffffff168473ffffffffffffffffffffffffffffffffffffffff167fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef84604051610b3991815260200190565b60405180910390a361088d565b600073ffffffffffffffffffffffffffffffffffffffff8316610b6b57506000610307565b73ffffffffffffffffffffffffffffffffffffffff831660009081526020819052604081208054849290610ba0908490610e5f565b925050819055508273ffffffffffffffffffffffffffffffffffffffff168473ffffffffffffffffffffffffffffffffffffffff167fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef84604051610c0691815260200190565b60405180910390a35092915050565b600060208083528351808285015260005b81811015610c4257858101830151858201604001528201610c26565b81811115610c54576000604083870101525b50601f017fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe016929092016040019392505050565b803573ffffffffffffffffffffffffffffffffffffffff81168114610cac57600080fd5b919050565b60008060408385031215610cc457600080fd5b610ccd83610c88565b946020939093013593505050565b600080600060608486031215610cf057600080fd5b610cf984610c88565b9250610d0760208501610c88565b9150604084013590509250925092565b600080600080600080600080610100898b031215610d3457600080fd5b610d3d89610c88565b9750610d4b60208a01610c88565b9650610d5960408a01610c88565b9550610d6760608a01610c88565b979a969950949760808101359660a0820135965060c0820135955060e0909101359350915050565b600060208284031215610da157600080fd5b61030782610c88565b60008060408385031215610dbd57600080fd5b610dc683610c88565b9150610dd460208401610c88565b90509250929050565b600181811c90821680610df157607f821691505b602082108103610e2a577f4e487b7100000000000000000000000000000000000000000000000000000000600052602260045260246000fd5b50919050565b7f4e487b7100000000000000000000000000000000000000000000000000000000600052601160045260246000fd5b60008219821115610e7257610e72610e30565b500190565b600082821015610e8957610e89610e30565b50039056fea164736f6c634300080f000a"
    );

    /// Build a test DB with the FeeCurrencyDirectory, MockOracle, AND a real
    /// FeeCurrency ERC20 contract deployed at `TEST_FEE_CURRENCY`.
    ///
    /// The ERC20 contract's `_balances[sender]` and `_totalSupply` are set so
    /// that `debitGasFees` / `creditGasFees` work end-to-end.
    pub(crate) fn make_celo_test_db_with_fee_currency(
        sender: Address,
        fc_balance: U256,
    ) -> InMemoryDB {
        let mut db = make_celo_test_db();

        // Deploy the FeeCurrency ERC20 at TEST_FEE_CURRENCY
        let bytecode = Bytecode::new_raw(FEE_CURRENCY_BYTECODE.into());
        let account_info = AccountInfo {
            balance: U256::ZERO,
            nonce: 0,
            code_hash: bytecode.hash_slow(),
            account_id: None,
            code: Some(bytecode),
        };
        db.insert_account_info(TEST_FEE_CURRENCY, account_info);

        // Set _balances[sender] at slot keccak256(abi.encode(sender, 0))
        // Solidity mapping slot 0: _balances
        let balance_slot = {
            let mut buf = [0u8; 64];
            buf[12..32].copy_from_slice(sender.as_slice()); // left-padded address
            // slot 0 is already zero in buf[32..64]
            U256::from_be_bytes(keccak256(buf).0)
        };
        db.insert_account_storage(TEST_FEE_CURRENCY, balance_slot, fc_balance)
            .unwrap();

        // Set _totalSupply at slot 2
        db.insert_account_storage(TEST_FEE_CURRENCY, U256::from(2), fc_balance)
            .unwrap();

        // Give sender some native CELO for value transfers
        db.insert_account_info(
            sender,
            AccountInfo {
                balance: U256::from(1_000_000_000_000_000_000u128), // 1 CELO
                nonce: 0,
                code_hash: Default::default(),
                account_id: None,
                code: None,
            },
        );

        db
    }

    #[test]
    fn test_get_currency_info() {
        let ctx = Context::celo().with_db(make_celo_test_db());
        let mut evm = ctx.build_celo();
        let currency_info = get_currency_info(&mut evm);

        let mut expected = HashMap::with_hasher(DefaultHashBuilder::default());
        _ = expected.insert(
            address!("0x1111111111111111111111111111111111111111"),
            FeeCurrencyInfo {
                exchange_rate: (U256::from(20), U256::from(10)),
                intrinsic_gas: 50_000,
            },
        );
        assert_eq!(currency_info, expected);
    }
}
