//!Handler related to Celo chain

use crate::{
    CeloContext, common::fee_currency_context::FeeCurrencyContext, constants::get_addresses,
    contracts::core_contracts::CoreContractError, contracts::erc20, evm::CeloEvm, transaction::CeloTxTr,
}
use op_revm::{
    L1BlockInfo, OpHaltReason, OpSpecId,
    constants::{L1_FEE_RECIPIENT, OPERATOR_FEE_RECIPIENT},
    handler::IsTxError,
    transaction::{OpTransactionError, OpTxTr, deposit::DEPOSIT_TRANSACTION_TYPE},
};
use revm::{
    Database, Inspector,
    context_interface::{
        Block, Cfg, ContextTr, JournalTr, Transaction,
        result::{ExecutionResult, FromStringError, InvalidTransaction, ResultAndState},
    },
    handler::{
        EvmTr, Frame, FrameResult, Handler, MainnetHandler, handler::EvmTrError,
        pre_execution::validate_account_nonce_and_code,
    },
    inspector::{InspectorFrame, InspectorHandler},
    interpreter::{FrameInput, Gas, interpreter::EthInterpreter},
    primitives::{HashMap, U256, hardfork::SpecId},
    state::Account,
};
use revm_context::LocalContextTr;
use std::{boxed::Box, string::ToString};

pub struct CeloHandler<EVM, ERROR, FRAME> {
    pub mainnet: MainnetHandler<EVM, ERROR, FRAME>,
    // pub _phantom: core::marker::PhantomData<(EVM, ERROR, FRAME)>,
}

impl<EVM, ERROR, FRAME> CeloHandler<EVM, ERROR, FRAME> {
    pub fn new() -> Self {
        Self {
            mainnet: MainnetHandler::default(),
            // _phantom: core::marker::PhantomData,
        }
    }
}

impl<EVM, ERROR, FRAME> Default for CeloHandler<EVM, ERROR, FRAME> {
    fn default() -> Self {
        Self::new()
    }
}

impl<ERROR, FRAME, DB, INSP> Handler for CeloHandler<CeloEvm<DB, INSP>, ERROR, FRAME>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
    ERROR: EvmTrError<CeloEvm<DB, INSP>> + From<OpTransactionError> + FromStringError + IsTxError,
    FRAME: Frame<
            Evm = CeloEvm<DB, INSP>,
            Error = ERROR,
            FrameResult = FrameResult,
            FrameInit = FrameInput,
        >,
{
    type Evm = CeloEvm<DB, INSP>;
    type Error = ERROR;
    type Frame = FRAME;
    type HaltReason = OpHaltReason;

    fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        // Do not perform any extra validation for deposit transactions, they are pre-verified on
        // L1.
        let ctx = evm.ctx();
        let tx = ctx.tx();
        let tx_type = tx.tx_type();
        if tx_type == DEPOSIT_TRANSACTION_TYPE {
            // Do not allow for a system transaction to be processed if Regolith is enabled.
            if tx.is_system_transaction()
                && evm.ctx().cfg().spec().is_enabled_in(OpSpecId::REGOLITH)
            {
                return Err(OpTransactionError::DepositSystemTxPostRegolith.into());
            }
            return Ok(());
        }
        self.mainnet.validate_env(evm)
    }

    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Self::Evm,
    ) -> Result<(), Self::Error> {
        let current_block = evm.ctx().block().number();
        if evm.ctx().chain().fee_currency_context.updated_at_block != Some(current_block) {
            // Update the chain with the new fee currency context
            match FeeCurrencyContext::new_from_evm(evm) {
                Ok(fee_currency_context) => {
                    evm.ctx().chain().fee_currency_context = fee_currency_context;
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

        let ctx = evm.ctx();

        let basefee = ctx.block().basefee() as u128;
        let blob_price = ctx.block().blob_gasprice().unwrap_or_default();
        let is_deposit = ctx.tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;
        let fee_currency = ctx.tx().fee_currency();
        let fees_in_celo = fee_currency.is_none();
        let spec = ctx.cfg().spec();
        let block_number = ctx.block().number();
        let is_balance_check_disabled = ctx.cfg().is_balance_check_disabled();
        let is_eip3607_disabled = ctx.cfg().is_eip3607_disabled();
        let is_nonce_check_disabled = ctx.cfg().is_nonce_check_disabled();
        let mint = ctx.tx().mint();
        let fee_currency_context = ctx.chain().fee_currency_context.clone();

        let mut additional_cost = U256::ZERO;

        // The L1-cost fee is only computed for Optimism non-deposit transactions.
        if !is_deposit {
            // L1 block info is stored in the context for later use.
            // and it will be reloaded from the database if it is not for the current block.
            if ctx.chain().l1_block_info.l2_block != block_number {
                ctx.chain().l1_block_info = L1BlockInfo::try_fetch(ctx.db(), block_number, spec)?;
            }

            // account for additional cost of l1 fee and operator fee
            let enveloped_tx = ctx
                .tx()
                .enveloped_tx()
                .expect("all not deposit tx have enveloped tx")
                .clone();

            // compute L1 cost
            additional_cost = ctx
                .chain()
                .l1_block_info
                .calculate_tx_l1_cost(&enveloped_tx, spec);

            // compute operator fee
            if spec.is_enabled_in(OpSpecId::ISTHMUS) {
                let gas_limit = U256::from(ctx.tx().gas_limit());
                let operator_fee_charge = ctx
                    .chain()
                    .l1_block_info
                    .operator_fee_charge(&enveloped_tx, gas_limit);
                additional_cost = additional_cost.saturating_add(operator_fee_charge);
            }
        }

        // Extract values needed for balance check before borrowing evm
        let (caller_addr, gas_limit, max_fee_per_gas, tx_value, tx_nonce, is_tx_call) = {
            let ctx = evm.ctx();
            let tx = ctx.tx();
            (
                tx.caller(),
                tx.gas_limit(),
                tx.max_fee_per_gas(),
                tx.value(),
                tx.nonce(),
                tx.kind().is_call(),
            )
        };

        // For CIP-64 transactions, check ERC20 balance AND debit the erc20 for fees before borrowing caller_account
        if !fees_in_celo && !is_balance_check_disabled && !is_deposit {
            // Check if the fee currency is registered
            if fee_currency_context
                .currency_exchange_rate(fee_currency)
                .is_err()
            {
                return Err(ERROR::from_string(
                    "unregistered fee-currency address".to_string(),
                ));
            }

            // Get ERC20 balance using the erc20 module
            let fee_currency_addr = fee_currency.unwrap();

            let balance = erc20::get_balance(evm, fee_currency_addr, caller_addr)
                .map_err(|e| ERROR::from_string(format!("Failed to get ERC20 balance: {}", e)))?;

            let gas_cost = (gas_limit as u128)
                .checked_mul(max_fee_per_gas)
                .and_then(|gas_cost| Some(U256::from(gas_cost)))
                .ok_or(InvalidTransaction::OverflowPaymentInTransaction)?;

            if balance < gas_cost {
                return Err(InvalidTransaction::LackOfFundForMaxFee {
                    fee: Box::new(gas_cost),
                    balance: Box::new(balance),
                }
                .into());
            }

            // For CIP-64 transactions, gas deduction from fee currency we call the erc20::debit_gas_fees function
            erc20::debit_gas_fees(evm, fee_currency_addr, caller_addr, gas_cost)
                .map_err(|e| ERROR::from_string(format!("Failed to debit gas fees: {}", e)))?;
        }

        // Now handle all account operations
        let (tx, journal) = evm.ctx().tx_journal();
        let caller_account = journal.load_account_code(caller_addr)?.data;

        // If the transaction is a deposit with a `mint` value, add the mint value
        // in wei to the caller's balance. This should be persisted to the database
        // prior to the rest of execution.
        if is_deposit {
            if let Some(mint) = mint {
                caller_account.info.balance =
                    caller_account.info.balance.saturating_add(U256::from(mint));
            }
            if is_tx_call {
                caller_account.info.nonce = caller_account.info.nonce.saturating_add(1);
            }
        } else {
            // validates account nonce and code
            validate_account_nonce_and_code(
                &mut caller_account.info,
                tx_nonce,
                is_tx_call,
                is_eip3607_disabled,
                is_nonce_check_disabled,
            )?;
        }

        if is_balance_check_disabled {
            // Make sure the caller's balance is at least the value of the transaction.
            // this is not consensus critical, and it is used in testing.
            caller_account.info.balance = caller_account.info.balance.max(tx_value);
        } else if !is_deposit {
            // Check CELO balance for value transfer (value is always in CELO)
            if tx_value > caller_account.info.balance {
                return Err(ERROR::from_string(
                    "lack of funds ({caller_account.info.balance}) for value payment ({tx_value})"
                        .to_string(),
                ));
            }

            // Check balance for gas payment for regular transactions
            if fees_in_celo {
                // Regular transaction: check CELO balance for both value and gas
                let max_balance_spending =
                    tx.max_balance_spending()?.saturating_add(additional_cost);

                if max_balance_spending > caller_account.info.balance {
                    return Err(InvalidTransaction::LackOfFundForMaxFee {
                        fee: Box::new(max_balance_spending),
                        balance: Box::new(caller_account.info.balance),
                    }
                    .into());
                }
            }
        }

        // Handle balance deduction for CELO gas fees
        if !is_balance_check_disabled {
            if fee_currency.is_none() {
                // Only deduct CELO for gas if not using fee currency
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

                caller_account.info.balance = caller_account
                    .info
                    .balance
                    .saturating_sub(op_gas_balance_spending);
            }
            // We are not deducting the tx value (in CELO) from the caller's balance for CIP-64 transactions
            // because it will be deducted later
        }

        // Touch account so we know it is changed.
        caller_account.mark_touch();
        Ok(())
    }

    fn last_frame_result(
        &mut self,
        evm: &mut Self::Evm,
        frame_result: &mut <Self::Frame as Frame>::FrameResult,
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
        exec_result: &mut <Self::Frame as Frame>::FrameResult,
    ) -> Result<(), Self::Error> {
        self.mainnet.reimburse_caller(evm, exec_result)?;

        let context = evm.ctx();
        if context.tx().tx_type() != DEPOSIT_TRANSACTION_TYPE {
            let caller = context.tx().caller();
            let spec = context.cfg().spec();
            let operator_fee_refund = context
                .chain()
                .l1_block_info
                .operator_fee_refund(exec_result.gas(), spec);

            let caller_account = context.journal().load_account(caller)?;

            // In additional to the normal transaction fee, additionally refund the caller
            // for the operator fee.
            caller_account.data.info.balance = caller_account
                .data
                .info
                .balance
                .saturating_add(operator_fee_refund);
        }

        Ok(())
    }

    fn refund(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut <Self::Frame as Frame>::FrameResult,
        eip7702_refund: i64,
    ) {
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
        exec_result: &mut <Self::Frame as Frame>::FrameResult,
    ) -> Result<(), Self::Error> {
        let is_deposit = evm.ctx().tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;

        // Transfer fee to coinbase/beneficiary.
        if !is_deposit {
            self.mainnet.reward_beneficiary(evm, exec_result)?;
            let basefee = evm.ctx().block().basefee() as u128;

            // If the transaction is not a deposit transaction, fees are paid out
            // to both the Base Fee Vault as well as the L1 Fee Vault.
            let ctx = evm.ctx();
            let enveloped = ctx.tx().enveloped_tx().cloned();
            let spec = ctx.cfg().spec();
            let l1_block_info = &mut ctx.chain().l1_block_info;

            let Some(enveloped_tx) = &enveloped else {
                return Err(ERROR::from_string(
                    "[OPTIMISM] Failed to load enveloped transaction.".into(),
                ));
            };

            let l1_cost = l1_block_info.calculate_tx_l1_cost(enveloped_tx, spec);
            let mut operator_fee_cost = U256::ZERO;
            if spec.is_enabled_in(OpSpecId::ISTHMUS) {
                operator_fee_cost = l1_block_info.operator_fee_charge(
                    enveloped_tx,
                    U256::from(exec_result.gas().spent() - exec_result.gas().refunded() as u64),
                );
            }
            // Send the L1 cost of the transaction to the L1 Fee Vault.
            let mut l1_fee_vault_account = ctx.journal().load_account(L1_FEE_RECIPIENT)?;
            l1_fee_vault_account.mark_touch();
            l1_fee_vault_account.info.balance += l1_cost;

            // Send the base fee of the transaction to the FeeHandler.
            let fee_handler = get_addresses(evm.ctx().cfg().chain_id()).fee_handler;
            let mut base_fee_vault_account = evm.ctx().journal().load_account(fee_handler)?;
            base_fee_vault_account.mark_touch();
            base_fee_vault_account.info.balance += U256::from(basefee.saturating_mul(
                (exec_result.gas().spent() - exec_result.gas().refunded() as u64) as u128,
            ));

            // Send the operator fee of the transaction to the coinbase.
            let mut operator_fee_vault_account =
                evm.ctx().journal().load_account(OPERATOR_FEE_RECIPIENT)?;
            operator_fee_vault_account.mark_touch();
            operator_fee_vault_account.data.info.balance += operator_fee_cost;
        }
        Ok(())
    }

    fn output(
        &self,
        evm: &mut Self::Evm,
        result: <Self::Frame as Frame>::FrameResult,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        let result = self.mainnet.output(evm, result)?;
        let result = result.map_haltreason(OpHaltReason::Base);
        if result.result.is_halt() {
            // Post-regolith, if the transaction is a deposit transaction and it halts,
            // we bubble up to the global return handler. The mint value will be persisted
            // and the caller nonce will be incremented there.
            let is_deposit = evm.ctx().tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;
            if is_deposit && evm.ctx().cfg().spec().is_enabled_in(OpSpecId::REGOLITH) {
                return Err(ERROR::from(OpTransactionError::HaltedDepositPostRegolith));
            }
        }
        evm.ctx().chain().l1_block_info.clear_tx_l1_cost();

        Ok(result)
    }

    fn catch_error(
        &self,
        evm: &mut Self::Evm,
        error: Self::Error,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        let is_deposit = evm.ctx().tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;
        let output = if error.is_tx_error() && is_deposit {
            let ctx = evm.ctx();
            let spec = ctx.cfg().spec();
            let tx = ctx.tx();
            let caller = tx.caller();
            let mint = tx.mint();
            let is_system_tx = tx.is_system_transaction();
            let gas_limit = tx.gas_limit();
            // If the transaction is a deposit transaction and it failed
            // for any reason, the caller nonce must be bumped, and the
            // gas reported must be altered depending on the Hardfork. This is
            // also returned as a special Halt variant so that consumers can more
            // easily distinguish between a failed deposit and a failed
            // normal transaction.

            // Increment sender nonce and account balance for the mint amount. Deposits
            // always persist the mint amount, even if the transaction fails.
            let account = {
                let mut acc = Account::from(
                    evm.ctx()
                        .db()
                        .basic(caller)
                        .unwrap_or_default()
                        .unwrap_or_default(),
                );
                acc.info.nonce = acc.info.nonce.saturating_add(1);
                acc.info.balance = acc
                    .info
                    .balance
                    .saturating_add(U256::from(mint.unwrap_or_default()));
                acc.mark_touch();
                acc
            };
            let state = HashMap::from_iter([(caller, account)]);

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
            Ok(ResultAndState {
                result: ExecutionResult::Halt {
                    reason: OpHaltReason::FailedDeposit,
                    gas_used,
                },
                state,
            })
        } else {
            Err(error)
        };
        // do the cleanup
        evm.ctx().chain().l1_block_info.clear_tx_l1_cost();
        evm.ctx().journal().clear();
        evm.ctx().local().clear();

        output
    }
}

impl<ERROR, FRAME, DB, INSP> InspectorHandler for CeloHandler<CeloEvm<DB, INSP>, ERROR, FRAME>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>>,
    ERROR: EvmTrError<CeloEvm<DB, INSP>> + From<OpTransactionError> + FromStringError + IsTxError,
    FRAME: InspectorFrame<
            Evm = CeloEvm<DB, INSP>,
            Error = ERROR,
            FrameResult = FrameResult,
            FrameInit = FrameInput,
            IT = EthInterpreter,
        >,
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
            CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<_, _, _>>::new();

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

        let mut l1_block_info = L1BlockInfo::default();
        l1_block_info.l1_base_fee = U256::from(1_000);
        l1_block_info.l1_fee_overhead = Some(U256::from(1_000));
        l1_block_info.l1_base_fee_scalar = U256::from(1_000);

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

        let handler = CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<_, _, _>>::new();
        handler
            .validate_against_state_and_deduct_caller(&mut evm)
            .unwrap();

        // Check the account balance is updated.
        let account = evm.ctx().journal().load_account(caller).unwrap();
        assert_eq!(account.info.balance, U256::from(1010));
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

        let mut l1_block_info = L1BlockInfo::default();
        l1_block_info.l1_base_fee = U256::from(1_000);
        l1_block_info.l1_fee_overhead = Some(U256::from(1_000));
        l1_block_info.l1_base_fee_scalar = U256::from(1_000);

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

        let handler = CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<_, _, _>>::new();
        handler
            .validate_against_state_and_deduct_caller(&mut evm)
            .unwrap();

        // Check the account balance is updated.
        let account = evm.ctx().journal().load_account(caller).unwrap();
        assert_eq!(account.info.balance, U256::from(1010));
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

        let mut l1_block_info = L1BlockInfo::default();
        l1_block_info.l1_base_fee = U256::from(1_000);
        l1_block_info.l1_fee_overhead = Some(U256::from(1_000));
        l1_block_info.l1_base_fee_scalar = U256::from(1_000);

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
        let handler = CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<_, _, _>>::new();

        // l1block cost is 1048 fee.
        handler
            .validate_against_state_and_deduct_caller(&mut evm)
            .unwrap();

        // Check the account balance is updated.
        let account = evm.ctx().journal().load_account(caller).unwrap();
        assert_eq!(account.info.balance, U256::from(1));
    }

    #[test]
    fn test_remove_operator_cost() {
        let caller = Address::ZERO;
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(151),
                ..Default::default()
            },
        );

        let mut l1_block_info = L1BlockInfo::default();
        l1_block_info.operator_fee_scalar = Some(U256::from(10_000_000));
        l1_block_info.operator_fee_constant = Some(U256::from(50));

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
        let handler = CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<_, _, _>>::new();

        // operator fee cost is operator_fee_scalar * gas_limit / 1e6 + operator_fee_constant
        // 10_000_000 * 10 / 1_000_000 + 50 = 150
        handler
            .validate_against_state_and_deduct_caller(&mut evm)
            .unwrap();

        // Check the account balance is updated.
        let account = evm.ctx().journal().load_account(caller).unwrap();
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

        let mut l1_block_info = L1BlockInfo::default();
        l1_block_info.l1_base_fee = U256::from(1_000);
        l1_block_info.l1_fee_overhead = Some(U256::from(1_000));
        l1_block_info.l1_base_fee_scalar = U256::from(1_000);

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
        let handler = CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<_, _, _>>::new();

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
        let handler = CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<_, _, _>>::new();

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
        let handler = CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<_, _, _>>::new();

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
        let handler = CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<_, _, _>>::new();

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
        let handler = CeloHandler::<_, EVMError<_, OpTransactionError>, EthFrame<_, _, _>>::new();

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
        let account = evm.ctx().journal().load_account(SENDER).unwrap();
        assert_eq!(account.info.balance, expected_refund);
    }
}
