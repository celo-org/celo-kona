//!Handler related to Celo chain

use crate::{
    CeloContext,
    common::fee_currency_context::FeeCurrencyContext,
    constants::get_addresses,
    contracts::{core_contracts::CoreContractError, erc20},
    evm::CeloEvm,
    transaction::{CeloTxTr, abstraction::Cip64Info},
};
use alloy_primitives::Address;
use celo_alloy_consensus::CeloTxType;
use op_revm::{
    L1BlockInfo, OpHaltReason, OpSpecId,
    constants::{L1_FEE_RECIPIENT, OPERATOR_FEE_RECIPIENT},
    handler::IsTxError,
    transaction::{OpTransactionError, OpTxTr, deposit::DEPOSIT_TRANSACTION_TYPE},
};
use revm::{
    Database, Inspector,
    context::{LocalContextTr, journaled_state::JournalCheckpoint, result::InvalidTransaction},
    context_interface::{
        Block, Cfg, ContextSetters, ContextTr, JournalTr, Transaction,
        result::{ExecutionResult, FromStringError},
    },
    handler::{
        EvmTr, FrameResult, Handler, MainnetHandler, evm::FrameTr, handler::EvmTrError,
        pre_execution::validate_account_nonce_and_code, validation::validate_priority_fee_tx,
    },
    inspector::InspectorHandler,
    interpreter::{
        Gas, InitialAndFloorGas, gas::calculate_initial_tx_gas_for_tx, interpreter::EthInterpreter,
    },
    primitives::{U256, hardfork::SpecId},
};
use std::{boxed::Box, format, string::ToString, vec::Vec};
use tracing::{info, warn};

pub struct CeloHandler<EVM, ERROR, FRAME> {
    pub mainnet: MainnetHandler<EVM, ERROR, FRAME>,
}

impl<EVM, ERROR, FRAME> CeloHandler<EVM, ERROR, FRAME> {
    pub fn new() -> Self {
        Self {
            mainnet: MainnetHandler::default(),
        }
    }
}

impl<EVM, ERROR, FRAME> Default for CeloHandler<EVM, ERROR, FRAME> {
    fn default() -> Self {
        Self::new()
    }
}

impl<ERROR, DB, INSP> CeloHandler<CeloEvm<DB, INSP>, ERROR, revm::handler::EthFrame<EthInterpreter>>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
    ERROR: EvmTrError<CeloEvm<DB, INSP>> + From<OpTransactionError> + FromStringError + IsTxError,
{
    fn load_fee_currency_context(&self, evm: &mut CeloEvm<DB, INSP>) -> Result<(), ERROR> {
        let current_block = evm.ctx().block().number();
        if evm.ctx().chain().fee_currency_context.updated_at_block != Some(current_block) {
            // Update the chain with the new fee currency context
            match FeeCurrencyContext::new_from_evm(evm) {
                Ok(fee_currency_context) => {
                    evm.ctx().chain_mut().fee_currency_context = fee_currency_context;
                }
                Err(CoreContractError::CoreContractMissing(_)) => {
                    // If core contracts are missing, we are probably in a non-celo test env.
                    // TODO: log a debug message here.
                }
                Err(e) => {
                    return Err(ERROR::from_string(e.to_string()));
                }
            }
        }

        Ok(())
    }

    fn cip64_get_base_fee_in_erc20(
        &self,
        evm: &mut CeloEvm<DB, INSP>,
        fee_currency: Option<Address>,
        basefee: u64,
    ) -> Result<u128, ERROR> {
        // Convert costs to fee currency
        let ctx = evm.ctx();
        let fee_currency_context = &ctx.chain().fee_currency_context;
        let base_fee_in_erc20 = fee_currency_context
            .celo_to_currency(fee_currency, U256::from(basefee))
            .map_err(|e| ERROR::from_string(e))?;
        // Convert base_fee_in_erc20 (U256) to u128 for gas price calculations
        let base_fee_in_erc20_u128: u128 = base_fee_in_erc20
            .try_into()
            .expect("Failed to convert base_fee_in_erc20 to u128: value exceeds u128 range");
        Ok(base_fee_in_erc20_u128)
    }

    fn cip64_max_allowed_gas_cost(
        &self,
        evm: &mut CeloEvm<DB, INSP>,
        fee_currency: Option<Address>,
    ) -> Result<u64, ERROR> {
        let ctx = evm.ctx();
        let fee_currency_context = &ctx.chain().fee_currency_context;
        let max_allowed_gas_cost = fee_currency_context
            .max_allowed_currency_intrinsic_gas_cost(fee_currency.unwrap())
            .map_err(|e| ERROR::from_string(e))?;
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
        evm: &mut CeloEvm<DB, INSP>,
        exec_result: &mut FrameResult,
    ) -> Result<(), ERROR> {
        let ctx = evm.ctx();
        let is_deposit = ctx.tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;
        let fee_currency = ctx.tx().fee_currency();
        let fees_in_celo = fee_currency.is_none() || fee_currency.unwrap() == Address::ZERO;
        let is_balance_check_disabled = ctx.cfg().is_balance_check_disabled();

        if is_deposit || fees_in_celo || is_balance_check_disabled {
            return Ok(());
        }

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
        let caller = ctx.tx().caller();

        // Convert costs to fee currency
        let base_fee_in_erc20 = self.cip64_get_base_fee_in_erc20(evm, fee_currency, basefee)?;
        let effective_gas_price = evm.ctx().tx().effective_gas_price(base_fee_in_erc20);
        let tip_gas_price = effective_gas_price
            .checked_sub(base_fee_in_erc20)
            .expect("tip_gas_price is positive because the effective_gas_price was validated before to be greater or equal than the base_fee_in_erc20");

        let tx_fee_tip_in_erc20 = U256::from(
            tip_gas_price.saturating_mul(exec_result.gas().spent_sub_refunded() as u128),
        );

        // Return balance of not spent gas.
        let refund_in_erc20 = U256::from(effective_gas_price.saturating_mul(
            (exec_result.gas().remaining() + exec_result.gas().refunded() as u64) as u128,
        ));

        let base_tx_charge =
            base_fee_in_erc20.saturating_mul(exec_result.gas().spent_sub_refunded() as u128);

        let max_allowed_gas_cost = self
            .cip64_max_allowed_gas_cost(evm, fee_currency)?
            .saturating_sub(
                evm.ctx()
                    .tx()
                    .cip64_tx_info
                    .as_ref()
                    .unwrap()
                    .actual_intrinsic_gas_used,
            );

        let (logs, gas_used) = erc20::credit_gas_fees(
            evm,
            fee_currency.unwrap(),
            caller,
            fee_recipient,
            fee_handler,
            refund_in_erc20,
            tx_fee_tip_in_erc20,
            U256::from(base_tx_charge),
            max_allowed_gas_cost,
        )
        .map_err(|e| ERROR::from_string(format!("Failed to credit gas fees: {e}")))?;

        // Collect logs from the system call to be included in the final receipt
        let mut tx = evm.ctx().tx().clone();
        let old_cip64_tx_info = tx.cip64_tx_info.as_ref().unwrap();
        tx.cip64_tx_info = Some(Cip64Info {
            actual_intrinsic_gas_used: old_cip64_tx_info.actual_intrinsic_gas_used + gas_used,
            logs_pre: old_cip64_tx_info.logs_pre.clone(),
            logs_post: logs,
        });
        evm.ctx().set_tx(tx);
        self.warn_if_gas_cost_exceeds_intrinsic_gas_cost(evm, fee_currency)?;
        Ok(())
    }

    fn warn_if_gas_cost_exceeds_intrinsic_gas_cost(
        &self,
        evm: &mut CeloEvm<DB, INSP>,
        fee_currency: Option<Address>,
    ) -> Result<(), ERROR> {
        let gas_cost = evm
            .ctx()
            .tx()
            .cip64_tx_info
            .as_ref()
            .unwrap()
            .actual_intrinsic_gas_used;
        let intrinsic_gas_cost = evm
            .ctx()
            .chain()
            .fee_currency_context
            .currency_intrinsic_gas_cost(fee_currency)
            .map_err(|e| ERROR::from_string(e))?;

        if gas_cost > intrinsic_gas_cost {
            if gas_cost > intrinsic_gas_cost * 2 {
                info!(
                    target: "celo_handler",
                    "Gas usage for debit+credit exceeds intrinsic gas: {:} > {:}",
                    gas_cost,
                    intrinsic_gas_cost
                );
            } else {
                warn!(
                    target: "celo_handler",
                    "Gas usage for debit+credit exceeds intrinsic gas, within a factor of 2.: {:} > {:}",
                    gas_cost,
                    intrinsic_gas_cost
                );
            }
        }
        Ok(())
    }

    fn cip64_validate_erc20_and_debit_gas_fees(
        &self,
        evm: &mut CeloEvm<DB, INSP>,
    ) -> Result<(), ERROR> {
        let ctx = evm.ctx();
        let tx = ctx.tx();
        let fee_currency = tx.fee_currency();
        let caller_addr = tx.caller();
        let gas_limit = tx.gas_limit();
        let basefee = ctx.block().basefee();

        let fee_currency_context = &ctx.chain().fee_currency_context;

        // For CIP-64 transactions, check ERC20 balance AND debit the erc20 for fees before borrowing caller_account
        // Check if the fee currency is registered
        if fee_currency_context
            .currency_exchange_rate(fee_currency)
            .is_err()
        {
            return Err(ERROR::from_string(
                "unregistered fee-currency address".to_string(),
            ));
        }

        let base_fee_in_erc20 = self.cip64_get_base_fee_in_erc20(evm, fee_currency, basefee)?;
        let effective_gas_price = evm.ctx().tx().effective_gas_price(base_fee_in_erc20);

        // Get ERC20 balance using the erc20 module
        let fee_currency_addr = fee_currency.unwrap();

        let balance = erc20::get_balance(evm, fee_currency_addr, caller_addr)
            .map_err(|e| ERROR::from_string(format!("Failed to get ERC20 balance: {e}")))?;

        let gas_cost = (gas_limit as u128)
            .checked_mul(effective_gas_price)
            .map(|gas_cost| U256::from(gas_cost))
            .ok_or(InvalidTransaction::OverflowPaymentInTransaction)?;

        if balance < gas_cost {
            return Err(InvalidTransaction::LackOfFundForMaxFee {
                fee: Box::new(gas_cost),
                balance: Box::new(balance),
            }
            .into());
        }

        let max_allowed_gas_cost = self.cip64_max_allowed_gas_cost(evm, fee_currency)?;

        // For CIP-64 transactions, gas deduction from fee currency we call the erc20::debit_gas_fees function
        let (logs, gas_used) = erc20::debit_gas_fees(
            evm,
            fee_currency_addr,
            caller_addr,
            gas_cost,
            max_allowed_gas_cost,
        )
        .map_err(|e| ERROR::from_string(format!("Failed to debit gas fees: {e}")))?;

        // Store CIP64 transaction information by modifying the transaction
        let mut tx = evm.ctx().tx().clone();
        tx.cip64_tx_info = Some(Cip64Info {
            actual_intrinsic_gas_used: gas_used,
            logs_pre: logs,
            logs_post: Vec::new(),
        });
        evm.ctx().set_tx(tx);

        Ok(())
    }

    fn validate_celo_initial_tx_gas(
        &self,
        evm: &mut CeloEvm<DB, INSP>,
    ) -> Result<InitialAndFloorGas, ERROR> {
        // Extract needed values first to avoid borrowing conflicts
        let ctx = evm.ctx();
        let fee_currency = ctx.tx().fee_currency();
        let gas_limit = ctx.tx().gas_limit();
        let spec = ctx.cfg().spec();

        let mut gas = calculate_initial_tx_gas_for_tx(ctx.tx(), spec.into_eth_spec());

        if fee_currency.is_some_and(|fc| fc != Address::ZERO) {
            let intrinsic_gas_for_erc20 = ctx
                .chain()
                .fee_currency_context
                .currency_intrinsic_gas_cost(fee_currency)
                .map_err(|e| ERROR::from_string(e))?;
            // Adding only in the initial gas, and not the floor because we never addapted the
            // eip7623 to the cip64 (discussions being taken)
            gas.initial_gas = gas.initial_gas.saturating_add(intrinsic_gas_for_erc20);
        }

        // Additional check to see if limit is big enough to cover initial gas.
        if gas.initial_gas > gas_limit {
            return Err(InvalidTransaction::CallGasCostMoreThanGasLimit {
                gas_limit,
                initial_gas: gas.initial_gas,
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
}

impl<ERROR, DB, INSP> Handler
    for CeloHandler<CeloEvm<DB, INSP>, ERROR, revm::handler::EthFrame<EthInterpreter>>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
    ERROR: EvmTrError<CeloEvm<DB, INSP>> + From<OpTransactionError> + FromStringError + IsTxError,
{
    type Evm = CeloEvm<DB, INSP>;
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

        match CeloTxType::try_from(tx_type).map_err(|e| ERROR::from_string(e.to_string()))? {
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

                let base_fee_in_erc20 =
                    self.cip64_get_base_fee_in_erc20(evm, fee_currency, base_fee)?;
                validate_priority_fee_tx(
                    max_fee,
                    max_priority_fee,
                    Some(base_fee_in_erc20),
                    false,
                )?;
            }
            _ => {
                // Ethereum's tx types will be handled in the "self.mainnet.validate_env(evm)" call below
                // where not only those transactions are validated, but also the block specifics.
            }
        }
        self.mainnet.validate_env(evm)
    }

    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Self::Evm,
    ) -> Result<(), Self::Error> {
        self.load_fee_currency_context(evm)?;

        let ctx = evm.ctx();

        let basefee = ctx.block().basefee() as u128;
        let blob_price = ctx.block().blob_gasprice().unwrap_or_default();
        let is_deposit = ctx.tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;
        let fee_currency = ctx.tx().fee_currency();
        let fees_in_celo = fee_currency.is_none() || fee_currency.unwrap() == Address::ZERO;
        let spec = ctx.cfg().spec();
        let block_number = ctx.block().number();
        let is_balance_check_disabled = ctx.cfg().is_balance_check_disabled();
        let is_eip3607_disabled = ctx.cfg().is_eip3607_disabled();
        let is_nonce_check_disabled = ctx.cfg().is_nonce_check_disabled();

        let mint = if is_deposit {
            ctx.tx().mint().unwrap_or_default()
        } else {
            0
        };

        let mut additional_cost = U256::ZERO;

        // The L1-cost fee is only computed for Optimism non-deposit transactions.
        if !is_deposit && !ctx.cfg().is_fee_charge_disabled() {
            // L1 block info is stored in the context for later use.
            // and it will be reloaded from the database if it is not for the current block.
            if ctx.chain().l1_block_info.l2_block != Some(block_number) {
                ctx.chain_mut().l1_block_info =
                    L1BlockInfo::try_fetch(ctx.db_mut(), block_number, spec)?;
            }

            // account for additional cost of l1 fee and operator fee
            let enveloped_tx = ctx
                .tx()
                .enveloped_tx()
                .expect("all not deposit tx have enveloped tx")
                .clone();

            // compute L1 cost
            additional_cost = ctx
                .chain_mut()
                .l1_block_info
                .calculate_tx_l1_cost(&enveloped_tx, spec);

            // compute operator fee
            if spec.is_enabled_in(OpSpecId::ISTHMUS) {
                let gas_limit = U256::from(ctx.tx().gas_limit());
                let operator_fee_charge =
                    ctx.chain()
                        .l1_block_info
                        .operator_fee_charge(&enveloped_tx, gas_limit, spec);
                additional_cost = additional_cost.saturating_add(operator_fee_charge);
            }
        }

        if !is_balance_check_disabled && !fees_in_celo && !is_deposit {
            self.cip64_validate_erc20_and_debit_gas_fees(evm)?;
        }
        let (tx, journal) = evm.ctx().tx_journal_mut();

        let mut caller_account = journal.load_account_with_code_mut(tx.caller())?.data;

        if !is_deposit {
            // validates account nonce and code
            validate_account_nonce_and_code(
                &caller_account.info,
                tx.nonce(),
                is_eip3607_disabled,
                is_nonce_check_disabled,
            )?;
        }

        let max_balance_spending = tx.max_balance_spending()?.saturating_add(additional_cost);

        // If the transaction is a deposit with a `mint` value, add the mint value
        // in wei to the caller's balance. This should be persisted to the database
        // prior to the rest of execution.
        let mut new_balance = caller_account.info.balance.saturating_add(U256::from(mint));

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
                return Err(ERROR::from_string(format!(
                    "lack of funds ({}) for value payment ({})",
                    new_balance,
                    tx.value()
                )));
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

    fn last_frame_result(
        &mut self,
        evm: &mut Self::Evm,
        frame_result: &mut FrameResult,
    ) -> Result<(), Self::Error> {
        let ctx = evm.ctx();
        let tx = ctx.tx();
        let is_deposit = tx.tx_type() == DEPOSIT_TRANSACTION_TYPE;
        let tx_gas_limit = tx.gas_limit();
        let is_regolith = ctx.cfg().spec().is_enabled_in(OpSpecId::REGOLITH);

        let instruction_result = frame_result.interpreter_result().result;
        let gas = frame_result.gas_mut();
        let remaining = gas.remaining();
        let refunded = gas.refunded();

        // Spend the gas limit. Gas is reimbursed when the tx returns successfully.
        *gas = Gas::new_spent(tx_gas_limit);

        if instruction_result.is_ok() {
            // On Optimism, deposit transactions report gas usage uniquely to other
            // transactions due to them being pre-paid on L1.
            //
            // Hardfork Behavior:
            // - Bedrock (success path):
            //   - Deposit transactions (non-system) report their gas limit as the usage. No
            //     refunds.
            //   - Deposit transactions (system) report 0 gas used. No refunds.
            //   - Regular transactions report gas usage as normal.
            // - Regolith (success path):
            //   - Deposit transactions (all) report their gas used as normal. Refunds enabled.
            //   - Regular transactions report their gas used as normal.
            if !is_deposit || is_regolith {
                // For regular transactions prior to Regolith and all transactions after
                // Regolith, gas is reported as normal.
                gas.erase_cost(remaining);
                gas.record_refund(refunded);
            } else if is_deposit {
                let tx = ctx.tx();
                if tx.is_system_transaction() {
                    // System transactions were a special type of deposit transaction in
                    // the Bedrock hardfork that did not incur any gas costs.
                    gas.erase_cost(tx_gas_limit);
                }
            }
        } else if instruction_result.is_revert() {
            // On Optimism, deposit transactions report gas usage uniquely to other
            // transactions due to them being pre-paid on L1.
            //
            // Hardfork Behavior:
            // - Bedrock (revert path):
            //   - Deposit transactions (all) report the gas limit as the amount of gas used on
            //     failure. No refunds.
            //   - Regular transactions receive a refund on remaining gas as normal.
            // - Regolith (revert path):
            //   - Deposit transactions (all) report the actual gas used as the amount of gas used
            //     on failure. Refunds on remaining gas enabled.
            //   - Regular transactions receive a refund on remaining gas as normal.
            if !is_deposit || is_regolith {
                gas.erase_cost(remaining);
            }
        }
        Ok(())
    }

    fn reimburse_caller(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut FrameResult,
    ) -> Result<(), Self::Error> {
        // For CIP-64 transactions, we need to credit the fee currency all in the same
        // place. We address that in the reward_beneficiary function.
        if evm.ctx().tx().fee_currency().is_none() {
            self.mainnet.reimburse_caller(evm, exec_result)?;
        }

        let context = evm.ctx();
        if context.tx().tx_type() != DEPOSIT_TRANSACTION_TYPE
            && context.tx().fee_currency().is_none()
        {
            let caller = context.tx().caller();
            let spec = context.cfg().spec();
            let operator_fee_refund = context
                .chain()
                .l1_block_info
                .operator_fee_refund(exec_result.gas(), spec);

            // In additional to the normal transaction fee, additionally refund the caller
            // for the operator fee.
            evm.ctx()
                .journal_mut()
                .balance_incr(caller, operator_fee_refund)?;
        }

        Ok(())
    }

    fn refund(&self, evm: &mut Self::Evm, exec_result: &mut FrameResult, eip7702_refund: i64) {
        exec_result.gas_mut().record_refund(eip7702_refund);

        let is_deposit = evm.ctx().tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;
        let is_regolith = evm.ctx().cfg().spec().is_enabled_in(OpSpecId::REGOLITH);

        // Prior to Regolith, deposit transactions did not receive gas refunds.
        let is_gas_refund_disabled = is_deposit && !is_regolith;
        if !is_gas_refund_disabled {
            exec_result.gas_mut().set_final_refund(
                evm.ctx()
                    .cfg()
                    .spec()
                    .into_eth_spec()
                    .is_enabled_in(SpecId::LONDON),
            );
        }
    }

    fn reward_beneficiary(
        &self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        let is_deposit = evm.ctx().tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;

        // Transfer fee to coinbase/beneficiary.
        if is_deposit || evm.ctx().tx().fee_currency().is_some() {
            return Ok(());
        }

        self.mainnet.reward_beneficiary(evm, frame_result)?;
        let basefee = evm.ctx().block().basefee() as u128;

        // If the transaction is not a deposit transaction, fees are paid out
        // to both the Base Fee Vault as well as the L1 Fee Vault.
        let ctx = evm.ctx();
        let enveloped = ctx.tx().enveloped_tx().cloned();
        let spec = ctx.cfg().spec();
        let l1_block_info = &mut ctx.chain_mut().l1_block_info;

        let Some(enveloped_tx) = &enveloped else {
            return Err(ERROR::from_string(
                "[OPTIMISM] Failed to load enveloped transaction.".into(),
            ));
        };

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
        mut frame_result: <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        // Handle context errors
        match core::mem::replace(evm.ctx().error(), Ok(())) {
            Err(revm::context_interface::context::ContextError::Db(e)) => return Err(e.into()),
            Err(revm::context_interface::context::ContextError::Custom(e)) => {
                return Err(ERROR::from_string(e));
            }
            Ok(_) => (),
        }

        // CIP-64: Credit fee currency AFTER reward_beneficiary but BEFORE finalizing result
        // This matches the old revm 24.0 flow where it was called in the `end` function
        // as a separate step after reward_beneficiary
        self.cip64_credit_fee_currency(evm, &mut frame_result)?;

        // Call post_execution::output to get ExecutionResult
        let exec_result = revm::handler::post_execution::output(evm.ctx(), frame_result)
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

        // Commit journal, clear frame stack, clear l1_block_info
        evm.ctx().journal_mut().commit_tx();
        evm.ctx().chain_mut().l1_block_info.clear_tx_l1_cost();
        evm.ctx().local_mut().clear();
        evm.frame_stack().clear();

        Ok(exec_result)
    }

    fn catch_error(
        &self,
        evm: &mut Self::Evm,
        error: Self::Error,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        let is_deposit = evm.ctx().tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;
        let output = if error.is_tx_error() && is_deposit {
            let ctx = evm.ctx();
            let spec = ctx.cfg().spec();
            let tx = ctx.tx();
            let caller = tx.caller();
            let mint = tx.mint();
            let is_system_tx = tx.is_system_transaction();
            let gas_limit = tx.gas_limit();
            let journal = evm.ctx().journal_mut();

            // discard all changes of this transaction
            // Default JournalCheckpoint is the first checkpoint and will wipe all changes.
            journal.checkpoint_revert(JournalCheckpoint::default());

            // If the transaction is a deposit transaction and it failed
            // for any reason, the caller nonce must be bumped, and the
            // gas reported must be altered depending on the Hardfork. This is
            // also returned as a special Halt variant so that consumers can more
            // easily distinguish between a failed deposit and a failed
            // normal transaction.

            // Increment sender nonce and account balance for the mint amount. Deposits
            // always persist the mint amount, even if the transaction fails.
            let mut acc = journal.load_account_mut(caller)?;
            acc.bump_nonce();
            acc.incr_balance(U256::from(mint.unwrap_or_default()));

            // We can now commit the changes.
            journal.commit_tx();

            // The gas used of a failed deposit post-regolith is the gas
            // limit of the transaction. pre-regolith, it is the gas limit
            // of the transaction for non system transactions and 0 for system
            // transactions.
            let gas_used = if spec.is_enabled_in(OpSpecId::REGOLITH) || !is_system_tx {
                gas_limit
            } else {
                0
            };
            // clear the journal
            Ok(ExecutionResult::Halt {
                reason: OpHaltReason::FailedDeposit,
                gas_used,
            })
        } else {
            Err(error)
        };
        // do the cleanup
        evm.ctx().chain_mut().l1_block_info.clear_tx_l1_cost();
        evm.ctx().local_mut().clear();
        evm.frame_stack().clear();

        output
    }
}

impl<ERROR, DB, INSP> InspectorHandler
    for CeloHandler<CeloEvm<DB, INSP>, ERROR, revm::handler::EthFrame<EthInterpreter>>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
    ERROR: EvmTrError<CeloEvm<DB, INSP>> + From<OpTransactionError> + FromStringError + IsTxError,
{
    type IT = EthInterpreter;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CeloBlockEnv, CeloBuilder, CeloContext, DefaultCelo};
    use op_revm::L1BlockInfo;
    use revm::{
        context::{Context, TransactionType},
        context_interface::result::{EVMError, InvalidTransaction},
        database::InMemoryDB,
        database_interface::EmptyDB,
        handler::EthFrame,
        interpreter::{CallOutcome, InstructionResult, InterpreterResult},
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
        assert_eq!(gas.spent(), 10);
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
        assert_eq!(gas.spent(), 10);
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
        assert_eq!(gas.spent(), 10);
        assert_eq!(gas.refunded(), 2); // min(20, 10/5)

        let gas = call_last_frame_return(ctx, InstructionResult::Revert, ret_gas);
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.spent(), 10);
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
        assert_eq!(gas.spent(), 100);
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
        assert_eq!(gas.spent(), 0);
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
            .with_chain(CeloBlockEnv {
                l1_block_info,
                ..CeloBlockEnv::default()
            })
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
            .validate_against_state_and_deduct_caller(&mut evm)
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
            .with_chain(CeloBlockEnv {
                l1_block_info,
                ..CeloBlockEnv::default()
            })
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
            .validate_against_state_and_deduct_caller(&mut evm)
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
            .with_chain(CeloBlockEnv {
                l1_block_info,
                ..CeloBlockEnv::default()
            })
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
            .validate_against_state_and_deduct_caller(&mut evm)
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
            .with_chain(CeloBlockEnv {
                l1_block_info,
                ..CeloBlockEnv::default()
            })
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
            .validate_against_state_and_deduct_caller(&mut evm)
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
            .with_chain(CeloBlockEnv {
                l1_block_info,
                ..CeloBlockEnv::default()
            })
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
            .validate_against_state_and_deduct_caller(&mut evm)
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
            .with_chain(CeloBlockEnv {
                l1_block_info,
                ..CeloBlockEnv::default()
            })
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
            handler.validate_against_state_and_deduct_caller(&mut evm),
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
        evm.ctx().chain.l1_block_info.operator_fee_scalar = Some(U256::from(OP_FEE_MOCK_PARAM));
        evm.ctx().chain.l1_block_info.operator_fee_constant = Some(U256::from(OP_FEE_MOCK_PARAM));

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
            .l1_block_info
            .operator_fee_refund(&gas, OpSpecId::ISTHMUS);
        assert!(op_fee_refund > U256::ZERO);

        if !is_deposit {
            expected_refund += op_fee_refund;
        }

        // Check that the caller was reimbursed the correct amount of ETH.
        let account = evm.ctx().journal_mut().load_account(SENDER).unwrap();
        assert_eq!(account.data.info.balance, expected_refund);
    }
}
