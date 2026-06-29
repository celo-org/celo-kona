//! [`transfer` precompile](https://specs.celo.org/token_duality.html#the-transfer-precompile)
//! For more details check [`transfer_run`] function.
//!
//! Note: `alloy-celo-evm` has a parallel implementation (`transfer_precompile`) that adapts the
//! same logic for `alloy-evm`'s stateless `DynPrecompile` interface, which receives balance
//! changes via `PrecompileInput::internals` rather than a full `ContextTr`. Both implementations
//! must be kept in sync.

use crate::constants;
use op_revm::OpSpecId;
use revm::{
    context::{Cfg, ContextTr, JournalTr},
    context_interface::journaled_state::account::JournaledAccountTr,
    interpreter::{CallInputs, Gas, InstructionResult, InterpreterResult},
    precompile::{
        PrecompileError, PrecompileHalt, PrecompileOutput, PrecompileResult, u64_to_address,
    },
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
        Ok(output) if output.is_success() => {
            let underflow = result.gas.record_regular_cost(output.gas_used);
            // SAFETY: gas_used <= gas_limit is guaranteed by the EVM, so underflow is
            // impossible. A panic here is intentional — silently returning OOG would risk
            // consensus divergence, while a crash is recoverable.
            assert!(underflow, "Gas underflow is not possible");
            result.result = InstructionResult::Return;
            result.output = output.bytes;
        }
        Ok(output) => {
            result.result = if matches!(output.halt_reason(), Some(PrecompileHalt::OutOfGas)) {
                InstructionResult::PrecompileOOG
            } else {
                InstructionResult::PrecompileError
            };
        }
        Err(PrecompileError::Fatal(e)) => return Err(e),
        Err(PrecompileError::FatalAny(e)) => return Err(format!("{e}")),
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
        return Ok(PrecompileOutput::halt(
            PrecompileHalt::Other(Cow::Borrowed(
                "transfer precompile cannot be called in static context",
            )),
            0,
        ));
    }

    if gas_limit < TRANSFER_GAS_COST {
        return Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, 0));
    }

    if caller_address != constants::get_addresses(context.cfg().chain_id()).celo_token {
        return Ok(PrecompileOutput::halt(
            PrecompileHalt::Other(Cow::Borrowed("invalid caller for transfer precompile")),
            0,
        ));
    }

    if input.len() != 96 {
        return Ok(PrecompileOutput::halt(
            PrecompileHalt::Other(Cow::Borrowed("invalid input length")),
            0,
        ));
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
        return Ok(PrecompileOutput::halt(
            PrecompileHalt::Other(Cow::Owned(format!(
                "transfer error occurred: {transfer_err:?}"
            ))),
            0,
        ));
    } else if let Err(db_err) = result {
        return Ok(PrecompileOutput::halt(
            PrecompileHalt::Other(Cow::Owned(format!("database error occurred: {db_err:?}"))),
            0,
        ));
    }

    Ok(PrecompileOutput::new(TRANSFER_GAS_COST, Bytes::new(), 0))
}

fn account_cold_status<CTX>(context: &mut CTX, address: Address) -> bool
where
    CTX: ContextTr<Cfg: Cfg<Spec = OpSpecId>>,
{
    // A missing account or load error is treated as cold.
    context
        .journal_mut()
        .load_account(address)
        .map_or(true, |account| account.is_cold)
}

fn revert_account_cold_status<CTX>(context: &mut CTX, address: Address, was_cold: bool)
where
    CTX: ContextTr<Cfg: Cfg<Spec = OpSpecId>>,
{
    if was_cold && let Ok(mut journaled_account) = context.journal_mut().load_account_mut(address) {
        journaled_account.unsafe_mark_cold();
    }
}
