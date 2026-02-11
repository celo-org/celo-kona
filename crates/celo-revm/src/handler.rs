//!Handler related to Celo chain

use crate::{
    CeloContext,
    constants::get_addresses,
    contracts::erc20,
    evm::CeloEvm,
    fee_currency_context::FeeCurrencyContext,
    transaction::{CeloTxTr, Cip64Info},
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
    context::{LocalContextTr, result::InvalidTransaction},
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
        InitialAndFloorGas, gas::calculate_initial_tx_gas_for_tx, interpreter::EthInterpreter,
    },
    primitives::{U256, hardfork::SpecId},
};
use std::{boxed::Box, format, string::ToString, vec::Vec};
use tracing::{info, warn};

/// Transaction hashes that have wrong chain IDs but were accepted historically.
/// These transactions were included due to a bug in EIP-2930 sender recovery that used
/// tx.ChainId() instead of the network's chain ID. We must accept them during historical
/// sync to avoid a hard fork. See <https://github.com/celo-org/op-geth/issues/454>.
///
/// Each entry contains (tx_hash, network_chain_id) to prevent replay attacks from other networks.
const LEGACY_CHAIN_ID_EXCEPTIONS: [(B256, u64); 2] = [
    // Celo Sepolia block 12531083 - tx had chain_id 11162320 instead of 11142220
    (
        b256!("4564b9903cfe18814ffc2696e1ad141d9cc3a549dc4f5726e15f7be2e0ccaa25"),
        11142220, // Celo Sepolia chain ID
    ),
    // Celo Mainnet block 53619115 - tx had chain_id 44787 instead of 42220
    (
        b256!("d6bdf3261df7e7a4db6bbc486bf091eb62dfd2883e335c31219b6a37d3febca1"),
        42220, // Celo Mainnet chain ID
    ),
];

fn is_legacy_chain_id_exception(enveloped_tx: Option<&[u8]>, network_chain_id: u64) -> bool {
    enveloped_tx.is_some_and(|tx| {
        let tx_hash = keccak256(tx);
        LEGACY_CHAIN_ID_EXCEPTIONS
            .iter()
            .any(|(hash, chain_id)| *hash == tx_hash && *chain_id == network_chain_id)
    })
}

pub struct CeloHandler<EVM, ERROR, FRAME> {
    pub mainnet: MainnetHandler<EVM, ERROR, FRAME>,
    pub op: op_revm::handler::OpHandler<EVM, ERROR, FRAME>,
}

impl<EVM, ERROR, FRAME> CeloHandler<EVM, ERROR, FRAME> {
    pub fn new() -> Self {
        Self {
            mainnet: MainnetHandler::default(),
            op: op_revm::handler::OpHandler::new(),
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
        if evm.fee_currency_context.updated_at_block != Some(current_block) {
            // Update the chain with the new fee currency context.
            // If core contracts are missing, we'll get an empty context (for non-celo test environments).
            let fee_currency_context = FeeCurrencyContext::new_from_evm(evm);
            evm.fee_currency_context = fee_currency_context;

            // Reset warmness after context loading to match op-geth behavior.
            // In op-geth, context loading uses a separate EVM instance, so no warmness
            // is shared with subsequent transaction processing. This affects all
            // transactions, not just CIP-64, ensuring consistent gas accounting.
            self.reset_warmness_to_default(evm);
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
        let fee_currency_context = &evm.fee_currency_context;
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
        let fee_currency_context = &evm.fee_currency_context;
        let max_allowed_gas_cost = fee_currency_context
            .max_allowed_currency_intrinsic_gas_cost(fee_currency.unwrap())
            .map_err(|e| ERROR::from_string(e))?;
        Ok(max_allowed_gas_cost)
    }

    /// Reset account warmness to the default state after fee currency context loading.
    ///
    /// This marks all accounts in the journal state with `AccountStatus::Cold`,
    /// effectively isolating the context loading phase from transaction processing.
    /// This matches op-geth's behavior where context loading uses a separate EVM instance.
    ///
    /// After this reset, warmness is determined by:
    /// - Precompiles: warm (via WarmAddresses.precompile_set)
    /// - Coinbase: warm (via WarmAddresses.coinbase, set later by load_accounts)
    /// - Access list entries: warm (set later by load_accounts from tx access list)
    /// - All other accounts: cold (first access costs 2600 gas)
    ///
    /// This is called right after context loading, before any transaction processing
    /// (including CIP-64 debit). The sender account becomes cold here, but:
    /// - For CIP-64 debit/credit: sender is not directly accessed, only the fee
    ///   currency contract is called (which reads sender's balance from its storage)
    /// - For main transaction: load_accounts() runs after debit and sets up the
    ///   access list properly, warming the sender
    fn reset_warmness_to_default(&self, evm: &mut CeloEvm<DB, INSP>) {
        use revm::state::AccountStatus;

        // Mark all loaded accounts as cold
        for account in evm.ctx().journal_mut().state.values_mut() {
            account.status |= AccountStatus::Cold;
        }
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
            refund_in_erc20,
            tx_fee_tip_in_erc20,
            U256::from(base_tx_charge),
            max_allowed_gas_cost,
        )
        .map_err(|e| ERROR::from_string(format!("Failed to credit gas fees: {e}")))?;

        // Collect logs from the system call to be included in the final receipt
        let mut tx = evm.ctx().tx().clone();
        let info = tx.cip64_tx_info.as_mut().unwrap();
        info.credit_gas_used = credit_gas_used;
        info.credit_gas_refunded = credit_gas_refunded;
        info.logs_post = logs;
        evm.ctx().set_tx(tx);
        self.log_and_warn_gas_cost(evm, fee_currency)?;
        Ok(())
    }

    /// Logs a summary of the CIP-64 gas costs and warns if they exceed the intrinsic gas cost.
    fn log_and_warn_gas_cost(
        &self,
        evm: &mut CeloEvm<DB, INSP>,
        fee_currency: Option<Address>,
    ) -> Result<(), ERROR> {
        let cip64_info = evm.ctx().tx().cip64_tx_info.clone().unwrap();

        let intrinsic_gas_cost = evm
            .fee_currency_context
            .currency_intrinsic_gas_cost(fee_currency)
            .map_err(|e| ERROR::from_string(e))?;

        // Log the gas summary for debugging and verification
        // gas_used + gas_refunded gives the raw gas before refunds (what op-geth calls gasUsed)
        info!(
            target: "celo_handler",
            "CIP-64 gas summary: fee_currency={:?}, \
            debit(gas_used={}, gas_refunded={}), \
            credit(gas_used={}, gas_refunded={}), \
            intrinsic_gas={}",
            fee_currency,
            cip64_info.debit_gas_used,
            cip64_info.debit_gas_refunded,
            cip64_info.credit_gas_used,
            cip64_info.credit_gas_refunded,
            intrinsic_gas_cost
        );

        // Compare raw gas (before refunds) against intrinsic gas limit
        let total_raw_gas = cip64_info.debit_gas_used
            + cip64_info.debit_gas_refunded
            + cip64_info.credit_gas_used
            + cip64_info.credit_gas_refunded;
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
        evm: &mut CeloEvm<DB, INSP>,
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
            return Err(ERROR::from_string(
                "unregistered fee-currency address".to_string(),
            ));
        }

        let base_fee_in_erc20 = self.cip64_get_base_fee_in_erc20(evm, fee_currency, basefee)?;
        let effective_gas_price = evm.ctx().tx().effective_gas_price(base_fee_in_erc20);

        // Get ERC20 balance using the erc20 module
        let fee_currency_addr = fee_currency.unwrap();

        let gas_cost = (gas_limit as u128)
            .checked_mul(effective_gas_price)
            .map(|gas_cost| U256::from(gas_cost))
            .ok_or(InvalidTransaction::OverflowPaymentInTransaction)?;

        let max_allowed_gas_cost = self.cip64_max_allowed_gas_cost(evm, fee_currency)?;

        // For CIP-64 transactions, gas deduction from fee currency we call the erc20::debit_gas_fees function
        // Note: Warmness was already reset in load_fee_currency_context() after context loading,
        // so accounts warmed during context loading are now cold.
        let (logs, debit_gas_used, debit_gas_refunded) = erc20::debit_gas_fees(
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
            debit_gas_used,
            debit_gas_refunded,
            credit_gas_used: 0,
            credit_gas_refunded: 0,
            logs_pre: logs,
            logs_post: Vec::new(),
        });
        // Store the effective gas price for the GASPRICE opcode.
        // This is calculated using the base fee converted to the fee currency.
        tx.effective_gas_price = Some(effective_gas_price);
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
            let intrinsic_gas_for_erc20 = evm
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

        // Check for legacy chain ID exception before mainnet validation.
        // These are historical transactions that were accepted with wrong chain IDs due to a bug.
        // Only compute the tx hash if there's actually a chain ID mismatch to avoid unnecessary hashing.
        let chain_id_mismatch = evm
            .ctx()
            .tx()
            .chain_id()
            .is_some_and(|tx_chain_id| tx_chain_id != evm.ctx().cfg().chain_id());
        let network_chain_id = evm.ctx().cfg().chain_id();
        if chain_id_mismatch
            && is_legacy_chain_id_exception(
                evm.ctx().tx().enveloped_tx().map(|b| b.as_ref()),
                network_chain_id,
            )
        {
            // Temporarily disable chain ID check for these historical exception txs,
            // preserving the original value to restore afterward
            let original_tx_chain_id_check = evm.ctx().cfg().tx_chain_id_check;
            evm.ctx().modify_cfg(|cfg| cfg.tx_chain_id_check = false);
            let result = self.mainnet.validate_env(evm);
            evm.ctx()
                .modify_cfg(|cfg| cfg.tx_chain_id_check = original_tx_chain_id_check);
            return result;
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
            if ctx.chain().l2_block != Some(block_number) {
                *ctx.chain_mut() = L1BlockInfo::try_fetch(ctx.db_mut(), block_number, spec)?;
            }

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
        self.op.last_frame_result(evm, frame_result)
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
        let l1_block_info = &mut ctx.chain_mut();

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
        assert!(is_legacy_chain_id_exception(Some(&encoded), 11142220)); // Celo Sepolia
        // Should not match on wrong network
        assert!(!is_legacy_chain_id_exception(Some(&encoded), 42220)); // Celo Mainnet
    }

    #[test]
    fn test_legacy_chain_id_exception_mainnet_tx_hash() {
        // Verify the encoded tx hashes to the expected exception hash
        let encoded = build_mainnet_exception_tx();
        assert_eq!(keccak256(&encoded), LEGACY_CHAIN_ID_EXCEPTIONS[1].0);
        assert!(is_legacy_chain_id_exception(Some(&encoded), 42220)); // Celo Mainnet
        // Should not match on wrong network
        assert!(!is_legacy_chain_id_exception(Some(&encoded), 11142220)); // Celo Sepolia
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
}
