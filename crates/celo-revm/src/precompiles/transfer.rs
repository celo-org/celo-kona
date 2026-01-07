//! [`transfer` precompile](https://specs.celo.org/token_duality.html#the-transfer-precompile)
//! For more details check [`transfer_run`] function.

use crate::constants;
use op_revm::OpSpecId;
use revm::{
    context::{Cfg, ContextTr, JournalTr},
    interpreter::{CallInputs, Gas, InstructionResult, InterpreterResult},
    precompile::{PrecompileError, PrecompileOutput, PrecompileResult, u64_to_address},
    primitives::{Address, Bytes, U256},
};
use std::borrow::Cow;
use std::{format, string::String};

/// Address of the `transfer` precompile.
pub const TRANSFER_ADDRESS: Address = u64_to_address(0xff - 2);

/// Gas cost of the `transfer` precompile.
pub const TRANSFER_GAS_COST: u64 = 9_000;

/// Run `transfer` precompile.
pub fn transfer_run<CTX>(
    context: &mut CTX,
    inputs: &CallInputs,
) -> Result<Option<InterpreterResult>, String>
where
    CTX: ContextTr<Cfg: Cfg<Spec = OpSpecId>>,
{
    let mut result = InterpreterResult {
        result: InstructionResult::Return,
        gas: Gas::new(inputs.gas_limit),
        output: Bytes::new(),
    };

    let input_bytes = inputs.input.bytes(context);
    match run(
        context,
        &input_bytes,
        inputs.caller,
        inputs.is_static,
        inputs.gas_limit,
    ) {
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

fn run<CTX>(
    context: &mut CTX,
    input: &Bytes,
    caller_address: Address,
    is_static: bool,
    gas_limit: u64,
) -> PrecompileResult
where
    CTX: ContextTr<Cfg: Cfg<Spec = OpSpecId>>,
{
    if is_static {
        return Err(PrecompileError::Other(Cow::Borrowed(
            "transfer precompile cannot be called in static context",
        )));
    }

    if gas_limit < TRANSFER_GAS_COST {
        return Err(PrecompileError::OutOfGas);
    }

    if caller_address != constants::get_addresses(context.cfg().chain_id()).celo_token {
        return Err(PrecompileError::Other(Cow::Borrowed(
            "invalid caller for transfer precompile",
        )));
    }

    if input.len() != 96 {
        return Err(PrecompileError::Other(Cow::Borrowed(
            "invalid input length",
        )));
    }

    let from = Address::from_slice(&input[12..32]);
    let to = Address::from_slice(&input[44..64]);
    let value = U256::from_be_slice(&input[64..96]);

    // Before Jovian, the Celo transfer precompile does not warm either address, so we need to
    // check if they were cold initially to match original Celo implementation behavior, and
    // make them cold again after the transfer. Starting with Jovian, this quirk is removed.
    let spec = context.cfg().spec();
    let revert_cold_status = !spec.is_enabled_in(OpSpecId::JOVIAN);
    let revert_from_cold = revert_cold_status && account_cold_status(context, from);
    let revert_to_cold = revert_cold_status && account_cold_status(context, to);

    // Now do the transfer (which will load both accounts and warm them)
    let result = context.journal_mut().transfer(from, to, value);

    // If the addresses were cold initially and we're pre-Jovian, make them cold again.
    revert_account_cold_status(context, from, revert_from_cold);
    revert_account_cold_status(context, to, revert_to_cold);

    if let Ok(Some(transfer_err)) = result {
        return Err(PrecompileError::Other(Cow::Owned(format!(
            "transfer error occurred: {transfer_err:?}"
        ))));
    } else if let Err(db_err) = result {
        return Err(PrecompileError::Other(Cow::Owned(format!(
            "database error occurred: {db_err:?}"
        ))));
    }

    Ok(PrecompileOutput::new(TRANSFER_GAS_COST, Bytes::new()))
}

fn account_cold_status<CTX>(context: &mut CTX, address: Address) -> bool
where
    CTX: ContextTr<Cfg: Cfg<Spec = OpSpecId>>,
{
    match context.journal_mut().load_account(address) {
        Ok(account) => account.is_cold,
        Err(_) => true, // If account doesn't exist or error loading, treat as cold
    }
}

fn revert_account_cold_status<CTX>(context: &mut CTX, address: Address, was_cold: bool)
where
    CTX: ContextTr<Cfg: Cfg<Spec = OpSpecId>>,
{
    if was_cold {
        if let Ok(mut journaled_account) = context.journal_mut().load_account_mut(address) {
            journaled_account.unsafe_mark_cold();
        }
    }
}
