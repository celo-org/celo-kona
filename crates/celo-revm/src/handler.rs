//!Handler related to Celo chain

use crate::{
    CeloContext,
    constants::{
        CELO_SYSTEM_ADDRESS, FEE_CREDIT_ERROR_PREFIX, FEE_DEBIT_ERROR_PREFIX, get_addresses,
    },
    contracts::{core_contracts::debug_assert_call_depth_unchanged, erc20},
    evm::CeloEvm,
    fee_currency_context::{FeeCurrencyContext, non_native_fee_currency},
    transaction::{CeloTxTr, Cip64Info},
    units::{Fc, FcU256, NativeU256},
};
use alloy_primitives::{Address, B256, b256, keccak256};
use celo_alloy_consensus::CeloTxType;
use op_revm::{
    L1BlockInfo, OpHaltReason, OpSpecId,
    constants::{L1_FEE_RECIPIENT, OPERATOR_FEE_RECIPIENT},
    handler::IsTxError,
    transaction::{OpTransactionError, OpTxTr, deposit::DEPOSIT_TRANSACTION_TYPE},
};
use revm::{
    Database, Inspector,
    context::{LocalContextTr, journal::JournalInner, result::InvalidTransaction},
    context_interface::{
        Block, Cfg, ContextTr, JournalTr, Transaction,
        journaled_state::account::JournaledAccountTr,
        result::{ExecutionResult, FromStringError},
    },
    handler::{
        EvmTr, FrameResult, Handler, PrecompileProvider, SYSTEM_ADDRESS, evm::FrameTr,
        handler::EvmTrError, pre_execution::validate_account_nonce_and_code,
        validation::validate_priority_fee_tx,
    },
    inspector::InspectorHandler,
    interpreter::{
        InitialAndFloorGas, InterpreterResult, gas::calculate_initial_tx_gas_for_tx,
        interpreter::EthInterpreter,
    },
    primitives::{U256, hardfork::SpecId},
};
use std::{boxed::Box, format, string::ToString, vec::Vec};
use tracing::{info, warn};

/// Transactions with wrong chain IDs that were accepted historically due to a bug in
/// op-geth's EIP-2930 sender recovery (tx.ChainId() instead of the network's chain ID).
/// They must be accepted during historical sync to avoid a hard fork.
/// See <https://github.com/celo-org/op-geth/issues/454>.
///
/// Each entry: `(tx_hash, network_chain_id, block_number)`.
const LEGACY_CHAIN_ID_EXCEPTIONS: [(B256, u64, u64); 2] = [
    // Celo Sepolia block 12531083 - tx had chain_id 11162320 instead of 11142220
    (
        b256!("4564b9903cfe18814ffc2696e1ad141d9cc3a549dc4f5726e15f7be2e0ccaa25"),
        11142220, // Celo Sepolia chain ID
        12531083,
    ),
    // Celo Mainnet block 53619115 - tx had chain_id 44787 instead of 42220
    (
        b256!("d6bdf3261df7e7a4db6bbc486bf091eb62dfd2883e335c31219b6a37d3febca1"),
        42220, // Celo Mainnet chain ID
        53619115,
    ),
];

fn is_legacy_chain_id_exception(
    network_chain_id: u64,
    block_number: u64,
    enveloped_tx: Option<&[u8]>,
) -> bool {
    // Filter on (network, block_number) before hashing. Live mempool traffic
    // can never reach the keccak path: the attacker can't choose `block.number`,
    // so any tx outside the two pinned historical blocks short-circuits.
    let Some((expected_hash, _, _)) = LEGACY_CHAIN_ID_EXCEPTIONS
        .iter()
        .find(|(_, net, block)| *net == network_chain_id && *block == block_number)
    else {
        return false;
    };
    enveloped_tx.is_some_and(|tx| keccak256(tx) == *expected_hash)
}

pub struct CeloHandler<EVM, ERROR, FRAME> {
    pub op: op_revm::handler::OpHandler<EVM, ERROR, FRAME>,
    /// When set, [`Self::execution_result`] skips `commit_tx` so the transaction's journal
    /// entries survive for an enclosing `checkpoint` / `checkpoint_revert` to commit or revert.
    /// Set only by [`Self::run_system_call_no_commit`]; every other entry point commits.
    no_commit: bool,
}

// The wrapped revm handlers are not `Debug`, so the fields are elided.
impl<EVM, ERROR, FRAME> core::fmt::Debug for CeloHandler<EVM, ERROR, FRAME> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CeloHandler").finish_non_exhaustive()
    }
}

impl<EVM, ERROR, FRAME> CeloHandler<EVM, ERROR, FRAME> {
    pub fn new() -> Self {
        Self {
            op: op_revm::handler::OpHandler::new(),
            no_commit: false,
        }
    }
}

impl<EVM, ERROR, FRAME> Default for CeloHandler<EVM, ERROR, FRAME> {
    fn default() -> Self {
        Self::new()
    }
}

impl<ERROR, DB, INSP, P>
    CeloHandler<CeloEvm<DB, INSP, P>, ERROR, revm::handler::EthFrame<EthInterpreter>>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
    ERROR:
        EvmTrError<CeloEvm<DB, INSP, P>> + From<OpTransactionError> + FromStringError + IsTxError,
{
    fn load_fee_currency_context(&self, evm: &mut CeloEvm<DB, INSP, P>) -> Result<(), ERROR> {
        let current_block = evm.ctx().block().number();
        if evm.fee_currency_context.updated_at_block != Some(current_block) {
            // Update the chain with the new fee currency context.
            // If core contracts are missing, we'll get an empty context (for non-celo test environments).
            let fee_currency_context = FeeCurrencyContext::new_from_evm(evm);
            evm.fee_currency_context = fee_currency_context;

            // Context loading is internal: it reads on-chain fee-currency config and is not
            // an EVM operation the transaction itself performs. It must therefore leave the
            // transaction's warm/cold accounting untouched, or the transaction pays less gas
            // than its own accesses should cost.
            //
            // But loading reads that config by calling into the FeeCurrencyDirectory and the
            // currency oracles, which leaves the accounts and storage slots it touches warm.
            // revm keys warmth off the journal `transaction_id`, and those calls run at the
            // transaction's own id, so the accesses stay warm for the transaction. Its first
            // SLOAD of such a slot would then cost 100 gas instead of the correct 2100.
            //
            // Advancing the transaction id resets that: everything loaded above now reads
            // cold. The id only ever increases (`commit_tx` bumps it per transaction), so it
            // is never reused and the warmth cannot leak into a later transaction either.
            // Resetting account status alone would miss the storage slots, which are also
            // keyed off the transaction id.
            //
            // Note the interplay with `core_contracts::call`: it *restores* `transaction_id`
            // to the surrounding tx's value after each system call, deliberately keeping that
            // warmth for its other callers (the erc20 debit/credit). For context loading we
            // want the opposite, so we advance one past the id `call` restored to.
            evm.ctx().journal_mut().transaction_id += 1;
        }

        Ok(())
    }

    fn cip64_get_base_fee_in_erc20(
        &self,
        evm: &mut CeloEvm<DB, INSP, P>,
        fee_currency: Option<Address>,
        basefee: u64,
    ) -> Result<Fc, ERROR> {
        let fee_currency_context = &evm.fee_currency_context;
        let base_fee_in_erc20 = fee_currency_context
            .celo_to_currency(fee_currency, NativeU256::new(U256::from(basefee)))
            .map_err(|e| InvalidTransaction::from(e.to_string()))?;
        // Narrow FcU256 → Fc. Downstream u128 gas-price multiplications saturate,
        // so a silent truncation here would mask only catastrophic on-chain rates —
        // we want a hard error instead.
        let v: u128 = base_fee_in_erc20
            .into_inner()
            .try_into()
            .map_err(|_| InvalidTransaction::from("base fee in ERC20 overflows u128"))?;
        Ok(Fc::new(v))
    }

    fn cip64_max_allowed_gas_cost(
        &self,
        evm: &mut CeloEvm<DB, INSP, P>,
        fee_currency: Option<Address>,
    ) -> Result<u64, ERROR> {
        let fee_currency_context = &evm.fee_currency_context;
        let max_allowed_gas_cost = fee_currency_context
            .max_allowed_currency_intrinsic_gas_cost(fee_currency.unwrap())
            .map_err(|e| InvalidTransaction::from(e.to_string()))?;
        Ok(max_allowed_gas_cost)
    }

    // For CIP-64 transactions, we need to credit the fee currency, which does everything
    // in the same call:
    // - refund
    // - reward beneficiary
    // - reward caller
    // - pay for l1 cost
    fn cip64_credit_fee_currency(
        &self,
        evm: &mut CeloEvm<DB, INSP, P>,
        exec_result: &mut FrameResult,
    ) -> Result<(), ERROR> {
        let ctx = evm.ctx();
        let is_deposit = ctx.tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;
        let fee_currency = ctx.tx().fee_currency();
        let fees_in_celo = ctx.tx().is_fee_in_celo();
        let is_balance_check_disabled = ctx.cfg().is_balance_check_disabled();
        let is_base_fee_disabled = ctx.cfg().is_base_fee_check_disabled();

        if is_deposit || fees_in_celo || is_balance_check_disabled || is_base_fee_disabled {
            return Ok(());
        }

        // `build_execution_result` runs this hook for *every* system call too (the CIP-64
        // debit's own `call_no_commit`, `call_read_only`, ...), not just the main tx. Those
        // sub-transactions carry a system caller and no fee currency, so the `fees_in_celo`
        // arm of the early return above already excluded them — the credit never re-enters.
        // Pin that: reaching here with a system caller and a real fee currency would
        // recursively debit/credit.
        //
        // Gate the assert on EIP-3607 being enforced. RPC entry points disable EIP-3607 and
        // default an omitted `from` to the zero address (== `CELO_SYSTEM_ADDRESS`); most also
        // disable the base-fee/balance checks and so return above, but `eth_simulateV1` with
        // `validation: true` keeps those checks on and can legitimately reach this hook with a
        // system caller — it must not panic a debug node. EIP-3607 is only enforced during block
        // execution, where a system-caller CIP-64 credit really is the bug this guards.
        let caller = ctx.tx().caller();
        debug_assert!(
            ctx.cfg().is_eip3607_disabled()
                || (caller != SYSTEM_ADDRESS && caller != CELO_SYSTEM_ADDRESS),
            "CIP-64 credit hook reached for a system-caller sub-transaction (should be native)"
        );

        // Extract all values first to avoid borrowing conflicts
        let basefee = ctx.block().basefee();
        let chain_id = ctx.cfg().chain_id();
        let fee_handler = get_addresses(chain_id).fee_handler;
        let mut fee_recipient = ctx.block().beneficiary();

        // Not all fee currencies can handle a receiver being the zero address.
        // In that case send the fee to the base fee recipient, which we know is non-zero.
        if fee_recipient == Address::ZERO {
            fee_recipient = fee_handler;
        }

        let base_fee_in_erc20: Fc = self.cip64_get_base_fee_in_erc20(evm, fee_currency, basefee)?;
        let effective_gas_price = Fc::new(
            evm.ctx()
                .tx()
                .effective_gas_price(base_fee_in_erc20.into_inner()),
        );
        let tip_gas_price = effective_gas_price
            .checked_sub(base_fee_in_erc20)
            .expect("effective_gas_price >= base_fee_in_erc20 enforced by validate_env");

        // EIP-8037 reservoir TODO (inert until an Amsterdam-equivalent OpSpecId
        // activates; `gas().reservoir()` is always 0 before that): this CIP-64 credit
        // split does not handle a non-zero reservoir. The debit prepaid the full
        // `tx.gas_limit` (reservoir included), but `refund_in_erc20` below reimburses
        // only `remaining + refunded` and omits the leftover `reservoir()`, so the user
        // would be over-charged by `effective_gas_price * reservoir()`; the tip/base
        // split is also metered on `spent_sub_refunded()` (regular-budget gas), not the
        // true consumed gas. op-revm subtracts the reservoir in its native path, but only
        // after resetting `Gas` to the full tx limit, so that formula does not port onto
        // this frame's `Gas` (limit == regular budget). Rework the debit+credit accounting
        // for reservoir>0 here when Amsterdam lands, with a credit conservation test.
        let tx_fee_tip_in_erc20 = FcU256::new(U256::from(
            tip_gas_price
                .into_inner()
                .saturating_mul(exec_result.gas().spent_sub_refunded() as u128),
        ));

        // Return balance of not spent gas.
        let refund_in_erc20 =
            FcU256::new(U256::from(effective_gas_price.into_inner().saturating_mul(
                (exec_result.gas().remaining() + exec_result.gas().refunded() as u64) as u128,
            )));

        let base_tx_charge = FcU256::new(U256::from(
            base_fee_in_erc20
                .into_inner()
                .saturating_mul(exec_result.gas().spent_sub_refunded() as u128),
        ));

        // Subtract raw debit gas (before refunds) to match op-geth, which computes
        // gasUsed = maxIntrinsicGasCost - leftoverGas (no refund adjustment).
        let ctx = evm.ctx();
        let cip64_info = ctx.tx().cip64_tx_info.as_ref().unwrap();
        let debit_raw_gas = cip64_info.debit_gas_used + cip64_info.debit_gas_refunded;
        let max_allowed_gas_cost = self
            .cip64_max_allowed_gas_cost(evm, fee_currency)?
            .saturating_sub(debit_raw_gas);

        let (logs, credit_gas_used, credit_gas_refunded) = erc20::credit_gas_fees(
            evm,
            fee_currency.unwrap(),
            caller,
            fee_recipient,
            fee_handler,
            refund_in_erc20.into_inner(),
            tx_fee_tip_in_erc20.into_inner(),
            base_tx_charge.into_inner(),
            max_allowed_gas_cost,
        )
        .map_err(|e| InvalidTransaction::from(format!("{FEE_CREDIT_ERROR_PREFIX}: {e}")))?;

        // Collect logs from the system call to be included in the final receipt
        let info = evm.ctx().tx.cip64_tx_info.as_mut().unwrap();
        info.credit_gas_used = credit_gas_used;
        info.credit_gas_refunded = credit_gas_refunded;
        info.logs_post = logs;
        self.log_and_warn_gas_cost(evm, fee_currency)?;
        Ok(())
    }

    /// Logs a summary of the CIP-64 gas costs and warns if they exceed the intrinsic gas cost.
    fn log_and_warn_gas_cost(
        &self,
        evm: &mut CeloEvm<DB, INSP, P>,
        fee_currency: Option<Address>,
    ) -> Result<(), ERROR> {
        let (debit_gas_used, debit_gas_refunded, credit_gas_used, credit_gas_refunded) = {
            let ctx = evm.ctx();
            let info = ctx.tx().cip64_tx_info.as_ref().unwrap();
            (
                info.debit_gas_used,
                info.debit_gas_refunded,
                info.credit_gas_used,
                info.credit_gas_refunded,
            )
        };

        let intrinsic_gas_cost = evm
            .fee_currency_context
            .currency_intrinsic_gas_cost(fee_currency)
            .map_err(|e| InvalidTransaction::from(e.to_string()))?;

        // Log the gas summary for debugging and verification
        // gas_used + gas_refunded gives the raw gas before refunds (what op-geth calls gasUsed)
        info!(
            target: "celo_handler",
            "CIP-64 gas summary: fee_currency={:?}, \
            debit(gas_used={}, gas_refunded={}), \
            credit(gas_used={}, gas_refunded={}), \
            intrinsic_gas={}",
            fee_currency,
            debit_gas_used,
            debit_gas_refunded,
            credit_gas_used,
            credit_gas_refunded,
            intrinsic_gas_cost
        );

        // Compare raw gas (before refunds) against intrinsic gas limit
        let total_raw_gas =
            debit_gas_used + debit_gas_refunded + credit_gas_used + credit_gas_refunded;
        if total_raw_gas > intrinsic_gas_cost {
            if total_raw_gas > intrinsic_gas_cost * 2 {
                warn!(
                    target: "celo_handler",
                    "Gas usage for debit+credit exceeds intrinsic gas: {} > {}",
                    total_raw_gas,
                    intrinsic_gas_cost
                );
            } else {
                info!(
                    target: "celo_handler",
                    "Gas usage for debit+credit exceeds intrinsic gas, within a factor of 2: {} > {}",
                    total_raw_gas,
                    intrinsic_gas_cost
                );
            }
        }
        Ok(())
    }

    fn cip64_validate_erc20_and_debit_gas_fees(
        &self,
        evm: &mut CeloEvm<DB, INSP, P>,
    ) -> Result<(), ERROR> {
        let ctx = evm.ctx();
        let tx = ctx.tx();
        let fee_currency = tx.fee_currency();
        let caller_addr = tx.caller();
        let gas_limit = tx.gas_limit();
        let basefee = ctx.block().basefee();

        let fee_currency_context = &evm.fee_currency_context;

        // For CIP-64 transactions, debit the erc20 for fees before borrowing caller_account
        // Check if the fee currency is registered
        if fee_currency_context
            .currency_exchange_rate(fee_currency)
            .is_err()
        {
            return Err(InvalidTransaction::from("unregistered fee-currency address").into());
        }

        let base_fee_in_erc20: Fc = self.cip64_get_base_fee_in_erc20(evm, fee_currency, basefee)?;
        let effective_gas_price = Fc::new(
            evm.ctx()
                .tx()
                .effective_gas_price(base_fee_in_erc20.into_inner()),
        );

        // Get ERC20 balance using the erc20 module
        let fee_currency_addr = fee_currency.unwrap();

        let gas_cost = FcU256::new(
            (gas_limit as u128)
                .checked_mul(effective_gas_price.into_inner())
                .map(U256::from)
                .ok_or(InvalidTransaction::OverflowPaymentInTransaction)?,
        );

        // Note: Unlike op-reth, we do not add L1 cost or operator fee to the ERC20 debit.
        // On Celo chains, l1BaseFeeScalar, l1BlobBaseFeeScalar, and operatorFeeScalar are
        // always zero by design, so these costs are structurally zero.

        let max_allowed_gas_cost = self.cip64_max_allowed_gas_cost(evm, fee_currency)?;

        // The debit's system call records its own logs as `Cip64Info::logs_pre` (via
        // `post_execution::output`'s `take_logs`, captured below). `run_system_call_no_commit`
        // detaches and restores the enclosing transaction's logs around the call, so the debit's
        // `take_logs` sees only its own logs regardless of what the enclosing tx has emitted —
        // no ordering precondition on this call site is required.

        // For CIP-64 transactions, deduct gas from the fee currency by calling erc20::debit_gas_fees.
        // Note: load_fee_currency_context() already advanced the journal transaction_id after
        // context loading, so the accounts and storage slots it warmed now read cold here.
        let (logs, debit_gas_used, debit_gas_refunded) = erc20::debit_gas_fees(
            evm,
            fee_currency_addr,
            caller_addr,
            gas_cost.into_inner(),
            max_allowed_gas_cost,
        )
        .map_err(|e| InvalidTransaction::from(format!("{FEE_DEBIT_ERROR_PREFIX}: {e}")))?;

        // `Cip64Info` and `CeloTransaction::effective_gas_price` are serialized public
        // types; we narrow back to `u128` at this boundary rather than widening them.
        let tx = &mut evm.ctx().tx;
        tx.cip64_tx_info = Some(Cip64Info {
            debit_gas_used,
            debit_gas_refunded,
            credit_gas_used: 0,
            credit_gas_refunded: 0,
            logs_pre: logs,
            logs_post: Vec::new(),
            base_fee_in_erc20: Some(base_fee_in_erc20.into_inner()),
        });
        tx.effective_gas_price = Some(effective_gas_price.into_inner());

        Ok(())
    }

    /// Runs the CIP-64 ERC20 fee debit together with the caller-state validation and native
    /// deduction as a single **rollbackable** unit.
    ///
    /// The debit mutates the payer's ERC20 fee-currency balance, yet a CIP-64 tx can still be
    /// *rejected* by a later state check (nonce / EIP-3607 / insufficient native funds). To
    /// stop a rejected tx from leaking a committed debit into the block, the debit runs
    /// through the non-committing system-call path (its journal entries survive), bracketed
    /// by a journal `checkpoint`:
    /// - on success the checkpoint is committed and the debit's transient storage is cleared
    ///   (EIP-1153), reproducing the committing `call` path's bookkeeping;
    /// - on any rejection the checkpoint is reverted, undoing the debit — state, account/slot
    ///   warmth, transient storage, and logs — together with the aborted validation.
    ///
    /// `transaction_id` is never bumped by the non-committing path, so on the success path the
    /// debit's warmed fee-currency slots stay warm for the main transaction exactly as the
    /// committing `call`'s `transaction_id` restore keeps them: warm/cold gas accounting is
    /// unchanged. Because the checkpoint spans the debit, the affordability checks in
    /// [`Self::debit_and_deduct_caller`] read post-debit state, so a fee currency that
    /// (non-conformantly) touched the payer's native account is handled by ordinary
    /// validation rather than a dedicated guard — on the include path celo-revm keeps the
    /// change and converges with op-geth (both run the same `debitGasFees`); on the reject
    /// path it is rolled back.
    ///
    /// `debit_erc20_fees` gates whether an ERC20 debit — and thus a checkpoint — is needed;
    /// for native-CELO and deposit txs it is `false` and this is a plain validate-and-deduct.
    fn cip64_rollbackable_debit_and_deduct_caller(
        &self,
        evm: &mut CeloEvm<DB, INSP, P>,
        debit_erc20_fees: bool,
        additional_cost: U256,
    ) -> Result<(), ERROR> {
        // Native / deposit txs make no irreversible pre-validation charge, so they need no
        // rollback checkpoint — this is a plain validate-and-deduct with no bracketing.
        if !debit_erc20_fees {
            return self.debit_and_deduct_caller(evm, false, additional_cost);
        }

        // Snapshot the call-stack depth so we can assert the checkpoint bracket below balances
        // it (via `checkpoint_commit` on Ok / `checkpoint_revert` on Err); see
        // [`debug_assert_call_depth_unchanged`] for the full rationale.
        let prev_depth = evm.ctx().journal_ref().depth;

        // The ERC20 debit makes an irreversible pre-validation charge, so bracket it in a
        // checkpoint that commits it into the tx (include) or reverts it whole (reject).
        let checkpoint = evm.ctx().journal_mut().checkpoint();

        let result = self.debit_and_deduct_caller(evm, true, additional_cost);

        match &result {
            Ok(()) => {
                let journal = evm.ctx().journal_mut();
                // Keep the debit's journal entries; the enclosing transaction owns them now.
                journal.checkpoint_commit();

                // Reproduce the committing `call` path's `commit_tx` per-tx cleanup for the two
                // fields `checkpoint_commit` does not touch (it only folds state/warmth into the
                // parent and decrements depth). Destructure the whole `JournalInner` — no `..` —
                // so that a future revm field addition fails to compile *here*, forcing a
                // decision on whether the debit must clean it up, exactly as upstream `commit_tx`
                // destructures for the same reason. This site already silently missed
                // `selfdestructed_addresses` once (commit 0edc56ca).
                let JournalInner {
                    transient_storage,
                    selfdestructed_addresses,
                    // Folded into the parent by `checkpoint_commit` or owned by the enclosing tx;
                    // nothing to reset per-debit.
                    state: _,
                    logs: _,
                    depth: _,
                    journal: _,
                    transaction_id: _,
                    cfg: _,
                    warm_addresses: _,
                } = &mut journal.inner;
                // Clear the debit's transient storage (EIP-1153), matching the committing
                // `call` path. Nothing in the main tx has run yet, so transient storage is
                // empty before the debit and this clears only the debit's own writes.
                transient_storage.clear();
                // Drop any self-destructs the debit recorded, matching `commit_tx`'s
                // `selfdestructed_addresses.clear()`. Without this a fee currency whose
                // `debitGasFees` SELFDESTRUCTs a contract would leave the list populated for
                // the main tx — a spurious EIP-7708 burn log (and receipt divergence) once a
                // spec that emits them is enabled. Truncating back to the checkpoint's index
                // (empty before the debit) reproduces `commit_tx`'s clear.
                selfdestructed_addresses.truncate(checkpoint.selfdestructed_i);
            }
            Err(_) => {
                evm.ctx().journal_mut().checkpoint_revert(checkpoint);
                // Fatal (non-revert) errors can unwind past a frame checkpoint without reverting it,
                // leaving depth inflated; force it back like call_read_only's error arm.
                evm.ctx().journal_mut().depth = prev_depth;
            }
        }

        // Both arms leave depth balanced — `checkpoint_commit` on Ok, `checkpoint_revert` plus
        // the force-restore above on Err — so assert rather than force it here.
        debug_assert_call_depth_unchanged(evm, prev_depth);

        result
    }

    /// The fallible body wrapped by [`Self::cip64_rollbackable_debit_and_deduct_caller`]:
    /// runs the ERC20 debit (when `debit_erc20_fees`), validates the caller's nonce/code and
    /// native affordability **against post-debit state**, and applies the caller mutations
    /// (native gas deduction + nonce bump). Every early return here is a tx rejection that the
    /// caller rolls back via the enclosing checkpoint; on `Ok` the caller commits it.
    fn debit_and_deduct_caller(
        &self,
        evm: &mut CeloEvm<DB, INSP, P>,
        debit_erc20_fees: bool,
        additional_cost: U256,
    ) -> Result<(), ERROR> {
        // CIP-64 ERC20 fee debit (rollbackable). Runs first, so the checks below validate the
        // caller against post-debit state.
        if debit_erc20_fees {
            self.cip64_validate_erc20_and_debit_gas_fees(evm)?;
        }

        let ctx = evm.ctx();
        let basefee = ctx.block().basefee() as u128;
        let blob_price = ctx.block().blob_gasprice().unwrap_or_default();
        let is_deposit = ctx.tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;
        let fees_in_celo = ctx.tx().is_fee_in_celo();
        let is_balance_check_disabled = ctx.cfg().is_balance_check_disabled();
        let is_eip3607_disabled = ctx.cfg().is_eip3607_disabled();
        let is_nonce_check_disabled = ctx.cfg().is_nonce_check_disabled();

        let mint = if is_deposit {
            ctx.tx().mint().unwrap_or_default()
        } else {
            0
        };

        let (tx, journal) = evm.ctx().tx_journal_mut();

        let mut caller_account = journal.load_account_with_code_mut(tx.caller())?.data;

        if !is_deposit {
            // validates account nonce and code
            validate_account_nonce_and_code(
                &caller_account.account().info,
                tx.nonce(),
                is_eip3607_disabled,
                is_nonce_check_disabled,
            )?;
        }

        let max_balance_spending = tx.max_balance_spending()?.saturating_add(additional_cost);

        // If the transaction is a deposit with a `mint` value, add the mint value
        // in wei to the caller's balance. This should be persisted to the database
        // prior to the rest of execution.
        let mut new_balance = caller_account.balance().saturating_add(U256::from(mint));

        if fees_in_celo {
            // Check if account has enough balance for `gas_limit * max_fee`` and value transfer.
            // Transfer will be done inside `*_inner` functions.
            if !is_deposit && max_balance_spending > new_balance && !is_balance_check_disabled {
                // skip max balance check for deposit transactions.
                // this check for deposit was skipped previously in `validate_tx_against_state` function
                return Err(InvalidTransaction::LackOfFundForMaxFee {
                    fee: Box::new(max_balance_spending),
                    balance: Box::new(new_balance),
                }
                .into());
            }

            let effective_balance_spending =
                tx.effective_balance_spending(basefee, blob_price).expect(
                    "effective balance is always smaller than max balance so it can't overflow",
                );

            // subtracting max balance spending with value that is going to be deducted later in the call.
            let gas_balance_spending = effective_balance_spending - tx.value();

            // If the transaction is not a deposit transaction, subtract the L1 data fee from the
            // caller's balance directly after minting the requested amount of ETH.
            // Additionally deduct the operator fee from the caller's account.
            //
            // In case of deposit additional cost will be zero.
            let op_gas_balance_spending = gas_balance_spending.saturating_add(additional_cost);

            new_balance = new_balance.saturating_sub(op_gas_balance_spending);
        } else {
            // Check CELO balance for value transfer (value is always in CELO)
            assert!(
                !is_deposit,
                "gas for deposit txs can't be paid in erc20 tokens"
            );
            if tx.value() > new_balance && !is_balance_check_disabled {
                return Err(InvalidTransaction::LackOfFundForMaxFee {
                    fee: Box::new(tx.value()),
                    balance: Box::new(new_balance),
                }
                .into());
            }
        }

        if is_balance_check_disabled {
            // Make sure the caller's balance is at least the value of the transaction.
            // this is not consensus critical, and it is used in testing.
            new_balance = new_balance.max(tx.value());
        }

        // make changes to the account
        //(for cip64, the set balance won't journal if the balance is the same)
        caller_account.set_balance(new_balance);
        if tx.kind().is_call() {
            caller_account.bump_nonce();
        }

        Ok(())
    }

    fn validate_celo_initial_tx_gas(
        &self,
        evm: &mut CeloEvm<DB, INSP, P>,
    ) -> Result<InitialAndFloorGas, ERROR> {
        // Extract needed values first to avoid borrowing conflicts
        let ctx = evm.ctx();
        let fee_currency = ctx.tx().fee_currency();
        let gas_limit = ctx.tx().gas_limit();
        let spec = *ctx.cfg().spec();

        let mut gas = calculate_initial_tx_gas_for_tx(ctx.tx(), spec.into_eth_spec());

        if non_native_fee_currency(fee_currency).is_some() {
            let intrinsic_gas_for_erc20 = evm
                .fee_currency_context
                .currency_intrinsic_gas_cost(fee_currency)
                .map_err(|e| InvalidTransaction::from(e.to_string()))?;
            // Adding only in the initial gas, and not the floor because we never adapted the
            // eip7623 to the cip64 (discussions being taken)
            gas.initial_total_gas = gas
                .initial_total_gas
                .saturating_add(intrinsic_gas_for_erc20);
        }

        // Additional check to see if limit is big enough to cover initial gas.
        if gas.initial_total_gas > gas_limit {
            return Err(InvalidTransaction::CallGasCostMoreThanGasLimit {
                gas_limit,
                initial_gas: gas.initial_total_gas,
            }
            .into());
        }

        // EIP-7623: Increase calldata cost
        // floor gas should be less than gas limit.
        if spec.into_eth_spec().is_enabled_in(SpecId::PRAGUE) && gas.floor_gas > gas_limit {
            return Err(InvalidTransaction::GasFloorMoreThanGasLimit {
                gas_floor: gas.floor_gas,
                gas_limit,
            }
            .into());
        };

        Ok(gas)
    }

    /// Builds the [`ExecutionResult`] for a finished transaction **without**
    /// committing the journal.
    ///
    /// This is the body of [`Self::execution_result`] up to — but excluding — the
    /// `commit_tx()` / teardown at the end (it runs the CIP-64 credit hook and
    /// `post_execution::output`). `execution_result` calls it and then does the teardown,
    /// gating `commit_tx` on the `no_commit` flag, so both the committing path and
    /// [`Self::run_system_call_no_commit`] share this body; the latter leaves the journal's
    /// revert log intact so an enclosing `checkpoint` can roll the call back (see
    /// [`crate::contracts::core_contracts::call_read_only`]).
    ///
    /// MAINTENANCE: the `call_and_call_no_commit_agree_on_outcome_and_state` test guards that
    /// the committing and no-commit paths stay in lockstep across revm bumps.
    fn build_execution_result(
        &mut self,
        evm: &mut CeloEvm<DB, INSP, P>,
        mut frame_result: FrameResult,
        result_gas: revm::context_interface::result::ResultGas,
    ) -> Result<ExecutionResult<OpHaltReason>, ERROR> {
        // Surface any DB / custom context error recorded during execution.
        revm::context_interface::context::take_error::<ERROR, _>(evm.ctx().error())?;

        // CIP-64: Credit fee currency AFTER reward_beneficiary but BEFORE finalizing result
        // This matches the old revm 24.0 flow where it was called in the `end` function
        // as a separate step after reward_beneficiary
        self.cip64_credit_fee_currency(evm, &mut frame_result)?;

        // Call post_execution::output to get ExecutionResult
        let exec_result =
            revm::handler::post_execution::output(evm.ctx(), frame_result, result_gas)
                .map_haltreason(OpHaltReason::Base);

        // CIP64 NOTE:
        // The ExecutionResult class does not allow logs to be passed in for revert results.
        // As the cip64 debit/credit generate logs, our reverts must have those due to gas payment.
        // So, instead of modifying only the success results here (to contain those logs),
        // both cases are handled in the receipts_builder (alloy-celo-evm)

        if exec_result.is_halt() {
            // Post-regolith, if the transaction is a deposit transaction and it halts,
            // we bubble up to the global return handler. The mint value will be persisted
            // and the caller nonce will be incremented there.
            let is_deposit = evm.ctx().tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;
            if is_deposit && evm.ctx().cfg().spec().is_enabled_in(OpSpecId::REGOLITH) {
                return Err(ERROR::from(OpTransactionError::HaltedDepositPostRegolith));
            }
        }

        Ok(exec_result)
    }

    /// Runs a system call like [`Handler::run_system_call`], but **without**
    /// committing the transaction journal.
    ///
    /// [`commit_tx`](revm::context_interface::JournalTr::commit_tx) does a bare
    /// `journal.clear()` that empties the shared revert log. Skipping it leaves the
    /// call's journal entries (state changes, warmed accounts/slots, transient
    /// storage, logs) in place so a surrounding `checkpoint` / `checkpoint_revert`
    /// pair can undo them — which is what makes
    /// [`call_read_only`](crate::contracts::core_contracts::call_read_only) an actual
    /// read-only call.
    ///
    /// This is the trait-default [`Handler::run_system_call`] with two Celo tweaks: the
    /// `no_commit` flag makes [`Self::execution_result`] skip `commit_tx` (the rest of its
    /// teardown — L1-cost cache, local context, frame stack — still runs), and the enclosing
    /// transaction's logs are detached around the call. Delegating rather than hand-copying the
    /// trait body removes the per-revm-bump re-diff burden the old inlined copy carried.
    ///
    /// On its own this method *keeps* the call's effects; the caller must bracket it with
    /// `checkpoint` / `checkpoint_revert` to discard them.
    pub(crate) fn run_system_call_no_commit(
        &mut self,
        evm: &mut CeloEvm<DB, INSP, P>,
    ) -> Result<ExecutionResult<OpHaltReason>, ERROR> {
        // Skip `commit_tx` in `execution_result` so the call's journal entries survive for the
        // enclosing `checkpoint` to own. Handlers are constructed fresh per system call
        // (`run_system_tx`), so this flag never leaks into a committing call.
        self.no_commit = true;

        // Detach the enclosing transaction's logs for the duration of the call. Building the
        // result runs `post_execution::output`, which `take_logs()` (a `mem::take`) the *whole*
        // journal logs buffer into this call's `ExecutionResult`; without setting the enclosing
        // buffer aside first, it would sweep up the enclosing tx's already-emitted logs. Because
        // `checkpoint_revert` can only truncate logs — never restore ones taken before it — this
        // isolation must live here, unconditionally for every no-commit caller, rather than in
        // each caller. The call's own logs still flow into its `ExecutionResult` (which the
        // CIP-64 debit records as `logs_pre`); only the enclosing buffer is set aside and
        // restored below.
        let saved_logs = core::mem::take(&mut evm.ctx().journal_mut().logs);

        let result = self.run_system_call(evm);

        // Reset `no_commit` so it can never leak into a committing call.
        self.no_commit = false;

        // On success `post_execution::output` took the call's logs into the result, so the
        // buffer is empty; assert before restoring so a future revm change to the take-logs
        // contract fails loudly instead of silently dropping the enclosing tx's logs.
        debug_assert!(
            result.is_err() || evm.ctx().journal_ref().logs.is_empty(),
            "system call left logs in the journal; restoring would drop the enclosing tx's logs"
        );

        // Reattach the enclosing transaction's logs, discarding anything this call left in the
        // buffer (captured in the result on success; unwanted on the error path). Any enclosing
        // `checkpoint_revert` then operates on the enclosing logs alone.
        evm.ctx().journal_mut().logs = saved_logs;

        result
    }
}

impl<ERROR, DB, INSP, P> Handler
    for CeloHandler<CeloEvm<DB, INSP, P>, ERROR, revm::handler::EthFrame<EthInterpreter>>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
    ERROR:
        EvmTrError<CeloEvm<DB, INSP, P>> + From<OpTransactionError> + FromStringError + IsTxError,
{
    type Evm = CeloEvm<DB, INSP, P>;
    type Error = ERROR;
    type HaltReason = OpHaltReason;

    fn validate(&self, evm: &mut Self::Evm) -> Result<InitialAndFloorGas, Self::Error> {
        self.validate_env(evm)?;
        self.validate_celo_initial_tx_gas(evm)
    }

    fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        self.load_fee_currency_context(evm)?;
        // Do not perform any extra validation for deposit transactions, they are pre-verified on
        // L1.

        let tx_type = evm.ctx().tx().tx_type();

        match CeloTxType::try_from(tx_type).map_err(|e| InvalidTransaction::from(e.to_string()))? {
            CeloTxType::Deposit => {
                let is_system_transaction = evm.ctx().tx().is_system_transaction();
                // Do not allow for a system transaction to be processed if Regolith is enabled.
                if is_system_transaction && evm.ctx().cfg().spec().is_enabled_in(OpSpecId::REGOLITH)
                {
                    return Err(OpTransactionError::DepositSystemTxPostRegolith.into());
                }
                return Ok(());
            }
            CeloTxType::Cip64 => {
                let max_fee = evm.ctx().tx().max_fee_per_gas();
                let max_priority_fee = evm
                    .ctx()
                    .tx()
                    .max_priority_fee_per_gas()
                    .unwrap_or_default();
                let base_fee = evm.ctx().block().basefee();
                let fee_currency = evm.ctx().tx().fee_currency();

                if Some(evm.ctx().cfg().chain_id()) != evm.ctx().tx().chain_id() {
                    return Err(InvalidTransaction::InvalidChainId.into());
                }

                // Skip base fee check when disabled (e.g. during eth_estimateGas).
                // `validate_priority_fee_tx` takes raw `u128`s; max_fee and
                // max_priority_fee come from the CIP-64 envelope (FC-denominated)
                // and base_fee_for_check is the FC-converted base fee — all three
                // share FC denomination, so calling `.into_inner()` here is safe.
                let base_fee_for_check = if evm.ctx().cfg().is_base_fee_check_disabled() {
                    None
                } else {
                    Some(
                        self.cip64_get_base_fee_in_erc20(evm, fee_currency, base_fee)?
                            .into_inner(),
                    )
                };
                validate_priority_fee_tx(max_fee, max_priority_fee, base_fee_for_check, false)?;
                // CIP-64-specific validation (chain-ID + priority fee) is complete.
                // Fall through to `self.op.mainnet.validate_env(evm)` for generic
                // checks (block gas limit, EIP-7825 gas cap, EIP-3860 initcode
                // size). CIP-64 maps to `Custom` in revm, so its gas-price
                // match arm is a no-op and chain-ID is re-checked harmlessly.
            }
            _ => {
                // Ethereum's tx types will be handled in the "self.op.mainnet.validate_env(evm)" call below
                // where not only those transactions are validated, but also the block specifics.
            }
        }

        if is_legacy_chain_id_exception(
            evm.ctx().cfg().chain_id(),
            evm.ctx().block().number().saturating_to::<u64>(),
            evm.ctx().tx().enveloped_tx().map(|b| b.as_ref()),
        ) {
            // Temporarily disable chain ID check for these historical exception txs,
            // preserving the original value to restore afterward
            let original_tx_chain_id_check = evm.ctx().cfg().tx_chain_id_check;
            evm.ctx().modify_cfg(|cfg| cfg.tx_chain_id_check = false);
            let result = self.op.mainnet.validate_env(evm);
            evm.ctx()
                .modify_cfg(|cfg| cfg.tx_chain_id_check = original_tx_chain_id_check);
            return result;
        }

        self.op.mainnet.validate_env(evm)
    }

    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Self::Evm,
        _init_and_floor_gas: &mut InitialAndFloorGas,
    ) -> Result<(), Self::Error> {
        self.load_fee_currency_context(evm)?;

        let ctx = evm.ctx();

        let basefee = ctx.block().basefee() as u128;
        let is_deposit = ctx.tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;
        let fees_in_celo = ctx.tx().is_fee_in_celo();
        let spec = *ctx.cfg().spec();
        let block_number = ctx.block().number();
        let is_balance_check_disabled = ctx.cfg().is_balance_check_disabled();

        let mut additional_cost = U256::ZERO;

        // The L1-cost fee is only computed for Optimism non-deposit transactions.
        if !is_deposit && !ctx.cfg().is_fee_charge_disabled() {
            // L1 block info is stored in the context for later use.
            // and it will be reloaded from the database if it is not for the current block.
            if ctx.chain().l2_block != Some(block_number) {
                *ctx.chain_mut() = L1BlockInfo::try_fetch(ctx.db_mut(), block_number, spec)?;
            }

            // For CIP-64 txs, L1+operator costs are handled in the fee currency debit/credit,
            // so we only compute additional_cost for native CELO transactions.
            if fees_in_celo {
                // account for additional cost of l1 fee and operator fee
                let enveloped_tx = ctx
                    .tx()
                    .enveloped_tx()
                    .expect("all not deposit tx have enveloped tx")
                    .clone();

                // compute L1 cost
                additional_cost = ctx.chain_mut().calculate_tx_l1_cost(&enveloped_tx, spec);

                // compute operator fee
                if spec.is_enabled_in(OpSpecId::ISTHMUS) {
                    let gas_limit = U256::from(ctx.tx().gas_limit());
                    let operator_fee_charge =
                        ctx.chain()
                            .operator_fee_charge(&enveloped_tx, gas_limit, spec);
                    additional_cost = additional_cost.saturating_add(operator_fee_charge);
                }
            }
        }

        // For native-fee CIP-64 transactions (fee_currency is None / ZERO), store
        // a minimal Cip64Info so the receipt builder emits `base_fee: Some(basefee)`
        // rather than `None`. Without this the receipt encoding would differ from
        // the historical behavior and break receipt roots.
        if evm.ctx().tx().is_cip64() && fees_in_celo {
            evm.ctx().tx.cip64_tx_info = Some(Cip64Info {
                base_fee_in_erc20: Some(basefee),
                ..Default::default()
            });
        }

        // The CIP-64 ERC20 fee debit runs only for a non-deposit tx paying in an ERC20 fee
        // currency, and only when the base-fee / balance checks are enabled.
        //
        // NOTE: When is_base_fee_disabled is true (eth_call/eth_estimateGas), we skip the
        // ERC20 debit, which also skips setting tx.effective_gas_price to the fee-currency
        // rate. The GASPRICE opcode will therefore return native pricing instead of the
        // ERC20-denominated price during simulations. This is a known limitation.
        let is_base_fee_disabled = evm.ctx().cfg().is_base_fee_check_disabled();
        let debit_erc20_fees =
            !is_balance_check_disabled && !is_base_fee_disabled && !fees_in_celo && !is_deposit;

        // The ERC20 debit (if any) plus the caller-state validation and native gas deduction
        // run as a single rollbackable unit: the debit's journal entries are bracketed by a
        // checkpoint that is reverted if any post-debit rejection check (nonce / EIP-3607 /
        // insufficient funds) fails, so a rejected CIP-64 tx charges no fee. The affordability
        // checks therefore validate the caller against post-debit state.
        self.cip64_rollbackable_debit_and_deduct_caller(evm, debit_erc20_fees, additional_cost)
    }

    fn last_frame_result(
        &mut self,
        evm: &mut Self::Evm,
        frame_result: &mut FrameResult,
    ) -> Result<(), Self::Error> {
        self.op.last_frame_result(evm, frame_result)
    }

    fn reimburse_caller(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut FrameResult,
    ) -> Result<(), Self::Error> {
        // For CIP-64 transactions with a non-zero fee currency, we credit the fee
        // currency all in the same place (in reward_beneficiary). Txs that pay fees in
        // native CELO — including CIP-64 txs with `feeCurrency == Address::ZERO` —
        // use the native reimbursement path here.
        let fees_in_celo = evm.ctx().tx().is_fee_in_celo();
        if fees_in_celo {
            self.op.mainnet.reimburse_caller(evm, exec_result)?;
        }

        let context = evm.ctx();
        if context.tx().tx_type() != DEPOSIT_TRANSACTION_TYPE && fees_in_celo {
            let caller = context.tx().caller();
            let spec = *context.cfg().spec();
            let operator_fee_refund = context.chain().operator_fee_refund(exec_result.gas(), spec);

            // In additional to the normal transaction fee, additionally refund the caller
            // for the operator fee.
            evm.ctx()
                .journal_mut()
                .balance_incr(caller, operator_fee_refund)?;
        }

        Ok(())
    }

    fn refund(&self, evm: &mut Self::Evm, exec_result: &mut FrameResult, eip7702_refund: i64) {
        self.op.refund(evm, exec_result, eip7702_refund)
    }

    fn reward_beneficiary(
        &self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        let is_deposit = evm.ctx().tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;

        // Transfer fee to coinbase/beneficiary.
        // For CIP-64 txs paying in a non-zero fee currency, fee distribution happens in
        // `cip64_credit_fee_currency`; native-fee txs (including CIP-64 with zero fee
        // currency) go through the normal payout path below.
        if is_deposit || !evm.ctx().tx().is_fee_in_celo() {
            return Ok(());
        }

        self.op.mainnet.reward_beneficiary(evm, frame_result)?;
        let basefee = evm.ctx().block().basefee() as u128;

        // If the transaction is not a deposit transaction, fees are paid out
        // to both the Base Fee Vault as well as the L1 Fee Vault.
        let ctx = evm.ctx();
        let enveloped = ctx.tx().enveloped_tx().cloned();
        let spec = *ctx.cfg().spec();
        let l1_block_info = &mut ctx.chain_mut();

        let Some(enveloped_tx) = &enveloped else {
            return Err(ERROR::from_string(
                "[OPTIMISM] Failed to load enveloped transaction.".into(),
            ));
        };

        // EIP-8037 reservoir TODO (inert until an Amsterdam-equivalent OpSpecId
        // activates; `gas().used()` is reservoir-free before that): the caller refund
        // (`reimburse_caller`) and the coinbase tip (`self.op.mainnet.reward_beneficiary`
        // above) delegate to the mainnet handler and inherit its reservoir handling, but
        // the celo-specific base-fee and operator-fee distribution below meters on raw
        // `frame_result.gas().used()`. Once the reservoir can be non-zero, `used()` must
        // be reservoir-adjusted (true consumed gas) here so the FeeHandler and operator-
        // fee recipient are not paid on reserved-but-unused state gas. Fix alongside the
        // CIP-64 credit TODO in `cip64_credit_fee_currency`.
        let l1_cost = l1_block_info.calculate_tx_l1_cost(enveloped_tx, spec);
        let operator_fee_cost = if spec.is_enabled_in(OpSpecId::ISTHMUS) {
            l1_block_info.operator_fee_charge(
                enveloped_tx,
                U256::from(frame_result.gas().used()),
                spec,
            )
        } else {
            U256::ZERO
        };
        // Send the base fee of the transaction to the FeeHandler.
        let fee_handler = get_addresses(ctx.cfg().chain_id()).fee_handler;
        let base_fee_amount = U256::from(basefee.saturating_mul(frame_result.gas().used() as u128));

        // Send fees to their respective recipients
        for (recipient, amount) in [
            (L1_FEE_RECIPIENT, l1_cost),
            (fee_handler, base_fee_amount),
            (OPERATOR_FEE_RECIPIENT, operator_fee_cost),
        ] {
            ctx.journal_mut().balance_incr(recipient, amount)?;
        }

        Ok(())
    }

    fn execution_result(
        &mut self,
        evm: &mut Self::Evm,
        frame_result: <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
        result_gas: revm::context_interface::result::ResultGas,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        let exec_result = self.build_execution_result(evm, frame_result, result_gas)?;

        // Commit journal, clear frame stack, clear l1_block_info.
        //
        // `commit_tx` is skipped in `no_commit` mode: it does a bare `journal.clear()` that
        // empties the shared revert log, which would make a surrounding `checkpoint_revert` a
        // no-op. Leaving the entries in place is exactly what lets `run_system_call_no_commit`'s
        // callers roll the call back (see `core_contracts::call_read_only`). The rest of the
        // teardown (L1-cost cache, local context, frame stack) runs either way.
        if !self.no_commit {
            evm.ctx().journal_mut().commit_tx();
        }
        evm.ctx().chain_mut().clear_tx_l1_cost();
        evm.ctx().local_mut().clear();
        evm.frame_stack().clear();

        Ok(exec_result)
    }

    fn catch_error(
        &self,
        evm: &mut Self::Evm,
        error: Self::Error,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        self.op.catch_error(evm, error)
    }
}

impl<ERROR, DB, INSP, P> InspectorHandler
    for CeloHandler<CeloEvm<DB, INSP, P>, ERROR, revm::handler::EthFrame<EthInterpreter>>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
    ERROR:
        EvmTrError<CeloEvm<DB, INSP, P>> + From<OpTransactionError> + FromStringError + IsTxError,
{
    type IT = EthInterpreter;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CeloBuilder, CeloContext, DefaultCelo};
    use op_revm::L1BlockInfo;
    use revm::{
        context::{Context, TransactionType},
        context_interface::result::{EVMError, InvalidTransaction},
        database::InMemoryDB,
        database_interface::EmptyDB,
        handler::EthFrame,
        interpreter::{CallOutcome, Gas, InstructionResult, InterpreterResult},
        primitives::{Address, B256, Bytes, bytes},
        state::AccountInfo,
    };
    use rstest::rstest;

    /// Creates frame result.
    fn call_last_frame_return(
        ctx: CeloContext<EmptyDB>,
        instruction_result: InstructionResult,
        gas: Gas,
    ) -> Gas {
        let mut evm = ctx.build_celo();

        let mut exec_result = FrameResult::Call(CallOutcome::new(
            InterpreterResult {
                result: instruction_result,
                output: Bytes::new(),
                gas,
            },
            0..0,
        ));

        let mut handler =
            CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();

        handler
            .last_frame_result(&mut evm, &mut exec_result)
            .unwrap();
        handler.refund(&mut evm, &mut exec_result, 0);
        *exec_result.gas()
    }

    #[test]
    fn test_revert_gas() {
        let ctx = Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.gas_limit = 100;
                tx.op_tx.enveloped_tx = None;
            })
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::BEDROCK);

        let gas = call_last_frame_return(ctx, InstructionResult::Revert, Gas::new(90));
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.total_gas_spent(), 10);
        assert_eq!(gas.refunded(), 0);
    }

    #[test]
    fn test_consume_gas() {
        let ctx = Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.gas_limit = 100;
                tx.op_tx.deposit.source_hash = B256::ZERO;
                tx.op_tx.base.tx_type = DEPOSIT_TRANSACTION_TYPE;
            })
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::REGOLITH);

        let gas = call_last_frame_return(ctx, InstructionResult::Stop, Gas::new(90));
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.total_gas_spent(), 10);
        assert_eq!(gas.refunded(), 0);
    }

    #[test]
    fn test_consume_gas_with_refund() {
        let ctx = Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.gas_limit = 100;
                tx.op_tx.base.tx_type = DEPOSIT_TRANSACTION_TYPE;
                tx.op_tx.deposit.source_hash = B256::ZERO;
            })
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::REGOLITH);

        let mut ret_gas = Gas::new(90);
        ret_gas.record_refund(20);

        let gas = call_last_frame_return(ctx.clone(), InstructionResult::Stop, ret_gas);
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.total_gas_spent(), 10);
        assert_eq!(gas.refunded(), 2); // min(20, 10/5)

        let gas = call_last_frame_return(ctx, InstructionResult::Revert, ret_gas);
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.total_gas_spent(), 10);
        assert_eq!(gas.refunded(), 0);
    }

    #[test]
    fn test_consume_gas_deposit_tx() {
        let ctx = Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.tx_type = DEPOSIT_TRANSACTION_TYPE;
                tx.op_tx.base.gas_limit = 100;
                tx.op_tx.deposit.source_hash = B256::ZERO;
            })
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::BEDROCK);
        let gas = call_last_frame_return(ctx, InstructionResult::Stop, Gas::new(90));
        assert_eq!(gas.remaining(), 0);
        assert_eq!(gas.total_gas_spent(), 100);
        assert_eq!(gas.refunded(), 0);
    }

    #[test]
    fn test_consume_gas_sys_deposit_tx() {
        let ctx = Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.tx_type = DEPOSIT_TRANSACTION_TYPE;
                tx.op_tx.base.gas_limit = 100;
                tx.op_tx.deposit.source_hash = B256::ZERO;
                tx.op_tx.deposit.is_system_transaction = true;
            })
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::BEDROCK);
        let gas = call_last_frame_return(ctx, InstructionResult::Stop, Gas::new(90));
        assert_eq!(gas.remaining(), 100);
        assert_eq!(gas.total_gas_spent(), 0);
        assert_eq!(gas.refunded(), 0);
    }

    #[test]
    fn test_commit_mint_value() {
        let caller = Address::ZERO;
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(1000),
                ..Default::default()
            },
        );

        let l1_block_info = L1BlockInfo {
            l1_base_fee: U256::from(1_000),
            l1_fee_overhead: Some(U256::from(1_000)),
            l1_base_fee_scalar: U256::from(1_000),
            ..Default::default()
        };

        let mut ctx = Context::celo()
            .with_db(db)
            .with_chain(l1_block_info)
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::REGOLITH);
        ctx.modify_tx(|celo_tx| {
            let tx = &mut celo_tx.op_tx;
            tx.base.tx_type = DEPOSIT_TRANSACTION_TYPE;
            tx.deposit.source_hash = B256::ZERO;
            tx.deposit.mint = Some(10);
        });

        let mut evm = ctx.build_celo();

        let handler =
            CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        handler
            .validate_against_state_and_deduct_caller(&mut evm, &mut Default::default())
            .unwrap();

        // Check the account balance is updated.
        let account = evm.ctx().journal_mut().load_account(caller).unwrap();
        assert_eq!(account.data.info.balance, U256::from(1010));
    }

    #[test]
    fn test_remove_l1_cost_non_deposit() {
        let caller = Address::ZERO;
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(1000),
                ..Default::default()
            },
        );

        let l1_block_info = L1BlockInfo {
            l1_base_fee: U256::from(1_000),
            l1_fee_overhead: Some(U256::from(1_000)),
            l1_base_fee_scalar: U256::from(1_000),
            ..Default::default()
        };

        let ctx = Context::celo()
            .with_db(db)
            .with_chain(l1_block_info)
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::REGOLITH)
            .modify_tx_chained(|celo_tx| {
                let tx = &mut celo_tx.op_tx;
                tx.base.gas_limit = 100;
                tx.base.tx_type = DEPOSIT_TRANSACTION_TYPE;
                tx.deposit.mint = Some(10);
                tx.enveloped_tx = Some(bytes!("FACADE"));
                tx.deposit.source_hash = B256::ZERO;
            });

        let mut evm = ctx.build_celo();

        let handler =
            CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        handler
            .validate_against_state_and_deduct_caller(&mut evm, &mut Default::default())
            .unwrap();

        // Check the account balance is updated.
        let account = evm.ctx().journal_mut().load_account(caller).unwrap();
        assert_eq!(account.data.info.balance, U256::from(1010));
    }

    #[test]
    fn test_remove_l1_cost() {
        let caller = Address::ZERO;
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(1049),
                ..Default::default()
            },
        );

        let l1_block_info = L1BlockInfo {
            l1_base_fee: U256::from(1_000),
            l1_fee_overhead: Some(U256::from(1_000)),
            l1_base_fee_scalar: U256::from(1_000),
            l2_block: Some(U256::from(0)),
            ..Default::default()
        };

        let ctx = Context::celo()
            .with_db(db)
            .with_chain(l1_block_info)
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::REGOLITH)
            .modify_tx_chained(|celo_tx| {
                let tx = &mut celo_tx.op_tx;
                tx.base.gas_limit = 100;
                tx.deposit.source_hash = B256::ZERO;
                tx.enveloped_tx = Some(bytes!("FACADE"));
            });

        let mut evm = ctx.build_celo();
        let handler =
            CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();

        // l1block cost is 1048 fee.
        handler
            .validate_against_state_and_deduct_caller(&mut evm, &mut Default::default())
            .unwrap();

        // Check the account balance is updated.
        let account = evm.ctx().journal_mut().load_account(caller).unwrap();
        assert_eq!(account.info.balance, U256::from(1));
    }

    #[test]
    fn test_remove_operator_cost_isthmus() {
        let caller = Address::ZERO;
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(151),
                ..Default::default()
            },
        );

        let l1_block_info = L1BlockInfo {
            operator_fee_scalar: Some(U256::from(10_000_000)),
            operator_fee_constant: Some(U256::from(50)),
            l2_block: Some(U256::from(0)),
            ..Default::default()
        };

        let ctx = Context::celo()
            .with_db(db)
            .with_chain(l1_block_info)
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::ISTHMUS)
            .modify_tx_chained(|celo_tx| {
                let tx = &mut celo_tx.op_tx;
                tx.base.gas_limit = 10;
                tx.enveloped_tx = Some(bytes!("FACADE"));
            });

        let mut evm = ctx.build_celo();
        let handler =
            CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();

        // Under Isthmus the operator fee cost is operator_fee_scalar * gas_limit / 1e6 + operator_fee_constant
        // 10_000_000 * 10 / 1_000_000 + 50 = 150
        handler
            .validate_against_state_and_deduct_caller(&mut evm, &mut Default::default())
            .unwrap();

        // Check the account balance is updated.
        let account = evm.ctx().journal_mut().load_account(caller).unwrap();
        assert_eq!(account.info.balance, U256::from(1));
    }

    #[test]
    fn test_remove_operator_cost_jovian() {
        let caller = Address::ZERO;
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(2_051),
                ..Default::default()
            },
        );

        let l1_block_info = L1BlockInfo {
            operator_fee_scalar: Some(U256::from(2)),
            operator_fee_constant: Some(U256::from(50)),
            l2_block: Some(U256::from(0)),
            ..Default::default()
        };

        let ctx = Context::celo()
            .with_db(db)
            .with_chain(l1_block_info)
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::JOVIAN)
            .modify_tx_chained(|celo_tx| {
                let tx = &mut celo_tx.op_tx;
                tx.base.gas_limit = 10;
                tx.enveloped_tx = Some(bytes!("FACADE"));
            });

        let mut evm = ctx.build_celo();
        let handler =
            CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();

        // Under Jovian the operator fee cost is operator_fee_scalar * gas_limit * 100 + operator_fee_constant
        // 2 * 10 * 100 + 50 = 2_050
        handler
            .validate_against_state_and_deduct_caller(&mut evm, &mut Default::default())
            .unwrap();

        let account = evm.ctx().journal_mut().load_account(caller).unwrap();
        assert_eq!(account.info.balance, U256::from(1));
    }

    #[test]
    fn test_remove_l1_cost_lack_of_funds() {
        let caller = Address::ZERO;
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(48),
                ..Default::default()
            },
        );

        let l1_block_info = L1BlockInfo {
            l1_base_fee: U256::from(1_000),
            l1_fee_overhead: Some(U256::from(1_000)),
            l1_base_fee_scalar: U256::from(1_000),
            l2_block: Some(U256::from(0)),
            ..Default::default()
        };

        let ctx = Context::celo()
            .with_db(db)
            .with_chain(l1_block_info)
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::REGOLITH)
            .modify_tx_chained(|tx| {
                tx.op_tx.enveloped_tx = Some(bytes!("FACADE"));
            });

        // l1block cost is 1048 fee.
        let mut evm = ctx.build_celo();
        let handler =
            CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();

        // l1block cost is 1048 fee.
        assert_eq!(
            handler.validate_against_state_and_deduct_caller(&mut evm, &mut Default::default()),
            Err(EVMError::Transaction(
                InvalidTransaction::LackOfFundForMaxFee {
                    fee: Box::new(U256::from(1048)),
                    balance: Box::new(U256::from(48)),
                }
                .into(),
            ))
        );
    }

    #[test]
    fn test_validate_sys_tx() {
        // mark the tx as a system transaction.
        let ctx = Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.tx_type = DEPOSIT_TRANSACTION_TYPE;
                tx.op_tx.deposit.is_system_transaction = true;
            })
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::REGOLITH);

        let mut evm = ctx.build_celo();
        let handler =
            CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();

        assert_eq!(
            handler.validate_env(&mut evm),
            Err(EVMError::Transaction(
                OpTransactionError::DepositSystemTxPostRegolith
            ))
        );

        evm.ctx().modify_cfg(|cfg| cfg.spec = OpSpecId::BEDROCK);

        // Pre-regolith system transactions should be allowed.
        assert!(handler.validate_env(&mut evm).is_ok());
    }

    #[test]
    fn test_validate_deposit_tx() {
        // Set source hash.
        let ctx = Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.tx_type = DEPOSIT_TRANSACTION_TYPE;
                tx.op_tx.deposit.source_hash = B256::ZERO;
            })
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::REGOLITH);

        let mut evm = ctx.build_celo();
        let handler =
            CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();

        assert!(handler.validate_env(&mut evm).is_ok());
    }

    #[test]
    fn test_validate_tx_against_state_deposit_tx() {
        // Set source hash.
        let ctx = Context::celo()
            .modify_tx_chained(|celo_tx| {
                let tx = &mut celo_tx.op_tx;
                tx.base.tx_type = DEPOSIT_TRANSACTION_TYPE;
                tx.deposit.source_hash = B256::ZERO;
            })
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::REGOLITH);

        let mut evm = ctx.build_celo();
        let handler =
            CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();

        // Nonce and balance checks should be skipped for deposit transactions.
        assert!(handler.validate_env(&mut evm).is_ok());
    }

    #[rstest]
    #[case::deposit(true)]
    #[case::dyn_fee(false)]
    fn test_operator_fee_refund(#[case] is_deposit: bool) {
        const SENDER: Address = Address::ZERO;
        const GAS_PRICE: u128 = 0xFF;
        const OP_FEE_MOCK_PARAM: u128 = 0xFFFF;

        let ctx = Context::celo()
            .modify_tx_chained(|celo_tx| {
                let tx = &mut celo_tx.op_tx;
                tx.base.tx_type = if is_deposit {
                    DEPOSIT_TRANSACTION_TYPE
                } else {
                    TransactionType::Eip1559 as u8
                };
                tx.base.gas_price = GAS_PRICE;
                tx.base.gas_priority_fee = None;
                tx.base.caller = SENDER;
            })
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::ISTHMUS);

        let mut evm = ctx.build_celo();
        let handler =
            CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();

        // Set the operator fee scalar & constant to non-zero values in the L1 block info.
        evm.ctx().chain.operator_fee_scalar = Some(U256::from(OP_FEE_MOCK_PARAM));
        evm.ctx().chain.operator_fee_constant = Some(U256::from(OP_FEE_MOCK_PARAM));

        let mut gas = Gas::new(100);
        gas.set_spent(10);
        let mut exec_result = FrameResult::Call(CallOutcome::new(
            InterpreterResult {
                result: InstructionResult::Return,
                output: Default::default(),
                gas,
            },
            0..0,
        ));

        // Reimburse the caller for the unspent portion of the fees.
        handler
            .reimburse_caller(&mut evm, &mut exec_result)
            .unwrap();

        // Compute the expected refund amount. If the transaction is a deposit, the operator fee
        // refund never applies. If the transaction is not a deposit, the operator fee
        // refund is added to the refund amount.
        let mut expected_refund =
            U256::from(GAS_PRICE * (gas.remaining() + gas.refunded() as u64) as u128);
        let op_fee_refund = evm
            .ctx()
            .chain()
            .operator_fee_refund(&gas, OpSpecId::ISTHMUS);
        assert!(op_fee_refund > U256::ZERO);

        if !is_deposit {
            expected_refund += op_fee_refund;
        }

        // Check that the caller was reimbursed the correct amount of ETH.
        let account = evm.ctx().journal_mut().load_account(SENDER).unwrap();
        assert_eq!(account.data.info.balance, expected_refund);
    }

    #[test]
    fn test_legacy_chain_id_exception() {
        // Test that transactions with wrong chain ID are rejected normally
        let ctx = Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.tx_type = TransactionType::Eip2930 as u8;
                tx.op_tx.base.chain_id = Some(999999); // Wrong chain ID
                tx.op_tx.enveloped_tx = Some(bytes!("deadbeef")); // Not an exception tx
            })
            .modify_cfg_chained(|cfg| {
                cfg.spec = OpSpecId::REGOLITH;
                cfg.chain_id = 42220; // Celo mainnet
            });

        let mut evm = ctx.build_celo();
        let handler =
            CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();

        // Should fail due to chain ID mismatch
        assert_eq!(
            handler.validate_env(&mut evm),
            Err(EVMError::Transaction(
                InvalidTransaction::InvalidChainId.into()
            ))
        );
    }

    /// Builds the EIP-2718 encoded Sepolia exception transaction.
    /// Celo Sepolia block 12531083 - tx had chain_id 11162320 instead of 11142220
    fn build_sepolia_exception_tx() -> Bytes {
        use alloy_consensus::{SignableTransaction, TxEip2930};
        use alloy_primitives::{Signature, U256, address};

        let tx = TxEip2930 {
            chain_id: 11162320, // Wrong chain ID (should be 11142220)
            nonce: 0,
            gas_price: 25001000000,
            gas_limit: 21000,
            to: address!("52BCbd8Bf68EE24A15adcD05951a49aE6c168A14").into(),
            value: U256::from(1),
            input: bytes!(""),
            access_list: Default::default(),
        };

        let sig = Signature::from_scalars_and_parity(
            b256!("c96e8be6c653d8eb6f03842ffdc29347745c8122893f9cc9b64809d1bc49302d"),
            b256!("49b51c8d25cff880327495cb1f322ebfbcb42151e9b617466eee2c737737f259"),
            false, // yParity: 0
        );

        let signed = tx.into_signed(sig);
        let mut encoded = Vec::new();
        signed.eip2718_encode(&mut encoded);
        encoded.into()
    }

    /// Builds the EIP-2718 encoded Mainnet exception transaction.
    /// Celo Mainnet block 53619115 - tx had chain_id 44787 instead of 42220
    fn build_mainnet_exception_tx() -> Bytes {
        use alloy_consensus::{SignableTransaction, TxEip2930};
        use alloy_primitives::{Signature, U256, address};

        let tx = TxEip2930 {
            chain_id: 44787, // Wrong chain ID (should be 42220)
            nonce: 5,
            gas_price: 30000000000,
            gas_limit: 30000,
            to: address!("C04b2FFAcc30C7FE19741E27ea150ccCc212e072").into(),
            value: U256::from(200000000000000_u64),
            input: bytes!(""),
            access_list: Default::default(),
        };

        let sig = Signature::from_scalars_and_parity(
            b256!("197400aceb14cacc9a75710ebb4d3cba85538fc96a2b254d51a0db742c24ad08"),
            b256!("2c18b58821fe02d107ff1fa9f4fd157bafb571bb83867d56618de3e1045141bb"),
            true, // yParity: 1
        );

        let signed = tx.into_signed(sig);
        let mut encoded = Vec::new();
        signed.eip2718_encode(&mut encoded);
        encoded.into()
    }

    #[test]
    fn test_legacy_chain_id_exception_sepolia_tx_hash() {
        // Verify the encoded tx hashes to the expected exception hash
        let encoded = build_sepolia_exception_tx();
        assert_eq!(keccak256(&encoded), LEGACY_CHAIN_ID_EXCEPTIONS[0].0);
        assert!(is_legacy_chain_id_exception(
            11142220,
            12531083,
            Some(&encoded)
        ));
        // Should not match on wrong network
        assert!(!is_legacy_chain_id_exception(
            42220,
            12531083,
            Some(&encoded)
        ));
        // Should not match at the wrong block height (live-traffic case)
        assert!(!is_legacy_chain_id_exception(
            11142220,
            12531084,
            Some(&encoded)
        ));
    }

    #[test]
    fn test_legacy_chain_id_exception_mainnet_tx_hash() {
        // Verify the encoded tx hashes to the expected exception hash
        let encoded = build_mainnet_exception_tx();
        assert_eq!(keccak256(&encoded), LEGACY_CHAIN_ID_EXCEPTIONS[1].0);
        assert!(is_legacy_chain_id_exception(
            42220,
            53619115,
            Some(&encoded)
        ));
        // Should not match on wrong network
        assert!(!is_legacy_chain_id_exception(
            11142220,
            53619115,
            Some(&encoded)
        ));
        // Should not match at the wrong block height (live-traffic case)
        assert!(!is_legacy_chain_id_exception(
            42220,
            53619116,
            Some(&encoded)
        ));
    }

    #[test]
    fn test_legacy_chain_id_exception_sepolia_validate_env_passes() {
        // Test that the Sepolia exception tx passes validate_env despite wrong chain ID
        let encoded = build_sepolia_exception_tx();

        let ctx = Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.tx_type = TransactionType::Eip2930 as u8;
                tx.op_tx.base.chain_id = Some(11162320); // Wrong chain ID in tx
                tx.op_tx.base.gas_limit = 21000;
                tx.op_tx.base.gas_price = 25001000000;
                tx.op_tx.enveloped_tx = Some(encoded);
            })
            .modify_block_chained(|block| {
                block.number = U256::from(12531083u64); // historical exception block
            })
            .modify_cfg_chained(|cfg| {
                cfg.spec = OpSpecId::REGOLITH;
                cfg.chain_id = 11142220; // Correct network chain ID
            });

        let mut evm = ctx.build_celo();
        let handler =
            CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();

        // Should pass validation because this is an exception tx
        assert!(handler.validate_env(&mut evm).is_ok());

        // Verify tx_chain_id_check is restored to true after validation
        assert!(evm.ctx().cfg().tx_chain_id_check);
    }

    #[test]
    fn test_legacy_chain_id_exception_mainnet_validate_env_passes() {
        // Test that the Mainnet exception tx passes validate_env despite wrong chain ID
        let encoded = build_mainnet_exception_tx();

        let ctx = Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.tx_type = TransactionType::Eip2930 as u8;
                tx.op_tx.base.chain_id = Some(44787); // Wrong chain ID in tx
                tx.op_tx.base.gas_limit = 30000;
                tx.op_tx.base.gas_price = 30000000000;
                tx.op_tx.enveloped_tx = Some(encoded);
            })
            .modify_block_chained(|block| {
                block.number = U256::from(53619115u64); // historical exception block
            })
            .modify_cfg_chained(|cfg| {
                cfg.spec = OpSpecId::REGOLITH;
                cfg.chain_id = 42220; // Correct network chain ID
            });

        let mut evm = ctx.build_celo();
        let handler =
            CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();

        // Should pass validation because this is an exception tx
        assert!(handler.validate_env(&mut evm).is_ok());

        // Verify tx_chain_id_check is restored to true after validation
        assert!(evm.ctx().cfg().tx_chain_id_check);
    }

    #[test]
    fn test_legacy_chain_id_exception_correct_chain_id_still_works() {
        // Test that a tx with matching chain ID still works (no exception needed)
        let ctx = Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.tx_type = TransactionType::Eip2930 as u8;
                tx.op_tx.base.chain_id = Some(42220); // Correct chain ID
                tx.op_tx.base.gas_limit = 21000;
                tx.op_tx.base.gas_price = 1000000000;
                tx.op_tx.enveloped_tx = Some(bytes!("deadbeef"));
            })
            .modify_cfg_chained(|cfg| {
                cfg.spec = OpSpecId::REGOLITH;
                cfg.chain_id = 42220;
            });

        let mut evm = ctx.build_celo();
        let handler =
            CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();

        // Should pass - chain IDs match, no exception logic triggered
        assert!(handler.validate_env(&mut evm).is_ok());
    }

    // -----------------------------------------------------------------------
    // CIP-64 debit/credit handler tests
    // -----------------------------------------------------------------------

    use crate::contracts::core_contracts::tests::{
        TEST_FEE_CURRENCY, make_celo_test_db_with_fee_currency,
    };
    use alloy_primitives::address;
    use revm::{ExecuteEvm, inspector::NoOpInspector, primitives::TxKind};

    /// Build a `CeloEvm` primed to run a single CIP-64 transaction paying in
    /// [`TEST_FEE_CURRENCY`], over a caller-supplied `db` so tests can pre-seed the sender's
    /// nonce, a stub target contract, or a non-conformant fee currency. Everything else — the
    /// tx caller/fees/kind/nonce/gas, the block basefee/beneficiary, and the REGOLITH chain-0
    /// cfg — is wired the same way for every CIP-64 handler test. [`run_cip64_tx`] is a thin
    /// replay wrapper over this.
    #[allow(clippy::too_many_arguments)]
    fn build_cip64_evm(
        db: InMemoryDB,
        sender: Address,
        beneficiary: Address,
        nonce: u64,
        kind: TxKind,
        gas_limit: u64,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        basefee: u64,
    ) -> CeloEvm<InMemoryDB, NoOpInspector> {
        Context::celo()
            .with_db(db)
            .modify_tx_chained(|tx| {
                tx.op_tx.base.tx_type = CeloTxType::Cip64 as u8;
                tx.fee_currency = Some(TEST_FEE_CURRENCY);
                tx.op_tx.base.gas_limit = gas_limit;
                tx.op_tx.base.gas_price = max_fee_per_gas;
                tx.op_tx.base.gas_priority_fee = Some(max_priority_fee_per_gas);
                tx.op_tx.base.caller = sender;
                tx.op_tx.base.nonce = nonce;
                tx.op_tx.base.kind = kind;
                tx.op_tx.base.chain_id = Some(0); // test chain uses chain_id 0
                tx.op_tx.enveloped_tx = Some(bytes!("FACADE"));
            })
            .modify_block_chained(|block| {
                block.basefee = basefee;
                block.beneficiary = beneficiary;
            })
            .modify_cfg_chained(|cfg| {
                cfg.spec = OpSpecId::REGOLITH;
                cfg.chain_id = 0; // match test chain
            })
            .build_celo()
    }

    /// Run a CIP-64 transaction through the full handler pipeline and return the
    /// execution result.
    fn run_cip64_tx(
        sender: Address,
        fc_balance: U256,
        gas_limit: u64,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        basefee: u64,
        beneficiary: Address,
    ) -> Result<
        revm::context_interface::result::ExecutionResult<OpHaltReason>,
        EVMError<<InMemoryDB as revm::Database>::Error, OpTransactionError>,
    > {
        let db = make_celo_test_db_with_fee_currency(sender, fc_balance);
        // Default kind matches `TxEnv`'s own default (`Call(Address::ZERO)`).
        let mut evm = build_cip64_evm(
            db,
            sender,
            beneficiary,
            0,
            TxKind::Call(Address::ZERO),
            gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            basefee,
        );
        evm.replay().map(|r| r.result)
    }

    #[test]
    fn test_cip64_happy_path() {
        let sender = address!("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let beneficiary = address!("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        // Give sender plenty of ERC20 balance
        let fc_balance = U256::from(1_000_000_000_000u128);
        // basefee=1, rate=20/10=2x, so base_fee_in_erc20=2
        // effective_gas_price = min(max_fee, basefee_erc20 + priority) = min(100, 2+10) = 12
        // gas_cost = 100_000 * 12 = 1_200_000
        let result = run_cip64_tx(sender, fc_balance, 100_000, 100, 10, 1, beneficiary);
        assert!(
            result.is_ok(),
            "CIP-64 happy path should succeed: {result:?}"
        );
        let exec = result.unwrap();
        assert!(exec.is_success(), "Execution should succeed: {exec:?}");
    }

    #[test]
    fn test_cip64_unregistered_currency() {
        // Use a fee currency that is NOT registered in the FeeCurrencyDirectory.
        let sender = address!("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let beneficiary = address!("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let db = make_celo_test_db_with_fee_currency(sender, U256::from(1_000_000u64));

        // Set fee_currency to an unregistered address
        let unregistered_fc = address!("0xdeaddeaddeaddeaddeaddeaddeaddeaddeaddead");
        let ctx = Context::celo()
            .with_db(db)
            .modify_tx_chained(|tx| {
                tx.op_tx.base.tx_type = CeloTxType::Cip64 as u8;
                tx.fee_currency = Some(unregistered_fc);
                tx.op_tx.base.gas_limit = 100_000;
                tx.op_tx.base.gas_price = 100;
                tx.op_tx.base.gas_priority_fee = Some(10);
                tx.op_tx.base.caller = sender;
                tx.op_tx.base.nonce = 0;
                tx.op_tx.base.chain_id = Some(0);
                tx.op_tx.enveloped_tx = Some(bytes!("FACADE"));
            })
            .modify_block_chained(|block| {
                block.basefee = 1;
                block.beneficiary = beneficiary;
            })
            .modify_cfg_chained(|cfg| {
                cfg.spec = OpSpecId::REGOLITH;
                cfg.chain_id = 0;
            });

        let mut evm = ctx.build_celo();
        let result = evm.replay();
        // Should fail during validation — unregistered fee currency
        assert!(
            result.is_err(),
            "Unregistered currency should be rejected: {result:?}"
        );
        let err_str = format!("{:?}", result.unwrap_err());
        assert!(
            err_str.contains("unregistered") || err_str.contains("not registered"),
            "Error should mention unregistered currency: {err_str}"
        );
    }

    #[test]
    fn test_cip64_insufficient_balance() {
        let sender = address!("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let beneficiary = address!("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        // Give sender very little ERC20 balance — not enough for gas
        // basefee=1, rate=20/10=2x, base_fee_in_erc20=2
        // effective_gas_price = min(100, 2+10) = 12
        // gas_cost = 100_000 * 12 = 1_200_000, but balance is only 100
        let fc_balance = U256::from(100u64);
        let result = run_cip64_tx(sender, fc_balance, 100_000, 100, 10, 1, beneficiary);
        // Should fail — debitGasFees will fail due to insufficient balance
        assert!(
            result.is_err(),
            "Insufficient ERC20 balance should be rejected: {result:?}"
        );
    }

    #[test]
    fn test_cip64_exchange_rate_conversion() {
        // Verify that base_fee_in_erc20 is correctly converted using the exchange rate.
        // The test DB has rate = 20/10 (numerator=20, denominator=10),
        // meaning 1 CELO = 2 FC (or equivalently, base_fee * numerator / denominator).
        let sender = address!("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let beneficiary = address!("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let fc_balance = U256::from(1_000_000_000_000u128);

        // basefee=100, rate=20/10 → base_fee_in_erc20 = 100 * 20 / 10 = 200
        // max_fee=500, priority=50
        // effective_gas_price = min(500, 200+50) = 250
        // gas_cost = 100_000 * 250 = 25_000_000
        let result = run_cip64_tx(sender, fc_balance, 100_000, 500, 50, 100, beneficiary);
        assert!(
            result.is_ok(),
            "Exchange rate conversion should work: {result:?}"
        );
        let exec = result.unwrap();
        assert!(exec.is_success(), "Execution should succeed: {exec:?}");
    }

    #[test]
    fn test_cip64_zero_beneficiary_fallback() {
        // When beneficiary is Address::ZERO, fees should go to the fee_handler instead.
        // This tests the zero-beneficiary fallback in cip64_credit_fee_currency.
        let sender = address!("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let fc_balance = U256::from(1_000_000_000_000u128);

        let result = run_cip64_tx(sender, fc_balance, 100_000, 100, 10, 1, Address::ZERO);
        assert!(
            result.is_ok(),
            "Zero beneficiary should fall back to fee_handler: {result:?}"
        );
        let exec = result.unwrap();
        assert!(
            exec.is_success(),
            "Execution should succeed with zero beneficiary: {exec:?}"
        );
    }

    #[test]
    fn test_cip64_zero_address_fee_currency_uses_native_path() {
        // A CIP-64 tx with `feeCurrency = Address::ZERO` must be treated as a
        // native-fee tx throughout the handler. Otherwise the tx gets charged in
        // native CELO (via `validate_against_state_and_deduct_caller`) but the
        // reimbursement and beneficiary payouts get skipped (they used to be
        // gated on `fee_currency().is_none()`), which over-charges the sender
        // and mis-distributes block fees. See PR #144 review.
        let sender = address!("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let beneficiary = address!("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        // Use fc_balance=0 — the tx must NOT touch any ERC20 state.
        let db = make_celo_test_db_with_fee_currency(sender, U256::ZERO);

        let ctx = Context::celo()
            .with_db(db)
            .modify_tx_chained(|tx| {
                tx.op_tx.base.tx_type = CeloTxType::Cip64 as u8;
                tx.fee_currency = Some(Address::ZERO);
                tx.op_tx.base.gas_limit = 100_000;
                tx.op_tx.base.gas_price = 100;
                tx.op_tx.base.gas_priority_fee = Some(10);
                tx.op_tx.base.caller = sender;
                tx.op_tx.base.nonce = 0;
                tx.op_tx.base.chain_id = Some(0);
                tx.op_tx.enveloped_tx = Some(bytes!("FACADE"));
            })
            .modify_block_chained(|block| {
                block.basefee = 1;
                block.beneficiary = beneficiary;
            })
            .modify_cfg_chained(|cfg| {
                cfg.spec = OpSpecId::REGOLITH;
                cfg.chain_id = 0;
            });

        let mut evm = ctx.build_celo();
        let output = evm
            .replay()
            .expect("CIP-64 with zero-address fee currency should use native path");
        assert!(
            output.result.is_success(),
            "Execution should succeed: {:?}",
            output.result
        );

        // Native fee path: caller's CELO balance must have dropped to cover gas,
        // and the beneficiary must have received a portion. Verify both sides of
        // the balance accounting (this would fail if reward_beneficiary's early
        // return still matched `fee_currency().is_some()`).
        let sender_balance = output
            .state
            .get(&sender)
            .map(|a| a.info.balance)
            .expect("sender account should exist in post-state");
        let beneficiary_balance = output
            .state
            .get(&beneficiary)
            .map(|a| a.info.balance)
            .unwrap_or(U256::ZERO);
        assert!(
            sender_balance < U256::from(1_000_000_000_000_000_000u128),
            "Sender CELO balance should have decreased: {sender_balance}"
        );
        assert!(
            beneficiary_balance > U256::ZERO,
            "Beneficiary should receive native CELO fees, got {beneficiary_balance}"
        );
    }

    #[test]
    fn test_cip64_gas_accounting() {
        // Verify that debit/credit gas is tracked in Cip64Info and stays within
        // the intrinsic gas budget (50_000 from the test DB × 3 = 150_000 max).
        let sender = address!("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let beneficiary = address!("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let fc_balance = U256::from(1_000_000_000_000u128);

        let db = make_celo_test_db_with_fee_currency(sender, fc_balance);
        let ctx = Context::celo()
            .with_db(db)
            .modify_tx_chained(|tx| {
                tx.op_tx.base.tx_type = CeloTxType::Cip64 as u8;
                tx.fee_currency = Some(TEST_FEE_CURRENCY);
                tx.op_tx.base.gas_limit = 200_000;
                tx.op_tx.base.gas_price = 100;
                tx.op_tx.base.gas_priority_fee = Some(10);
                tx.op_tx.base.caller = sender;
                tx.op_tx.base.nonce = 0;
                tx.op_tx.base.chain_id = Some(0);
                tx.op_tx.enveloped_tx = Some(bytes!("FACADE"));
            })
            .modify_block_chained(|block| {
                block.basefee = 1;
                block.beneficiary = beneficiary;
            })
            .modify_cfg_chained(|cfg| {
                cfg.spec = OpSpecId::REGOLITH;
                cfg.chain_id = 0;
            });

        let mut evm = ctx.build_celo();

        // Use the handler directly to inspect the tx after debit
        let mut handler =
            CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.run(&mut evm);
        assert!(result.is_ok(), "Handler run should succeed: {result:?}");

        // Check that cip64_tx_info was populated with gas values
        let ctx = evm.ctx();
        let cip64_info = ctx.tx().cip64_tx_info.as_ref();
        assert!(
            cip64_info.is_some(),
            "CIP-64 info should be set after execution"
        );
        let info = cip64_info.unwrap();

        // Debit and credit should have used some gas
        assert!(info.debit_gas_used > 0, "Debit should use gas");
        assert!(info.credit_gas_used > 0, "Credit should use gas");

        // Total raw gas (before refunds) should be within the max allowed (50_000 * 3 = 150_000)
        let total_raw = info.debit_gas_used
            + info.debit_gas_refunded
            + info.credit_gas_used
            + info.credit_gas_refunded;
        assert!(
            total_raw <= 150_000,
            "Total debit+credit gas ({total_raw}) should be within intrinsic budget (150_000)"
        );

        // Credit should generate Transfer event logs (from sender refund + tip/base to recipients)
        assert!(!info.logs_post.is_empty(), "Credit should generate logs");
    }

    /// Regression test for a storage-warmth leak from fee-currency context loading.
    ///
    /// Loading the context system-calls the FeeCurrencyDirectory (a proxy), reading its
    /// storage. That read is internal config loading, not part of the transaction's own
    /// execution, so it must not pre-warm any slot for the transaction. revm tracks warmth
    /// per `transaction_id`, and the system call runs at the surrounding transaction's id,
    /// so those slots used to stay *warm* — under-charging the transaction's first SLOAD of
    /// them. (A plain account-status reset missed slots, whose warmth is transaction_id
    /// based.) `load_fee_currency_context` now bumps the journal transaction id so
    /// everything touched during loading reads cold.
    #[test]
    fn test_context_loading_does_not_leak_storage_warmth() {
        use crate::contracts::core_contracts::tests::make_celo_test_db;
        use revm::{context_interface::ContextTr, handler::EvmTr};

        let ctx = Context::celo().with_db(make_celo_test_db());
        let mut evm = ctx.build_celo();

        let handler =
            CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();

        let id_before = evm.ctx().journal_mut().transaction_id;
        handler
            .load_fee_currency_context(&mut evm)
            .expect("loading the fee-currency context should succeed");
        let tx_id = evm.ctx().journal_mut().transaction_id;

        // The bump is the mechanism that isolates context-loading warmth; without it the
        // leak returns. This deterministically guards the fix.
        assert!(
            tx_id > id_before,
            "context loading must advance the transaction id (was {id_before}, now {tx_id})"
        );

        // Guard against a vacuous test: loading must actually have read FeeCurrencyDirectory
        // storage, otherwise the cold-warmth assertions below would pass over an empty journal.
        let directory = get_addresses(0).fee_currency_directory;
        let journal = evm.ctx().journal_mut();
        let dir_acct = journal
            .state
            .get(&directory)
            .expect("context loading should have loaded the FeeCurrencyDirectory account");
        assert!(
            !dir_acct.storage.is_empty(),
            "context loading should have read FeeCurrencyDirectory storage; an empty journal \
             would make the warmth assertions below vacuous"
        );

        // Every account and storage slot touched while loading — the directory, the oracle
        // behind it, and the system caller — must now read cold to the surrounding
        // transaction. (Precompiles live in WarmAddresses, not journal state, so they
        // correctly never appear here and stay warm.)
        for (addr, acct) in journal.state.iter() {
            assert!(
                acct.is_cold_transaction_id(tx_id),
                "account {addr} leaked warmth from context loading"
            );
            for (slot_key, slot) in acct.storage.iter() {
                assert!(
                    slot.is_cold_transaction_id(tx_id),
                    "slot {slot_key} of {addr} leaked warmth from context loading \
                     (would under-charge the transaction's first SLOAD)"
                );
            }
        }
    }

    /// The `_balances[account]` slot of the `FEE_CURRENCY_BYTECODE` ERC20 (Solidity
    /// mapping in slot 0): `keccak256(abi.encode(account, uint256(0)))`.
    fn fee_currency_balance_slot(account: Address) -> U256 {
        let mut buf = [0u8; 64];
        buf[12..32].copy_from_slice(account.as_slice());
        U256::from_be_bytes(keccak256(buf).0)
    }

    // -----------------------------------------------------------------------
    // A CIP-64 tx rejected by a state check that runs *after* the ERC20 fee debit must
    // leak no debit into the block: the rollbackable debit is reverted with the tx.
    //
    // The debit runs first (early), charging the sender's fee-currency balance, and only
    // then does `validate_against_state_and_deduct_caller` run the nonce / EIP-3607 /
    // affordability checks. Because the debit is bracketed by a journal checkpoint
    // (`cip64_rollbackable_debit_and_deduct_caller`), a rejection here reverts it. If the
    // debit instead committed irreversibly (the pre-fix behavior), the block builder —
    // which reuses one journal across the block, skips the invalid tx, and finalizes once —
    // would seal the orphaned debit, while a validator re-executing only the *included* txs
    // never applies it: the fee-currency balance (hence the state root) diverges — a
    // consensus split. This test rejects a nonce-too-low CIP-64 tx and asserts nothing is
    // debited.
    //
    // Driven through the bare handler (`handler.run` + a manual `finalize`), then asserting the
    // finalized fee-currency balance. On the rejection path op-revm's `catch_error` override
    // does no journal work for these non-deposit txs — it neither commits nor discards — so what
    // reaches `finalize` is whatever the debit left in the journal. Against a committing debit,
    // `commit_tx` already folded the charge into state and cleared the revert log, so the
    // rejected tx leaks the debit. The rollbackable (non-committing) debit keeps its revert-log
    // entries and `checkpoint_revert` unwinds them on the Err arm of
    // `cip64_rollbackable_debit_and_deduct_caller`, so nothing is debited.
    #[test]
    fn rejected_cip64_tx_reverts_the_fee_debit() {
        let sender = address!("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let beneficiary = address!("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let fc_balance = U256::from(1_000_000_000_000u128);

        let mut db = make_celo_test_db_with_fee_currency(sender, fc_balance);
        // Advance the sender's on-chain nonce to 1 so a tx with nonce 0 is rejected as
        // nonce-too-low — a check that runs after the debit. (The helper gives the sender
        // 1 CELO at nonce 0; we only bump the nonce.)
        db.insert_account_info(
            sender,
            AccountInfo {
                balance: U256::from(1_000_000_000_000_000_000u128),
                nonce: 1,
                ..Default::default()
            },
        );

        // tx nonce 0 vs the on-chain nonce 1 seeded above → NonceTooLow, a check that runs
        // after the debit.
        let mut evm = build_cip64_evm(
            db,
            sender,
            beneficiary,
            0,
            TxKind::Call(Address::ZERO),
            100_000,
            100,
            10,
            1,
        );

        // Mirror the builder: run the (rejected) tx through the handler, then finalize the
        // journal (the builder reuses one journal across the block and finalizes once).
        let mut handler =
            CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let err = handler
            .run(&mut evm)
            .expect_err("a nonce-too-low CIP-64 tx must be rejected");
        let err_str = format!("{err:?}");
        assert!(
            err_str.contains("Nonce") || err_str.contains("nonce"),
            "tx must be rejected by the nonce check (which runs after the debit), got: {err_str}"
        );

        let state = evm.finalize();

        // If the debit was rolled back, the slot is either absent from the post-state or
        // carries its original value; either way the balance is unchanged.
        let balance_slot = fee_currency_balance_slot(sender);
        let final_balance = state
            .get(&TEST_FEE_CURRENCY)
            .and_then(|acct| acct.storage.get(&balance_slot))
            .map(|slot| slot.present_value)
            .unwrap_or(fc_balance);

        assert_eq!(
            final_balance, fc_balance,
            "rejected CIP-64 tx leaked an ERC20 fee debit into the block \
             (balance {final_balance} != original {fc_balance}); the rollbackable debit \
             must be reverted when a later rejection check fails"
        );
    }

    // -----------------------------------------------------------------------
    // Caveat: revert-on-rejection must NOT become revert-on-execution-revert. A CIP-64 tx
    // that is *included* but whose EVM execution reverts still owes gas — the fee stays
    // debited (the checkpoint is committed in validation, before execution runs, so an
    // execution-time revert cannot unwind it). This pins that boundary.
    #[test]
    fn included_but_reverted_cip64_tx_still_pays_the_fee() {
        use revm::state::Bytecode;

        let sender = address!("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let beneficiary = address!("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let fc_balance = U256::from(1_000_000_000_000u128);

        // A contract that always reverts: PUSH1 0x00; PUSH1 0x00; REVERT.
        let revert_stub = address!("0x00000000000000000000000000000000000000dd");
        let mut db = make_celo_test_db_with_fee_currency(sender, fc_balance);
        let bytecode = Bytecode::new_raw(bytes!("60006000fd"));
        db.insert_account_info(
            revert_stub,
            AccountInfo {
                balance: U256::ZERO,
                nonce: 0,
                code_hash: bytecode.hash_slow(),
                account_id: None,
                code: Some(bytecode),
            },
        );

        let mut evm = build_cip64_evm(
            db,
            sender,
            beneficiary,
            0,
            TxKind::Call(revert_stub),
            200_000,
            100,
            10,
            1,
        );

        let out = evm.replay().expect("an included tx must not be rejected");
        assert!(
            matches!(out.result, ExecutionResult::Revert { .. }),
            "the main execution should revert: {:?}",
            out.result
        );

        // The debit must have stuck despite the execution revert: the checkpoint is
        // committed in validation, so only a *pre-inclusion rejection* reverts it.
        let balance_slot = fee_currency_balance_slot(sender);
        let final_balance = out
            .state
            .get(&TEST_FEE_CURRENCY)
            .and_then(|acct| acct.storage.get(&balance_slot))
            .map(|slot| slot.present_value)
            .expect("fee-currency balance slot must be present — the debit ran");
        assert!(
            final_balance < fc_balance,
            "an included-but-reverted CIP-64 tx must still pay its fee \
             (balance {final_balance} was not reduced from {fc_balance})"
        );
    }

    /// Runtime bytecode of a fee-currency stub whose response to *any* call transfers 1 wei of
    /// its own native balance to `sink`, then returns success:
    ///
    ///   PUSH1 00 (retLen) PUSH1 00 (retOff) PUSH1 00 (argLen) PUSH1 00 (argOff)
    ///   PUSH1 01 (value)  PUSH20 <sink> (addr) GAS CALL POP STOP
    ///
    /// Stands in for a non-conformant fee currency whose `debitGasFees` mutates *native* state,
    /// not just its own ERC20 storage slot.
    fn native_transfer_fee_currency_code(sink: Address) -> revm::state::Bytecode {
        let mut code = vec![
            0x60, 0x00, // PUSH1 0  -- retLength
            0x60, 0x00, // PUSH1 0  -- retOffset
            0x60, 0x00, // PUSH1 0  -- argsLength
            0x60, 0x00, // PUSH1 0  -- argsOffset
            0x60, 0x01, // PUSH1 1  -- value (1 wei)
            0x73, //       PUSH20   -- address
        ];
        code.extend_from_slice(sink.as_slice());
        code.extend_from_slice(&[
            0x5a, // GAS  -- forward all remaining gas
            0xf1, // CALL
            0x50, // POP  -- discard the success flag
            0x00, // STOP -- return success (empty output)
        ]);
        revm::state::Bytecode::new_raw(code.into())
    }

    // -----------------------------------------------------------------------
    // A fee currency whose `debitGasFees` mutates *native* state (here: transfers native CELO
    // out of itself) must have that mutation rolled back when the CIP-64 tx is rejected after
    // the debit. The rollback checkpoint spans native balances, not only the ERC20 balance
    // slot, so this closes the gap the removed conformance guard covered: a non-conformant
    // currency that touches native accounts is reverted with the tx on the reject path.
    //
    // Positive control: the same stub on an *included* tx does land its native transfer, so a
    // silently-skipped transfer (e.g. the inner CALL running out of gas) cannot make the
    // reject-path assertion pass for the wrong reason.
    #[test]
    fn rejected_cip64_tx_reverts_native_mutation_by_fee_currency() {
        let sender = address!("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let beneficiary = address!("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let sink = address!("0x00000000000000000000000000000000000000ee");
        let fc_balance = U256::from(1_000_000_000_000u128);
        // Pre-fund the sink so the fee currency's CALL is not a (pricier) new-account write.
        let sink_seed = U256::from(1_000u64);

        // Build a DB whose fee currency transfers native CELO to `sink` on any call, holding
        // enough native balance to do so. `sender_nonce` sets the sender's on-chain nonce.
        let build_evm = |sender_nonce: u64, tx_nonce: u64, kind: TxKind| {
            let mut db = make_celo_test_db_with_fee_currency(sender, fc_balance);
            let code = native_transfer_fee_currency_code(sink);
            db.insert_account_info(
                TEST_FEE_CURRENCY,
                AccountInfo {
                    balance: U256::from(1_000_000u64),
                    nonce: 0,
                    code_hash: code.hash_slow(),
                    account_id: None,
                    code: Some(code),
                },
            );
            db.insert_account_info(
                sink,
                AccountInfo {
                    balance: sink_seed,
                    ..Default::default()
                },
            );
            db.insert_account_info(
                sender,
                AccountInfo {
                    balance: U256::from(1_000_000_000_000_000_000u128),
                    nonce: sender_nonce,
                    ..Default::default()
                },
            );

            build_cip64_evm(db, sender, beneficiary, tx_nonce, kind, 200_000, 100, 10, 1)
        };

        let sink_balance = |state: &revm::state::EvmState| {
            state
                .get(&sink)
                .map(|acct| acct.info.balance)
                .unwrap_or(sink_seed)
        };

        // Positive control: a valid, included CIP-64 tx. The fee currency's native transfer
        // during the debit (and credit) actually lands, so the sink balance grows.
        let mut included = build_evm(0, 0, TxKind::Call(beneficiary));
        let out = included
            .replay()
            .expect("a valid CIP-64 tx must be included");
        assert!(
            matches!(out.result, ExecutionResult::Success { .. }),
            "control tx should succeed: {:?}",
            out.result
        );
        assert!(
            sink_balance(&out.state) > sink_seed,
            "control: the fee currency's native transfer must land on an included tx \
             (sink {} not increased from {sink_seed})",
            sink_balance(&out.state)
        );

        // Reject path: on-chain nonce 1, tx nonce 0 -> NonceTooLow, which runs after the debit.
        // The fee currency's native transfer done during the debit must be rolled back.
        let mut rejected = build_evm(1, 0, TxKind::Call(beneficiary));
        let mut handler =
            CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let err = handler
            .run(&mut rejected)
            .expect_err("a nonce-too-low CIP-64 tx must be rejected");
        let err_str = format!("{err:?}");
        assert!(
            err_str.contains("Nonce") || err_str.contains("nonce"),
            "tx must be rejected by the nonce check (which runs after the debit), got: {err_str}"
        );
        let state = rejected.finalize();
        assert_eq!(
            sink_balance(&state),
            sink_seed,
            "rejected CIP-64 tx leaked a native mutation made by the fee currency's debitGasFees; \
             the rollbackable debit must revert native balance changes, not only the ERC20 slot"
        );
    }
}
