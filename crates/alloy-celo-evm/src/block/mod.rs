//! Block executor for Celo.

pub use receipt_builder::CeloAlloyReceiptBuilder;

pub mod receipt_builder;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CeloEvmFactory;
    use alloy_consensus::{SignableTransaction, TxLegacy, transaction::Recovered};
    use alloy_eips::{Encodable2718, eip2718::WithEncoded};
    use alloy_evm::{EvmEnv, EvmFactory, block::BlockExecutor, block::BlockExecutorFactory};
    use alloy_op_evm::{OpBlockExecutionCtx, OpBlockExecutorFactory};
    use alloy_op_hardforks::OpChainHardforks;
    use alloy_primitives::{Address, Signature};
    use celo_alloy::CeloTxEnvelope;
    use revm::database::{CacheDB, EmptyDB, State};

    #[test]
    fn test_with_encoded() {
        let executor_factory = OpBlockExecutorFactory::new(
            CeloAlloyReceiptBuilder::default(),
            OpChainHardforks::op_mainnet(),
            CeloEvmFactory::default(),
        );
        let mut db = State::builder()
            .with_database(CacheDB::<EmptyDB>::default())
            .build();
        let evm = executor_factory
            .evm_factory()
            .create_evm(&mut db, EvmEnv::default());
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
}
