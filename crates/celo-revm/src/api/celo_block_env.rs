use crate::{
    CeloContext,
    core_contracts::{CoreContractError, get_currencies, get_exchange_rates, get_intrinsic_gas},
    evm::CeloEvm,
};
use alloy_primitives::{Address, U256, map::HashMap};
use op_revm::L1BlockInfo;
use revm::{Database, handler::EvmTr};

#[derive(Debug, Clone, Default)]
pub struct CeloBlockEnv {
    pub l1_block_info: L1BlockInfo,
    pub exchange_rates: HashMap<Address, (U256, U256)>,
    pub intrinsic_gas: HashMap<Address, U256>,
}

impl CeloBlockEnv {
    /// Return a new [CeloBlockEnv] with updated exchange rates and intrinsic gas for all fee
    /// currencies in the FeeCurrencyDirectory.
    pub fn update_fee_currencies<DB, INSP>(
        evm: &mut CeloEvm<CeloContext<DB>, INSP>,
    ) -> Result<CeloBlockEnv, CoreContractError>
    where
        DB: Database,
    {
        let currencies = &get_currencies(evm)?;
        let exchange_rates = get_exchange_rates(evm, currencies)?;
        let intrinsic_gas = get_intrinsic_gas(evm, currencies)?;
        Ok(CeloBlockEnv {
            l1_block_info: evm.ctx().chain.l1_block_info.clone(),
            exchange_rates,
            intrinsic_gas,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CeloBuilder, DefaultCelo, core_contracts::tests::make_celo_test_db};
    use alloy_primitives::{address, map::DefaultHashBuilder};
    use revm::Context;

    #[test]
    fn test_update_block_env() {
        let ctx = Context::celo().with_db(make_celo_test_db());
        let mut evm = ctx.build_celo();
        let block_env = CeloBlockEnv::update_fee_currencies(&mut evm).unwrap();

        let mut expected = HashMap::with_hasher(DefaultHashBuilder::default());
        _ = expected.insert(
            address!("0x1111111111111111111111111111111111111111"),
            (U256::from(20), U256::from(10)),
        );
        assert_eq!(block_env.exchange_rates, expected);

        let mut expected = HashMap::with_hasher(DefaultHashBuilder::default());
        _ = expected.insert(
            address!("0x1111111111111111111111111111111111111111"),
            U256::from(50000),
        );
        assert_eq!(block_env.intrinsic_gas, expected);
    }
}
