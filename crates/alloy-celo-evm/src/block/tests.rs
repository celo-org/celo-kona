use alloy_consensus::{SignableTransaction, TxLegacy, transaction::Recovered};
use alloy_eips::{Encodable2718, eip2718::WithEncoded};
use alloy_evm::{
    EvmEnv, EvmFactory,
    block::{BlockExecutor, BlockExecutorFactory},
};
use alloy_op_evm::OpBlockExecutionCtx;
use alloy_op_hardforks::OpChainHardforks;
use alloy_primitives::{Address, Signature};
use celo_alloy_consensus::CeloTxEnvelope;
use revm::database::{CacheDB, EmptyDB, State};

use crate::CeloEvmFactory;

use super::*;

/// Wraps a `TxLegacy` in a `CeloTxEnvelope::Legacy` recovered with a zero signer.
fn recovered_legacy(tx: TxLegacy) -> Recovered<CeloTxEnvelope> {
    Recovered::new_unchecked(
        CeloTxEnvelope::Legacy(tx.into_signed(Signature::new(
            Default::default(),
            Default::default(),
            Default::default(),
        ))),
        Address::ZERO,
    )
}

#[test]
fn test_with_encoded() {
    let executor_factory = CeloBlockExecutorFactory::<CeloAlloyReceiptBuilder, _>::new(
        OpChainHardforks::op_mainnet(),
        CeloEvmFactory::default(),
    );
    let mut db = State::builder().with_database(CacheDB::<EmptyDB>::default()).build();
    let evm = executor_factory.evm_factory().create_evm(&mut db, EvmEnv::default());
    let mut executor = executor_factory.create_executor(evm, OpBlockExecutionCtx::default());
    let tx = recovered_legacy(TxLegacy::default());
    let tx_with_encoded = WithEncoded::new(tx.encoded_2718().into(), tx.clone());

    // Make sure we can use both `WithEncoded` and the transaction itself as inputs.
    let _ = executor.execute_transaction(&tx);
    let _ = executor.execute_transaction(&tx_with_encoded);
}

#[test]
fn receipt_builder_uses_evm_cip64_storage() {
    let executor_factory = CeloBlockExecutorFactory::<CeloAlloyReceiptBuilder, _>::new(
        OpChainHardforks::op_mainnet(),
        CeloEvmFactory::default(),
    );
    let mut db = State::builder().with_database(CacheDB::<EmptyDB>::default()).build();
    let evm = executor_factory.evm_factory().create_evm(&mut db, EvmEnv::default());
    let storage = evm.cip64_storage().clone();
    let executor = executor_factory.create_executor(evm, OpBlockExecutionCtx::default());
    let fee_currency = Some(Address::with_last_byte(1));

    storage.store_cip64_info(fee_currency, celo_revm::Cip64Info::default());

    let data = executor
        .receipt_builder
        .cip64_storage
        .pop_cip64_receipt_data()
        .expect("receipt builder must share the EVM's CIP-64 storage");
    assert_eq!(data.fee_currency, fee_currency);
}
