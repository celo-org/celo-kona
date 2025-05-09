use auto_impl::auto_impl;
use op_revm::{OpTransaction, transaction::OpTxTr};
use revm::{
    context::TxEnv,
    context_interface::transaction::Transaction,
    handler::SystemCallTx,
    primitives::{Address, B256, Bytes, TxKind, U256},
};

pub const CIP64_TRANSACTION_TYPE: u8 = 0x7b;

#[auto_impl(&, &mut, Box, Arc)]
pub trait CeloTxTr: OpTxTr {
    fn fee_currency(&self) -> Option<Address>;

    /// Returns `true` if transaction is of type [`CIP64_TRANSACTION_TYPE`].
    fn is_cip64(&self) -> bool {
        self.tx_type() == CIP64_TRANSACTION_TYPE
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CeloTransaction<T: Transaction> {
    pub op_tx: OpTransaction<T>,
    pub fee_currency: Option<Address>,
}

impl<T: Transaction> CeloTransaction<T> {
    pub fn new(op_tx: OpTransaction<T>) -> Self {
        Self {
            op_tx,
            fee_currency: None,
        }
    }
}

impl Default for CeloTransaction<TxEnv> {
    fn default() -> Self {
        Self {
            op_tx: OpTransaction::default(),
            fee_currency: None,
        }
    }
}

impl<TX: Transaction + SystemCallTx> SystemCallTx for CeloTransaction<TX> {
    fn new_system_tx(data: Bytes, system_contract_address: Address) -> Self {
        CeloTransaction::new(OpTransaction::new(TX::new_system_tx(
            data,
            system_contract_address,
        )))
    }
}

impl<T: Transaction> Transaction for CeloTransaction<T> {
    type AccessListItem = T::AccessListItem;
    type Authorization = T::Authorization;

    fn tx_type(&self) -> u8 {
        self.op_tx.tx_type()
    }

    fn caller(&self) -> Address {
        self.op_tx.caller()
    }

    fn gas_limit(&self) -> u64 {
        self.op_tx.gas_limit()
    }

    fn value(&self) -> U256 {
        self.op_tx.value()
    }

    fn input(&self) -> &Bytes {
        self.op_tx.input()
    }

    fn nonce(&self) -> u64 {
        self.op_tx.nonce()
    }

    fn kind(&self) -> TxKind {
        self.op_tx.kind()
    }

    fn chain_id(&self) -> Option<u64> {
        self.op_tx.chain_id()
    }

    fn access_list(&self) -> Option<impl Iterator<Item = &Self::AccessListItem>> {
        self.op_tx.access_list()
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.op_tx.max_priority_fee_per_gas()
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.op_tx.max_fee_per_gas()
    }

    fn gas_price(&self) -> u128 {
        self.op_tx.gas_price()
    }

    fn blob_versioned_hashes(&self) -> &[B256] {
        self.op_tx.blob_versioned_hashes()
    }

    fn max_fee_per_blob_gas(&self) -> u128 {
        self.op_tx.max_fee_per_blob_gas()
    }

    fn effective_gas_price(&self, base_fee: u128) -> u128 {
        self.op_tx.effective_gas_price(base_fee)
    }

    fn authorization_list_len(&self) -> usize {
        self.op_tx.authorization_list_len()
    }

    fn authorization_list(&self) -> impl Iterator<Item = &Self::Authorization> {
        self.op_tx.authorization_list()
    }
}

impl<T: Transaction> OpTxTr for CeloTransaction<T> {
    fn enveloped_tx(&self) -> Option<&Bytes> {
        self.op_tx.enveloped_tx()
    }

    fn source_hash(&self) -> Option<B256> {
        self.op_tx.source_hash()
    }

    fn mint(&self) -> Option<u128> {
        self.op_tx.mint()
    }

    fn is_system_transaction(&self) -> bool {
        self.op_tx.is_system_transaction()
    }
}

impl<T: Transaction> CeloTxTr for CeloTransaction<T> {
    fn fee_currency(&self) -> Option<Address> {
        self.fee_currency
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use op_revm::transaction::deposit::DepositTransactionParts;
    use revm::primitives::Address;

    #[test]
    fn test_cip64_transaction_fields() {
        let cip64_tx = CeloTransaction {
            op_tx: OpTransaction {
                base: TxEnv {
                    tx_type: CIP64_TRANSACTION_TYPE,
                    gas_limit: 10,
                    gas_price: 100,
                    gas_priority_fee: Some(5),
                    ..Default::default()
                },
                enveloped_tx: None,
                deposit: DepositTransactionParts::default(),
            },
            fee_currency: Some(Address::with_last_byte(1)),
        };
        // Verify transaction type
        assert_eq!(cip64_tx.tx_type(), CIP64_TRANSACTION_TYPE);
        // Verify common fields access
        assert_eq!(cip64_tx.gas_limit(), 10);
        assert_eq!(
            cip64_tx.kind(),
            revm::primitives::TxKind::Call(Address::ZERO)
        );
        // Verify gas related calculations
        assert_eq!(cip64_tx.effective_gas_price(90), 95);
        assert_eq!(cip64_tx.max_fee_per_gas(), 100);
        // Verify CIP-64 fields
        assert_eq!(cip64_tx.fee_currency(), Some(Address::with_last_byte(1)));
    }
}
