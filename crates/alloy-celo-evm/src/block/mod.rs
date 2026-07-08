//! Block executor for Celo.

pub use executor::CeloBlockExecutor;
pub use executor_factory::CeloBlockExecutorFactory;
pub use receipt_builder::CeloAlloyReceiptBuilder;
pub use upgrade18::{
    CELO_GAS_BRIDGE_L2, Upgrade18Overrides, Upgrade18Param, Upgrade18ParamMissing,
};

pub mod executor;
pub mod executor_factory;
pub mod receipt_builder;
pub mod upgrade18;
mod upgrade18_data;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CeloEvmFactory;
    use alloy_consensus::{SignableTransaction, TxLegacy, transaction::Recovered};
    use alloy_eips::{Encodable2718, eip2718::WithEncoded};
    use alloy_evm::{
        EvmEnv, EvmFactory,
        block::{BlockExecutor, BlockExecutorFactory},
    };
    use alloy_op_evm::OpBlockExecutionCtx;
    use alloy_op_hardforks::OpChainHardforks;
    use alloy_primitives::{Address, Signature, U256};
    use celo_alloy_consensus::CeloTxEnvelope;
    use revm::database::{CacheDB, EmptyDB, State};

    #[test]
    fn test_with_encoded() {
        let executor_factory = CeloBlockExecutorFactory::<CeloAlloyReceiptBuilder, _>::new(
            OpChainHardforks::op_mainnet(),
            CeloEvmFactory::default(),
        );
        let mut db = State::builder().with_database(CacheDB::<EmptyDB>::default()).build();
        let evm = executor_factory.evm_factory().create_evm(&mut db, EvmEnv::default());
        let mut executor = executor_factory.create_executor(evm, OpBlockExecutionCtx::default());
        let tx = Recovered::new_unchecked(
            CeloTxEnvelope::Legacy(TxLegacy::default().into_signed(Signature::new(
                Default::default(),
                Default::default(),
                Default::default(),
            ))),
            Address::ZERO,
        );
        let tx_with_encoded = WithEncoded::new(tx.encoded_2718().into(), tx.clone());

        // make sure we can use both `WithEncoded` and transaction itself as inputs.
        let _ = executor.execute_transaction(&tx);
        let _ = executor.execute_transaction(&tx_with_encoded);
    }

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
        let mut env: EvmEnv<op_revm::OpSpecId> = EvmEnv::default();
        env.block_env.timestamp = U256::from(100);
        let evm = executor_factory.evm_factory().create_evm(&mut db, env);
        let mut executor = executor_factory.create_executor(evm, OpBlockExecutionCtx::default());
        executor.apply_pre_execution_changes().expect("boundary pre-execution must succeed");
        drop(executor);

        let marker = db.basic(CELO_GAS_BRIDGE_L2).unwrap().expect("marker account exists");
        assert!(!marker.is_empty_code_hash(), "CeloGasBridgeL2 code must be installed");
    }
}
