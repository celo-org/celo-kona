//! Block executor for Celo.

pub use executor::CeloBlockExecutor;
pub use executor_factory::CeloBlockExecutorFactory;
pub use receipt_builder::CeloAlloyReceiptBuilder;
pub use upgrade18::{
    CELO_GAS_BRIDGE_L2, Upgrade18Overrides, Upgrade18Param, Upgrade18ParamMissing,
};

pub mod executor;
pub mod executor_factory;
pub mod migration;
pub mod receipt_builder;
pub mod upgrade18;
mod upgrade18_data;

#[cfg(test)]
mod upgrade18_tests {
    use super::*;
    use crate::CeloEvmFactory;
    use alloy_evm::{
        EvmEnv, EvmFactory,
        block::{BlockExecutor, BlockExecutorFactory},
    };
    use alloy_op_evm::OpBlockExecutionCtx;
    use alloy_op_hardforks::OpChainHardforks;
    use alloy_primitives::{Address, U256};
    use revm::database::{CacheDB, EmptyDB, State};

    /// The Upgrade 18 hook runs inside `apply_pre_execution_changes` at the activation
    /// boundary without disturbing the upstream pre-execution flow, and actually plants
    /// the predeploys through the full factory → executor path.
    #[test]
    fn upgrade18_boundary_pre_execution_installs_predeploys() {
        use revm::Database;

        let overrides = upgrade18::Upgrade18Overrides {
            liquidity_controller_owner: Some(Address::with_last_byte(0xaa)),
            celo_token_l1: Some(Address::with_last_byte(0xbb)),
            celo_gas_bridge_l1: Some(Address::with_last_byte(0xcc)),
            native_asset_liquidity_amount: Some(U256::from(1_000_000u64)),
        };
        let executor_factory = CeloBlockExecutorFactory::<CeloAlloyReceiptBuilder, _>::new(
            OpChainHardforks::op_mainnet(),
            CeloEvmFactory::default(),
        )
        .with_upgrade18_time(Some(100))
        .with_upgrade18_overrides(overrides);
        let mut db = State::builder().with_database(CacheDB::<EmptyDB>::default()).build();
        let reported = std::sync::Arc::new(std::sync::Mutex::new(alloc::vec::Vec::new()));
        let reported_by_hook = reported.clone();
        db.set_state_hook(Some(alloc::boxed::Box::new(move |state: revm::state::EvmState| {
            reported_by_hook.lock().unwrap().push(state);
        })));
        let mut env: EvmEnv<op_revm::OpSpecId> = EvmEnv::default();
        env.block_env.timestamp = U256::from(100);
        let evm = executor_factory.evm_factory().create_evm(&mut db, env);
        let mut executor = executor_factory.create_executor(evm, OpBlockExecutionCtx::default());
        executor.apply_pre_execution_changes().expect("boundary pre-execution must succeed");
        drop(executor);

        let marker = db.basic(CELO_GAS_BRIDGE_L2).unwrap().expect("marker account exists");
        assert!(!marker.is_empty_code_hash(), "CeloGasBridgeL2 code must be installed");
        let reported = reported.lock().unwrap();
        let migrations: alloc::vec::Vec<_> =
            reported.iter().filter(|state| state.contains_key(&CELO_GAS_BRIDGE_L2)).collect();
        assert_eq!(migrations.len(), 1, "the DB hook must report migration exactly once");
        assert_eq!(migrations[0][&CELO_GAS_BRIDGE_L2].info.code_hash, marker.code_hash);
    }
}

#[cfg(test)]
mod tests;
