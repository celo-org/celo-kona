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
//!
//! # Committing vs. read-only calls
//! [`call`] uses the "keep by default" approach above: it runs the target as a system
//! sub-transaction that ends in `commit_tx`, so its state changes stay in the journal for
//! the enclosing transaction (used by the CIP-64 fee debit/credit).
//!
//! [`call_read_only`] instead brackets the *non-committing* `call_no_commit` with a journal
//! `checkpoint` / `checkpoint_revert`, so every state change, warmed account/slot,
//! transient-storage write, and log the target produced is rolled back. Because
//! `commit_tx` does a bare `journal.clear()`, running a read-only target through the
//! committing [`call`] would leave `checkpoint_revert` with nothing to undo — the target's
//! writes would silently persist. The non-committing path is what makes the revert
//! effective.

use crate::{
    CeloContext, constants::get_addresses, evm::CeloEvm, fee_currency_context::FeeCurrencyInfo,
};
use alloy_primitives::{
    Address, Bytes, U256, hex,
    map::{DefaultHashBuilder, HashMap},
};
use alloy_sol_types::{SolCall, SolType, sol, sol_data};
use op_revm::OpHaltReason;
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
use tracing::{debug, warn};

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

/// `debug_assert` that the journal's call-stack depth is back to `depth_before` — the depth
/// captured immediately before opening a non-committing checkpoint bracket ([`call_read_only`]
/// here, and the CIP-64 rollbackable debit in the handler).
///
/// The explicit `checkpoint` (+1) that opens such a bracket is paired with exactly one
/// `checkpoint_commit` / `checkpoint_revert` (-1), so depth ends balanced however the bracketed
/// call ends — the happy path and a system-call error alike. The balance comes from our own
/// checkpoint bookkeeping, not from `discard_tx`: op-revm's `catch_error` override does no
/// journal work for these non-deposit system txs (optimism@3bccc60 op-revm/src/handler.rs).
/// This matters because the read-only callers (`get_currencies` / `get_exchange_rate` /
/// `get_intrinsic_gas`) swallow the error and drop the currency rather than aborting the tx, so
/// a leaked depth would be silent. The one path this guards is a *fatal* error inside the call
/// that unwinds past a frame-level checkpoint without reverting it (e.g. a DB error), which
/// would leave depth inflated; the assert then trips tests loudly instead of being masked.
pub(crate) fn debug_assert_call_depth_unchanged<DB, INSP, P>(
    evm: &mut CeloEvm<DB, INSP, P>,
    depth_before: usize,
) where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
{
    debug_assert_eq!(
        evm.ctx().journal_ref().depth,
        depth_before,
        "the checkpoint bracket should have restored the call-stack depth"
    );
}

/// Call a core contract function in read-only mode, discarding **all** state changes.
///
/// Brackets the non-committing `call_no_commit` with a journal `checkpoint` /
/// `checkpoint_revert`, so any storage writes, account/slot warming, transient-storage
/// writes, logs, and self-destructs the target performs are rolled back before this function
/// returns. See the module docs for why the committing [`call`] path cannot provide this
/// guarantee.
///
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
    // Snapshot the call-stack depth so we can assert the checkpoint bracket below balances it;
    // see [`debug_assert_call_depth_unchanged`] for the full rationale.
    let prev_depth = evm.ctx().journal_ref().depth;

    // Snapshot the journal: `checkpoint` records the current revert-log length so
    // `checkpoint_revert` below can replay and undo everything appended after it.
    let checkpoint = evm.ctx().journal_mut().checkpoint();

    // Run the target through the shared non-committing path. Unlike the committing [`call`]
    // (which ends in `commit_tx`, whose `journal.clear()` empties the revert log and would make
    // the `checkpoint_revert` below a no-op), [`call_no_commit`] leaves the revert log intact —
    // and also restores `ctx.tx` and decodes the result. It leaves its journal entries for the
    // enclosing checkpoint to own, which is exactly what we revert next.
    let result = call_no_commit(evm, address, calldata, gas_limit);

    // Undo the call: reverts state, account/slot warmth, transient storage, logs, and
    // self-destructs back to the checkpoint. Warmth is reverted here, so the surrounding
    // transaction's warm/cold gas accounting is unaffected (no `transaction_id` dance
    // needed — the non-committing path never bumped it). `checkpoint_revert` touches only
    // journal state, not the already-decoded `result` or the restored tx env.
    evm.ctx().journal_mut().checkpoint_revert(checkpoint);

    // A *fatal* (non-revert) error inside the call can leave `depth` inflated (the window
    // `debug_assert_call_depth_unchanged` documents). The read-only callers swallow the error
    // and continue, so force depth back to the snapshot on the error arm; the happy path is
    // already balanced, so the assert still guards it.
    if result.is_err() {
        evm.ctx().journal_mut().depth = prev_depth;
    }
    debug_assert_call_depth_unchanged(evm, prev_depth);

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
    call_inner(evm, address, calldata, gas_limit, true)
}

/// Shared body of [`call`] (`commit == true`) and [`call_no_commit`] (`commit == false`).
///
/// Runs the target as a system sub-transaction and restores the surrounding tx env either
/// way. `commit` selects both the entry point (committing vs. non-committing system call) and
/// the teardown: the committing path additionally clears transient storage and restores
/// `transaction_id` (matching upstream `commit_tx`'s per-tx cleanup), while the non-committing
/// path leaves both for the enclosing checkpoint to own. Keeping the two paths in one function
/// removes a divergence point in consensus-relevant code.
fn call_inner<DB, INSP, P>(
    evm: &mut CeloEvm<DB, INSP, P>,
    address: Address,
    calldata: Bytes,
    gas_limit: Option<u64>,
    commit: bool,
) -> Result<(Bytes, Vec<Log>, u64, u64), CoreContractError>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
{
    // Preserve the tx to restore afterwards (the system call overwrites it) and the
    // transaction_id the committing teardown restores below.
    let prev_tx = evm.ctx().tx().clone();
    let prev_transaction_id = evm.ctx().journal_ref().transaction_id;

    let call_result = match (commit, gas_limit) {
        (true, Some(limit)) => evm.transact_system_call_with_gas_limit(address, calldata, limit),
        (true, None) => evm.system_call_one(address, calldata),
        (false, Some(limit)) => {
            evm.transact_system_call_no_commit_with_gas_limit(address, calldata, limit)
        }
        (false, None) => evm.system_call_one_no_commit(address, calldata),
    };

    // Restore the original transaction context.
    evm.ctx().set_tx(prev_tx);

    if commit {
        // Committing teardown, matching upstream `commit_tx`'s per-tx cleanup.
        // Clear transient storage (EIP-1153).
        evm.ctx().journal_mut().transient_storage.clear();
        // Restore transaction_id for correct warm/cold accounting.
        evm.ctx().journal_mut().transaction_id = prev_transaction_id;
    }

    process_call_result(call_result)
}

/// Like [`call`], but runs the target through the **non-committing** system-call path so the
/// call's journal entries survive for an enclosing `checkpoint` to commit or revert.
///
/// The committing [`call`] ends in `commit_tx`, whose `journal.clear()` empties the revert
/// log — making the call's state changes irreversible for the surrounding transaction. This
/// variant skips that, leaving the call's writes, warmed accounts/slots, transient storage,
/// and logs in the journal so a surrounding `checkpoint` / `checkpoint_revert` can undo them.
///
/// It restores the surrounding transaction's tx env (the system call overwrote it) but,
/// unlike [`call`], does **not** clear transient storage or restore `transaction_id`:
/// - `transaction_id` is never bumped without `commit_tx`, so the call's warmed slots stay
///   warm for the caller exactly as [`call`]'s `transaction_id` restore keeps them.
/// - transient storage is left in place for the enclosing checkpoint to revert on the reject
///   path (or for the caller to clear on the commit path — see
///   `CeloHandler::cip64_rollbackable_debit_and_deduct_caller`).
///
/// Used by the rollbackable CIP-64 fee debit.
pub(crate) fn call_no_commit<DB, INSP, P>(
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
    call_inner(evm, address, calldata, gas_limit, false)
}

/// Decode a finished system-call result into (output, logs, gas_used, gas_refunded),
/// mapping halts/reverts/errors to [`CoreContractError`]. Shared by [`call`] and
/// [`call_read_only`].
fn process_call_result<E: core::fmt::Display>(
    call_result: Result<ExecutionResult<OpHaltReason>, E>,
) -> Result<(Bytes, Vec<Log>, u64, u64), CoreContractError> {
    let exec_result = match call_result {
        Err(e) => return Err(CoreContractError::Evm(e.to_string())),
        Ok(o) => o,
    };

    // Check success
    match exec_result {
        ExecutionResult::Success {
            output: Output::Call(bytes),
            gas,
            logs,
            ..
        } => Ok((bytes, logs, gas.tx_gas_used(), gas.inner_refunded())),
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
        // Fetch exchange rate. A registered currency whose config cannot be read is
        // dropped from the context; without this warning that drop is silent and any
        // CIP-64 tx paying in this currency is excluded from blocks with no trace
        // (it fails with "fee currency not registered" before debit/credit, so the
        // blocklist path never logs it).
        let exchange_rate = match get_exchange_rate(evm, fee_currency_directory, token) {
            Some(rate) => rate,
            None => {
                warn!(
                    target: "celo_core_contracts",
                    "registered fee currency 0x{token:x} dropped from the fee-currency context: \
                     exchange rate read failed (CIP-64 txs in this currency will be excluded from blocks)"
                );
                continue;
            }
        };

        // Fetch intrinsic gas (same drop semantics as the exchange rate above).
        let intrinsic_gas = match get_intrinsic_gas(evm, fee_currency_directory, token) {
            Some(gas) => gas,
            None => {
                warn!(
                    target: "celo_core_contracts",
                    "registered fee currency 0x{token:x} dropped from the fee-currency context: \
                     intrinsic gas read failed (CIP-64 txs in this currency will be excluded from blocks)"
                );
                continue;
            }
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
        Context, ExecuteEvm,
        database::InMemoryDB,
        primitives::{Address, Bytes, U256},
        state::{AccountInfo, Bytecode, EvmState},
    };

    pub(crate) use crate::test_utils::{
        TEST_FEE_CURRENCY, make_celo_test_db, make_celo_test_db_with_fee_currency,
    };

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

    #[test]
    fn test_get_currency_info_drops_currency_with_unreadable_config() {
        // A currency registered in the directory (returned by getCurrencies) but whose
        // config cannot be read — here, no oracle configured, so getExchangeRate reverts —
        // must be dropped from the context rather than surface partial data. Without the
        // drop, a CIP-64 tx in this currency would be silently excluded from blocks. The
        // fully-configured currency from make_celo_test_db must still load.
        let mut db = make_celo_test_db();
        let dir = get_addresses(0).fee_currency_directory;
        let unconfigured = address!("0x2222222222222222222222222222222222222222");

        // Append `unconfigured` to the directory's currencies array (slot 2), leaving its
        // currencyConfig mapping (slot 1) unset so getExchangeRate reverts for it.
        let currencies_slot = U256::from(2);
        db.insert_account_storage(dir, currencies_slot, U256::from(2))
            .unwrap(); // length 1 -> 2
        let slot_bytes: [u8; 32] = currencies_slot.to_be_bytes();
        let data_start = U256::from_be_bytes(keccak256(slot_bytes).0);
        db.insert_account_storage(
            dir,
            data_start + U256::from(1),
            unconfigured.into_word().into(),
        )
        .unwrap();

        let ctx = Context::celo().with_db(db);
        let mut evm = ctx.build_celo();
        let currency_info = get_currency_info(&mut evm);

        assert!(
            currency_info.contains_key(&TEST_FEE_CURRENCY),
            "fully-configured currency must load"
        );
        assert!(
            !currency_info.contains_key(&unconfigured),
            "currency with unreadable config must be dropped from the context"
        );
        assert_eq!(currency_info.len(), 1, "exactly one currency should remain");
    }

    /// Runtime bytecode of a stub contract that, on every call, does
    /// `slot0 = slot0 + 1` (an `SSTORE`) and returns the new value as a 32-byte word:
    ///
    ///   PUSH1 0x00; SLOAD; PUSH1 0x01; ADD; DUP1; PUSH1 0x00; SSTORE;
    ///   PUSH1 0x00; MSTORE; PUSH1 0x20; PUSH1 0x00; RETURN
    ///
    /// Used to prove `call_read_only` rolls back persistent writes (and the warmth
    /// they cause), while the committing `call` keeps them.
    const SSTORE_STUB_BYTECODE: &[u8] = &hex!("6000546001018060005560005260206000f3");

    /// Address the SSTORE stub is deployed at in the tests below.
    const SSTORE_STUB_ADDR: Address = address!("0x00000000000000000000000000000000000000aa");

    fn make_sstore_stub_db() -> InMemoryDB {
        let bytecode = Bytecode::new_raw(SSTORE_STUB_BYTECODE.into());
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SSTORE_STUB_ADDR,
            AccountInfo {
                balance: U256::ZERO,
                nonce: 0,
                code_hash: bytecode.hash_slow(),
                account_id: None,
                code: Some(bytecode),
            },
        );
        db
    }

    /// A `call_read_only` whose target performs an `SSTORE` must leave no persistent
    /// state change: the write, and the slot/account warming it caused, are both rolled
    /// back. Running the stub twice must yield identical output *and* identical gas —
    /// the second call sees pristine (slot0 == 0, cold) state. If the revert were a
    /// no-op (the latent bug this fixes), the second call would read slot0 == 1 and
    /// return 2, and its gas would differ (warm SLOAD, dirty SSTORE).
    #[test]
    fn test_call_read_only_rolls_back_state_and_warmth() {
        let ctx = Context::celo().with_db(make_sstore_stub_db());
        let mut evm = ctx.build_celo();

        let (out1, _, gas1, _) =
            call_read_only(&mut evm, SSTORE_STUB_ADDR, Bytes::new(), None).expect("first call ok");
        assert_eq!(
            U256::from_be_slice(out1.as_ref()),
            U256::from(1),
            "stub returns slot0 + 1 == 1 on a pristine slot"
        );

        let (out2, _, gas2, _) =
            call_read_only(&mut evm, SSTORE_STUB_ADDR, Bytes::new(), None).expect("second call ok");
        assert_eq!(
            U256::from_be_slice(out2.as_ref()),
            U256::from(1),
            "call_read_only must roll back the target's SSTORE (else this would be 2)"
        );
        assert_eq!(
            gas1, gas2,
            "state (SSTORE) and warmth (SLOAD) are both reverted, so gas is identical"
        );
    }

    /// Differential check: the committing `call` *keeps* the target's `SSTORE`, while
    /// `call_read_only` discards it. After a committing `call` persists slot0 == 1,
    /// repeated `call_read_only`s all observe slot0 == 1 (returning 2) without ever
    /// advancing it further — proving read-only reverts land while committed writes stick.
    #[test]
    fn test_call_persists_state_but_read_only_does_not() {
        let ctx = Context::celo().with_db(make_sstore_stub_db());
        let mut evm = ctx.build_celo();

        // Committing call: slot0 0 -> 1, returns 1, and the write persists.
        let (out, _, _, _) =
            call(&mut evm, SSTORE_STUB_ADDR, Bytes::new(), None).expect("committing call ok");
        assert_eq!(U256::from_be_slice(out.as_ref()), U256::from(1));

        // Read-only calls now see the persisted slot0 == 1 -> return 2, and each rolls
        // back its own increment, so slot0 never moves past 1.
        for _ in 0..2 {
            let (out, _, _, _) = call_read_only(&mut evm, SSTORE_STUB_ADDR, Bytes::new(), None)
                .expect("read-only call ok");
            assert_eq!(
                U256::from_be_slice(out.as_ref()),
                U256::from(2),
                "read-only call reads persisted slot0 == 1 and must not advance it"
            );
        }
    }

    /// Runtime bytecode of a stub that emits an empty `LOG0` and returns 42, without
    /// writing storage: PUSH1 0x00; PUSH1 0x00; LOG0; PUSH1 0x2a; PUSH1 0x00; MSTORE;
    /// PUSH1 0x20; PUSH1 0x00; RETURN.
    const LOG_STUB_BYTECODE: &[u8] = &hex!("60006000a0602a60005260206000f3");
    const LOG_STUB_ADDR: Address = address!("0x00000000000000000000000000000000000000bb");

    /// A `call_read_only` target's logs must not leak into the enclosing transaction, and
    /// the enclosing transaction's already-emitted logs must survive the call. This guards
    /// the log-isolation fix: `post_execution::output` `take_logs()` the whole buffer, so
    /// without the save/restore the surrounding tx's logs would be silently dropped.
    #[test]
    fn test_call_read_only_isolates_logs() {
        let bytecode = Bytecode::new_raw(LOG_STUB_BYTECODE.into());
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            LOG_STUB_ADDR,
            AccountInfo {
                balance: U256::ZERO,
                nonce: 0,
                code_hash: bytecode.hash_slow(),
                account_id: None,
                code: Some(bytecode),
            },
        );
        let ctx = Context::celo().with_db(db);
        let mut evm = ctx.build_celo();

        // Stand in for logs the surrounding transaction has already emitted.
        let sentinel_addr = address!("0x00000000000000000000000000000000000000cc");
        evm.ctx().journal_mut().logs.push(Log::new_unchecked(
            sentinel_addr,
            Vec::new(),
            Bytes::new(),
        ));

        let (_out, call_logs, _, _) =
            call_read_only(&mut evm, LOG_STUB_ADDR, Bytes::new(), None).expect("call ok");

        // The read-only target's own log is returned to the caller...
        assert_eq!(call_logs.len(), 1, "the target emitted one LOG0");
        assert_eq!(call_logs[0].address, LOG_STUB_ADDR);

        // ...but it is rolled back out of the shared journal, and the surrounding
        // transaction's sentinel log is preserved (not stolen by the call's take_logs).
        let journal_logs_len = evm.ctx().journal_ref().logs.len();
        let first_log_addr = evm.ctx().journal_ref().logs[0].address;
        assert_eq!(
            journal_logs_len, 1,
            "surrounding logs preserved and the read-only target's log reverted"
        );
        assert_eq!(first_log_addr, sentinel_addr);
    }

    /// The committing `call` and non-committing `call_no_commit` system-call paths must produce
    /// the same execution outcome and the same persistent state change — they differ only in
    /// whether the journal's revert log survives (reversibility), not in what the call does.
    /// This pins `run_system_call_no_commit`'s hand-copied teardown against upstream
    /// `run_system_call`: if a future revm bump changed one path's teardown or gas accounting,
    /// the two would diverge here.
    #[test]
    fn call_and_call_no_commit_agree_on_outcome_and_state() {
        // Committing path.
        let mut evm_c = Context::celo().with_db(make_sstore_stub_db()).build_celo();
        let (out_c, _logs_c, gas_c, refund_c) =
            call(&mut evm_c, SSTORE_STUB_ADDR, Bytes::new(), None).expect("committing call ok");
        let state_c = evm_c.finalize();

        // Non-committing path, fresh EVM over an identical DB.
        let mut evm_n = Context::celo().with_db(make_sstore_stub_db()).build_celo();
        let (out_n, _logs_n, gas_n, refund_n) =
            call_no_commit(&mut evm_n, SSTORE_STUB_ADDR, Bytes::new(), None)
                .expect("no-commit call ok");
        let state_n = evm_n.finalize();

        // Same execution outcome.
        assert_eq!(
            out_c, out_n,
            "committing and non-committing paths returned different output"
        );
        assert_eq!(
            (gas_c, refund_c),
            (gas_n, refund_n),
            "committing and non-committing paths disagree on gas accounting"
        );

        // Same persistent state change: the stub's SSTORE (slot 0 -> 1) lands on both paths.
        let slot0 = |state: &EvmState| {
            state
                .get(&SSTORE_STUB_ADDR)
                .and_then(|a| a.storage.get(&U256::ZERO))
                .map(|s| s.present_value)
        };
        assert_eq!(
            slot0(&state_c),
            Some(U256::from(1)),
            "committing path lost the SSTORE"
        );
        assert_eq!(
            slot0(&state_c),
            slot0(&state_n),
            "committing and non-committing paths disagree on the persisted state"
        );
    }

    /// A [`Database`](revm::Database) that returns an error on **every** storage read for one
    /// target address, delegating everything else to an inner [`InMemoryDB`]. Stands in for a
    /// backing-store read failure inside a bracketed non-committing call — e.g. an `SLOAD` in a
    /// nested `debitGasFees` frame.
    struct StorageFailingDb {
        inner: InMemoryDB,
        fail_addr: Address,
    }

    /// Error type for [`StorageFailingDb`]. `InMemoryDB`'s own error is `Infallible`, which
    /// cannot be constructed, so we need a real error type to inject.
    #[derive(Debug)]
    struct InjectedDbError;

    impl core::fmt::Display for InjectedDbError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("injected storage-read failure")
        }
    }
    impl core::error::Error for InjectedDbError {}
    impl revm::database_interface::DBErrorMarker for InjectedDbError {}

    impl revm::Database for StorageFailingDb {
        type Error = InjectedDbError;

        // The inner error is `Infallible`, so `unwrap` on the delegated calls never panics.
        fn basic(
            &mut self,
            address: Address,
        ) -> Result<Option<revm::state::AccountInfo>, Self::Error> {
            Ok(self.inner.basic(address).unwrap())
        }

        fn code_by_hash(
            &mut self,
            code_hash: alloy_primitives::B256,
        ) -> Result<Bytecode, Self::Error> {
            Ok(self.inner.code_by_hash(code_hash).unwrap())
        }

        fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
            if address == self.fail_addr {
                return Err(InjectedDbError);
            }
            Ok(self.inner.storage(address, index).unwrap())
        }

        fn block_hash(&mut self, number: u64) -> Result<alloy_primitives::B256, Self::Error> {
            Ok(self.inner.block_hash(number).unwrap())
        }
    }

    /// Rollback correctness leans on op-revm's `catch_error` doing **no** journal work for these
    /// non-deposit system txs (optimism@3bccc60): on a fatal error inside a bracketed
    /// non-committing call, the journal must be left intact so `checkpoint_revert` undoes exactly
    /// the call — and nothing recorded before the bracket. If a future revm bump aligned
    /// `catch_error` with the mainnet default (`journal.discard_tx()`), the enclosing tx's
    /// journal would be wiped mid-tx and pre-bracket state would silently vanish. On the
    /// error-swallowing `call_read_only` path that is a state divergence, not a crash, so it is
    /// pinned only by comments today.
    ///
    /// This pins it: a committing `call` persists slot0, then a bracketed `call_read_only` whose
    /// target's `SLOAD` hits an injected DB error must return `Err` **and** leave the persisted
    /// slot0 untouched.
    #[test]
    fn fatal_error_in_bracketed_call_preserves_pre_bracket_journal() {
        // A second copy of the SSTORE stub, deployed at an address whose storage reads fail.
        const FAIL_STUB_ADDR: Address = address!("0x00000000000000000000000000000000000000ff");

        let mut inner = make_sstore_stub_db();
        let bytecode = Bytecode::new_raw(SSTORE_STUB_BYTECODE.into());
        inner.insert_account_info(
            FAIL_STUB_ADDR,
            AccountInfo {
                balance: U256::ZERO,
                nonce: 0,
                code_hash: bytecode.hash_slow(),
                account_id: None,
                code: Some(bytecode),
            },
        );

        let db = StorageFailingDb {
            inner,
            fail_addr: FAIL_STUB_ADDR,
        };
        let mut evm = Context::celo().with_db(db).build_celo();

        // Pre-bracket: a committing `call` persists slot0 == 1 in the journal — a real
        // pre-bracket entry that `discard_tx` would wipe but `checkpoint_revert` must not.
        let (out, _, _, _) =
            call(&mut evm, SSTORE_STUB_ADDR, Bytes::new(), None).expect("committing call ok");
        assert_eq!(U256::from_be_slice(out.as_ref()), U256::from(1));

        // Bracketed non-committing call whose target's first op is `SLOAD`, which the injected DB
        // fails — a fatal (non-revert) error inside the checkpoint bracket.
        let result = call_read_only(&mut evm, FAIL_STUB_ADDR, Bytes::new(), None);
        let err =
            result.expect_err("a fatal DB error inside the bracketed call must surface as Err");
        // Guard against a vacuous pass: the Err must be the injected storage failure (i.e. the
        // stub's `SLOAD` really hit the failing DB), not an unrelated revert/halt.
        assert!(
            err.to_string().contains("injected storage-read failure"),
            "expected the injected DB error to surface, got: {err}"
        );

        // The pre-bracket slot0 must survive: `checkpoint_revert` only unwinds entries recorded
        // after the checkpoint, and `catch_error` did no journal work. Had the journal been wiped
        // on error, slot0 would be gone here.
        let slot0 = evm
            .ctx()
            .journal_ref()
            .state
            .get(&SSTORE_STUB_ADDR)
            .and_then(|a| a.storage.get(&U256::ZERO))
            .map(|s| s.present_value);
        assert_eq!(
            slot0,
            Some(U256::from(1)),
            "a failed bracketed call wiped the pre-bracket committed state — catch_error must \
             not perform journal work (e.g. discard_tx)"
        );
    }
}
