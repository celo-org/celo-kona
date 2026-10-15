//! [`transfer` precompile](https://specs.celo.org/token_duality.html#the-transfer-precompile)
//! Built into the precompile set by [`super::celo_precompiles_map`].

use crate::constants;
use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use op_revm::OpSpecId;
use revm::{
    context_interface::result::AnyError,
    precompile::{
        PrecompileError, PrecompileHalt, PrecompileOutput, PrecompileResult, u64_to_address,
    },
    primitives::{Address, Bytes, U256},
};
use std::borrow::Cow;
use std::format;

/// Address of the `transfer` precompile.
pub const TRANSFER_ADDRESS: Address = u64_to_address(0xff - 2);

/// Gas cost of the `transfer` precompile.
pub const TRANSFER_GAS_COST: u64 = 9_000;

/// Creates the Celo transfer [`DynPrecompile`] for the given spec.
pub(super) fn make_transfer_precompile(spec_id: OpSpecId) -> DynPrecompile {
    const fn coerce<F: Fn(PrecompileInput<'_>) -> PrecompileResult + Send + Sync + 'static>(
        f: F,
    ) -> F {
        f
    }
    DynPrecompile::from(coerce(move |input| transfer_precompile(spec_id, input))).stateful()
}

/// Transfer precompile implementation for use as a [`DynPrecompile`].
///
/// Balance changes go through `PrecompileInput::internals`; `spec_id` is fixed when the
/// precompile is built.
fn transfer_precompile(spec_id: OpSpecId, mut input: PrecompileInput<'_>) -> PrecompileResult {
    if input.is_static {
        return Ok(PrecompileOutput::halt(
            PrecompileHalt::Other(Cow::Borrowed(
                "transfer precompile cannot be called in static context",
            )),
            0,
        ));
    }

    if input.gas < TRANSFER_GAS_COST {
        return Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, 0));
    }

    let chain_id = input.internals.chain_id();
    if input.caller != constants::get_addresses(chain_id).celo_token {
        return Ok(PrecompileOutput::halt(
            PrecompileHalt::Other(Cow::Borrowed("invalid caller for transfer precompile")),
            0,
        ));
    }

    if input.data.len() != 96 {
        return Ok(PrecompileOutput::halt(
            PrecompileHalt::Other(Cow::Borrowed("invalid input length")),
            0,
        ));
    }

    let from = Address::from_slice(&input.data[12..32]);
    let to = Address::from_slice(&input.data[44..64]);
    let value = U256::from_be_slice(&input.data[64..96]);

    // PrecompilesMap's pinned interface turns fatal errors into EVMError::Custom.
    // Preserve the DB cause here and abort the attempt, rather than returning a Halt.
    let fatal_db_error = |error| PrecompileError::FatalAny(AnyError::new(error));

    // Before Jovian, the Celo transfer precompile does not warm either address, so we need to
    // check if they were cold initially to match original Celo implementation behavior, and
    // make them cold again after the transfer. Starting with Jovian, this quirk is removed.
    let revert_cold_status = !spec_id.is_enabled_in(OpSpecId::JOVIAN);
    let revert_from_cold = revert_cold_status
        && input
            .internals
            .load_account(from)
            .map_err(fatal_db_error)?
            .is_cold;
    let revert_to_cold = revert_cold_status
        && input
            .internals
            .load_account(to)
            .map_err(fatal_db_error)?
            .is_cold;

    let result = input
        .internals
        .transfer(from, to, value)
        .map_err(fatal_db_error)?;

    // Both accounts are already in the pinned journal. Restoration changes
    // warmth without another DB read; keep `?` for the fallible internals API.
    if revert_from_cold {
        input
            .internals
            .load_account_mut(from)
            .map_err(fatal_db_error)?
            .data
            .unsafe_mark_cold();
    }
    if revert_to_cold {
        input
            .internals
            .load_account_mut(to)
            .map_err(fatal_db_error)?
            .data
            .unsafe_mark_cold();
    }

    Ok(result.map_or_else(
        || PrecompileOutput::new(TRANSFER_GAS_COST, Bytes::new(), 0),
        |transfer_err| {
            PrecompileOutput::halt(
                PrecompileHalt::Other(Cow::Owned(format!(
                    "transfer error occurred: {transfer_err:?}"
                ))),
                0,
            )
        },
    ))
}
