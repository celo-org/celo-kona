//! Abstraction of an executable transaction.

use crate::{CeloTransaction, CeloTxEnvelope, TxCip64};
use alloy_eips::{Encodable2718, Typed2718};
use alloy_evm::{FromRecoveredTx, FromTxWithEncoded, IntoTxEnv};
use alloy_primitives::{Address, Bytes};
use op_alloy_consensus::TxDeposit;
use op_revm::{OpTransaction, transaction::deposit::DepositTransactionParts};
use revm::context::{Transaction, TxEnv};

impl<T: Transaction> IntoTxEnv<Self> for CeloTransaction<T> {
    fn into_tx_env(self) -> Self {
        self
    }
}

impl FromTxWithEncoded<CeloTxEnvelope> for CeloTransaction<TxEnv> {
    fn from_encoded_tx(tx: &CeloTxEnvelope, caller: Address, encoded: Bytes) -> Self {
        let base = match tx {
            CeloTxEnvelope::Legacy(tx) => TxEnv::from_recovered_tx(tx.tx(), caller),
            CeloTxEnvelope::Eip1559(tx) => TxEnv::from_recovered_tx(tx.tx(), caller),
            CeloTxEnvelope::Eip2930(tx) => TxEnv::from_recovered_tx(tx.tx(), caller),
            CeloTxEnvelope::Eip7702(tx) => TxEnv::from_recovered_tx(tx.tx(), caller),
            CeloTxEnvelope::Cip64(tx) => {
                let TxCip64 {
                    chain_id,
                    nonce,
                    gas_limit,
                    to,
                    value,
                    input,
                    max_fee_per_gas,
                    max_priority_fee_per_gas,
                    access_list,
                    fee_currency: _,
                } = tx.tx();
                TxEnv {
                    tx_type: tx.ty(),
                    caller,
                    gas_limit: *gas_limit,
                    gas_price: *max_fee_per_gas,
                    kind: *to,
                    value: *value,
                    data: input.clone(),
                    nonce: *nonce,
                    chain_id: Some(*chain_id),
                    gas_priority_fee: Some(*max_priority_fee_per_gas),
                    access_list: access_list.clone(),
                    ..Default::default()
                }
            }
            CeloTxEnvelope::Deposit(tx) => {
                let TxDeposit {
                    to,
                    value,
                    gas_limit,
                    input,
                    source_hash: _,
                    from: _,
                    mint: _,
                    is_system_transaction: _,
                } = tx.inner();
                TxEnv {
                    tx_type: tx.ty(),
                    caller,
                    gas_limit: *gas_limit,
                    kind: *to,
                    value: *value,
                    data: input.clone(),
                    ..Default::default()
                }
            }
        };

        let deposit = if let CeloTxEnvelope::Deposit(tx) = tx {
            DepositTransactionParts {
                source_hash: tx.source_hash,
                mint: tx.mint,
                is_system_transaction: tx.is_system_transaction,
            }
        } else {
            Default::default()
        };

        let fee_currency: Option<Address> = if let CeloTxEnvelope::Cip64(tx) = tx {
            Some(tx.tx().fee_currency)
        } else {
            None
        };

        Self {
            op_tx: OpTransaction {
                base,
                enveloped_tx: Some(encoded),
                deposit,
            },
            fee_currency,
        }
    }
}

impl FromRecoveredTx<CeloTxEnvelope> for CeloTransaction<TxEnv> {
    fn from_recovered_tx(tx: &CeloTxEnvelope, sender: Address) -> Self {
        let encoded = tx.encoded_2718();
        Self::from_encoded_tx(tx, sender, encoded.into())
    }
}
