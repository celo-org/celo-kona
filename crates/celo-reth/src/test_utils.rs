//! Shared test utilities for `celo-reth` unit tests.

use crate::pool::CeloPoolTx;
use alloy_primitives::{Address, Signature, U256};
use celo_alloy_consensus::{CeloPooledTransaction, CeloTxEnvelope, TxCip64};
use reth_optimism_txpool::OpPooledTransaction;
use reth_primitives_traits::Recovered;
use reth_transaction_pool::PoolTransaction;

/// Inner OP pool transaction type used in tests.
pub(crate) type TestInnerPoolTx =
    OpPooledTransaction<crate::primitives::CeloTransactionSigned, CeloPooledTransaction>;

/// Create a test [`CeloPoolTx`] with configurable fields and nonce 0.
pub(crate) fn make_test_tx(
    fee_currency: Option<Address>,
    gas_limit: u64,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    sender: Address,
) -> CeloPoolTx {
    make_test_tx_with_nonce(
        fee_currency,
        0,
        gas_limit,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        sender,
    )
}

/// Create a test [`CeloPoolTx`] with configurable fields.
///
/// The signature is a fixed constant (`Signature::test_signature`), so the tx
/// hash is derived from the payload fields alone — two txs built with identical
/// arguments collide; vary at least one field per tx when inserting several
/// into one pool.
pub(crate) fn make_test_tx_with_nonce(
    fee_currency: Option<Address>,
    nonce: u64,
    gas_limit: u64,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    sender: Address,
) -> CeloPoolTx {
    let tx = fee_currency.map_or_else(
        || {
            let eip1559 = alloy_consensus::TxEip1559 {
                chain_id: 42220,
                nonce,
                gas_limit,
                max_fee_per_gas,
                max_priority_fee_per_gas,
                to: alloy_primitives::TxKind::Call(Address::ZERO),
                value: U256::ZERO,
                access_list: Default::default(),
                input: Default::default(),
            };
            CeloTxEnvelope::Eip1559(alloy_consensus::Signed::new_unhashed(
                eip1559,
                Signature::test_signature(),
            ))
        },
        |fc| {
            let cip64 = TxCip64 {
                chain_id: 42220,
                nonce,
                gas_limit,
                max_fee_per_gas,
                max_priority_fee_per_gas,
                to: alloy_primitives::TxKind::Call(Address::ZERO),
                value: U256::ZERO,
                access_list: Default::default(),
                input: Default::default(),
                fee_currency: Some(fc),
            };
            CeloTxEnvelope::Cip64(alloy_consensus::Signed::new_unhashed(
                cip64,
                Signature::test_signature(),
            ))
        },
    );

    let recovered = Recovered::new_unchecked(tx, sender);
    let pooled = CeloPooledTransaction::try_from(recovered.into_inner()).unwrap();
    let inner = TestInnerPoolTx::from_pooled(Recovered::new_unchecked(pooled, sender));
    CeloPoolTx::new(inner)
}
