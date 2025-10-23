use crate::{CeloContext, constants::get_addresses, evm::CeloEvm};
use alloy_primitives::{
    Address, Bytes, U256, hex,
    map::{DefaultHashBuilder, HashMap},
};
use alloy_sol_types::{SolCall, SolType, sol, sol_data};
use revm::{
    Database, ExecuteEvm,
    context_interface::ContextTr,
    handler::{EvmTr, SystemCallEvm},
    inspector::Inspector,
    primitives::Log,
    state::EvmState,
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

/// Call a core contract function and return the result.
pub fn call<DB, INSP>(
    evm: &mut CeloEvm<DB, INSP>,
    address: Address,
    calldata: Bytes,
    gas_limit: Option<u64>,
) -> Result<(Bytes, EvmState, Vec<Log>, u64), CoreContractError>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
{
    // Preserve the tx set in the evm before the call to restore it afterwards
    let prev_tx = evm.ctx().tx().clone();

    let call_result = if let Some(limit) = gas_limit {
        evm.transact_system_call_with_gas_limit(address, calldata, limit)
    } else {
        evm.system_call_one(address, calldata)
    };

    // Restore the original transaction context
    evm.ctx().set_tx(prev_tx);

    let exec_result = match call_result {
        Err(e) => return Err(CoreContractError::Evm(e.to_string())),
        Ok(o) => o,
    };

    // Get logs from the execution result
    let logs_from_call = match &exec_result {
        ExecutionResult::Success { logs, .. } => logs.clone(),
        _ => Vec::new(),
    };

    let state = evm.finalize();

    // Check success
    match exec_result {
        ExecutionResult::Success {
            output: Output::Call(bytes),
            gas_used,
            ..
        } => Ok((bytes, state, logs_from_call, gas_used)),
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

pub fn get_currencies<DB, INSP>(
    evm: &mut CeloEvm<DB, INSP>,
) -> Result<Vec<Address>, CoreContractError>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
{
    let fee_curr_dir = get_addresses(evm.ctx_ref().cfg().chain_id).fee_currency_directory;
    let (output_bytes, _, _, _) = call(
        evm,
        fee_curr_dir,
        getCurrenciesCall {}.abi_encode().into(),
        None,
    )?;

    if output_bytes.is_empty() {
        return Err(CoreContractError::CoreContractMissing(fee_curr_dir));
    }

    // Decode the output
    match getCurrenciesCall::abi_decode_returns(output_bytes.as_ref()) {
        Ok(decoded_return) => Ok(decoded_return),
        Err(e) => Err(CoreContractError::ExecutionFailed(format!(
            "Failed to decode getCurrenciesCall return (bytes: 0x{}): {}",
            hex::encode(output_bytes),
            e
        ))),
    }
}

pub fn get_exchange_rates<DB, INSP>(
    evm: &mut CeloEvm<DB, INSP>,
    currencies: &[Address],
) -> Result<HashMap<Address, (U256, U256)>, CoreContractError>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
{
    let mut exchange_rates =
        HashMap::with_capacity_and_hasher(currencies.len(), DefaultHashBuilder::default());

    for token in currencies {
        let (output_bytes, _, _, _) = call(
            evm,
            get_addresses(evm.ctx_ref().cfg().chain_id).fee_currency_directory,
            getExchangeRateCall { token: *token }.abi_encode().into(),
            None,
        )?;

        // Decode the output
        let rate = match getExchangeRateCall::abi_decode_returns(output_bytes.as_ref()) {
            Ok(decoded_return) => decoded_return,
            Err(e) => {
                return Err(CoreContractError::ExecutionFailed(format!(
                    "Failed to decode getExchangeRateCall return for token 0x{} (bytes: 0x{}): {}",
                    hex::encode(token),
                    hex::encode(output_bytes),
                    e
                )));
            }
        };

        _ = exchange_rates.insert(*token, (rate.numerator, rate.denominator))
    }

    Ok(exchange_rates)
}

pub fn get_intrinsic_gas<DB, INSP>(
    evm: &mut CeloEvm<DB, INSP>,
    currencies: &[Address],
) -> Result<HashMap<Address, u64>, CoreContractError>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
{
    let mut intrinsic_gas =
        HashMap::with_capacity_and_hasher(currencies.len(), DefaultHashBuilder::default());

    for token in currencies {
        let (output_bytes, _, _, _) = call(
            evm,
            get_addresses(evm.ctx_ref().cfg().chain_id).fee_currency_directory,
            getCurrencyConfigCall { token: *token }.abi_encode().into(),
            None,
        )?;

        // Decode the output
        let curr_conf = match getCurrencyConfigCall::abi_decode_returns(output_bytes.as_ref()) {
            Ok(decoded_return) => decoded_return,
            Err(e) => {
                return Err(CoreContractError::ExecutionFailed(format!(
                    "Failed to decode getCurrencyConfigCall return for token 0x{} (bytes: 0x{}): {}",
                    hex::encode(token),
                    hex::encode(output_bytes),
                    e
                )));
            }
        };

        _ = intrinsic_gas.insert(
            *token,
            curr_conf
                .intrinsicGas
                .try_into()
                .expect("Failed to convert U256 to u64: value exceeds u64 range"),
        );
    }

    Ok(intrinsic_gas)
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

    #[test]
    fn test_get_currencies() {
        let ctx = Context::celo().with_db(make_celo_test_db());
        let mut evm = ctx.build_celo();
        let currencies = get_currencies(&mut evm).unwrap();

        assert_eq!(
            currencies,
            vec![address!("0x1111111111111111111111111111111111111111")]
        );
    }

    #[test]
    fn test_get_exchange_rates() {
        let ctx = Context::celo().with_db(make_celo_test_db());
        let mut evm = ctx.build_celo();
        let exchange_rates = get_exchange_rates(
            &mut evm,
            &[address!("0x1111111111111111111111111111111111111111")],
        )
        .unwrap();

        let mut expected = HashMap::with_hasher(DefaultHashBuilder::default());
        _ = expected.insert(
            address!("0x1111111111111111111111111111111111111111"),
            (U256::from(20), U256::from(10)),
        );
        assert_eq!(exchange_rates, expected);
    }

    #[test]
    fn test_get_intrinsic_gas() {
        let ctx = Context::celo().with_db(make_celo_test_db());
        let mut evm = ctx.build_celo();
        let intrinsic_gas = get_intrinsic_gas(
            &mut evm,
            &[address!("0x1111111111111111111111111111111111111111")],
        )
        .unwrap();

        let mut expected = HashMap::with_hasher(DefaultHashBuilder::default());
        _ = expected.insert(
            address!("0x1111111111111111111111111111111111111111"),
            50_000,
        );
        assert_eq!(intrinsic_gas, expected);
    }
}
