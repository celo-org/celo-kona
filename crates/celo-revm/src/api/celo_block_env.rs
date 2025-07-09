use crate::{
    CeloContext,
    common::fee_currency_context::FeeCurrencyContext,
    core_contracts::{CoreContractError, get_currencies, get_exchange_rates, get_intrinsic_gas},
    evm::CeloEvm,
};
use op_revm::L1BlockInfo;
use revm::{
    Database,
    context_interface::{Block, ContextTr},
    handler::EvmTr,
    inspector::Inspector,
};

#[derive(Debug, Clone, Default)]
pub struct CeloBlockEnv {
    pub l1_block_info: L1BlockInfo,
    pub fee_currency_context: FeeCurrencyContext,
}

impl CeloBlockEnv {
    /// Return a new [CeloBlockEnv] with updated exchange rates and intrinsic gas for all fee
    /// currencies in the FeeCurrencyDirectory.
    pub fn update_fee_currencies<DB, INSP>(
        evm: &mut CeloEvm<DB, INSP>,
    ) -> Result<CeloBlockEnv, CoreContractError>
    where
        DB: Database,
        INSP: Inspector<CeloContext<DB>>,
    {
        let currencies = &get_currencies(evm)?;
        let exchange_rates = get_exchange_rates(evm, currencies)?;
        let intrinsic_gas = get_intrinsic_gas(evm, currencies)?;
        let current_block_number = evm.ctx().block().number();
        let fee_currency_context =
            FeeCurrencyContext::new(exchange_rates, intrinsic_gas, current_block_number);
        Ok(CeloBlockEnv {
            l1_block_info: evm.ctx().chain.l1_block_info.clone(),
            fee_currency_context,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CeloBuilder, DefaultCelo, core_contracts::tests::make_celo_test_db};
    use alloy_primitives::{U256, address};
    use revm::Context;

    #[test]
    fn test_update_block_env() {
        let ctx = Context::celo().with_db(make_celo_test_db());
        let mut evm = ctx.build_celo();
        let block_env = CeloBlockEnv::update_fee_currencies(&mut evm).unwrap();

        let exchange_rate = block_env
            .fee_currency_context
            .currency_exchange_rate(Some(address!("0x1111111111111111111111111111111111111111")))
            .unwrap();
        assert_eq!(exchange_rate, (U256::from(20), U256::from(10)));

        let intrinsic_gas_cost = block_env
            .fee_currency_context
            .currency_intrinsic_gas_cost(Some(address!(
                "0x1111111111111111111111111111111111111111"
            )))
            .unwrap();
        assert_eq!(intrinsic_gas_cost, U256::from(50000));

        // Verify that updated_at_block is set to the current block number
        assert_eq!(
            block_env.fee_currency_context.updated_at_block,
            evm.ctx().block().number()
        );
    }
}
