//! [`transfer` precompile](https://specs.celo.org/token_duality.html#the-transfer-precompile)
//! For more details check [`transfer_run`] function.

use crate::chain_info;
use op_revm::OpSpecId;
use revm::{
    context::{Cfg, ContextTr, JournalTr, Transaction},
    interpreter::{Gas, InputsImpl, InstructionResult, InterpreterResult},
    precompile::{PrecompileError, PrecompileOutput, PrecompileResult, u64_to_address},
    primitives::{Address, Bytes, U256},
};
use std::{
    format,
    string::{String, ToString},
};

/// Address of the `transfer` precompile.
pub const TRANSFER_ADDRESS: Address = u64_to_address(0xff - 2);

/// Gas cost of the `transfer` precompile.
pub const TRANSFER_GAS_COST: u64 = 9_000;

/// Run `transfer` precompile.
pub fn transfer_run<CTX>(
    context: &mut CTX,
    inputs: &InputsImpl,
    gas_limit: u64,
) -> Result<Option<InterpreterResult>, String>
where
    CTX: ContextTr<Cfg: Cfg<Spec = OpSpecId>>,
{
    let mut result = InterpreterResult {
        result: InstructionResult::Return,
        gas: Gas::new(gas_limit),
        output: Bytes::new(),
    };

    match run(context, &inputs.input, gas_limit) {
        Ok(output) => {
            let underflow = result.gas.record_cost(output.gas_used);
            assert!(underflow, "Gas underflow is not possible");
            result.result = InstructionResult::Return;
            result.output = output.bytes;
        }
        Err(PrecompileError::Fatal(e)) => return Err(e),
        Err(e) => {
            result.result = if e.is_oog() {
                InstructionResult::PrecompileOOG
            } else {
                InstructionResult::PrecompileError
            };
        }
    }
    Ok(Some(result))
}

fn run<CTX>(context: &mut CTX, input: &Bytes, gas_limit: u64) -> PrecompileResult
where
    CTX: ContextTr<Cfg: Cfg<Spec = OpSpecId>>,
{
    if gas_limit < TRANSFER_GAS_COST {
        return Err(PrecompileError::OutOfGas);
    }

    if context.tx().caller() != chain_info::get_addresses(context.cfg().chain_id()).celo_token {
        return Err(PrecompileError::Other(
            "invalid caller for transfer precompile".to_string(),
        ));
    }

    if input.len() != 96 {
        return Err(PrecompileError::Other("invalid input length".to_string()));
    }

    let from = Address::from_slice(&input[12..32]);
    let to = Address::from_slice(&input[44..64]);
    let value = U256::from_be_slice(&input[64..96]);

    let result = context.journal().transfer(from, to, value);
    if let Ok(Some(transfer_err)) = result {
        return Err(PrecompileError::Other(format!(
            "transfer error occurred: {:?}",
            transfer_err
        )));
    } else if let Err(db_err) = result {
        return Err(PrecompileError::Other(format!(
            "database error occurred: {:?}",
            db_err
        )));
    }

    Ok(PrecompileOutput::new(TRANSFER_GAS_COST, Bytes::new()))
}
