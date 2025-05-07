use revm::context::TxEnv;
use revm::context_interface::transaction::{AccessListItem, SignedAuthorization, Transaction};
use revm::primitives::{Address, Bytes, TxKind, B256, U256};

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CeloTxEnv {
    pub base: TxEnv,
    pub fee_currency: Option<Address>,
}

impl CeloTxEnv {
    /// Calculate the effective gas price for the transaction.
    pub fn effective_gas_price(&self, base_fee: u128) -> u128 {
        self.base.effective_gas_price(base_fee)
    }
}

impl Default for CeloTxEnv {
    fn default() -> Self {
        Self {
            base: TxEnv::default(),
            fee_currency: None,
        }
    }
}

impl Transaction for CeloTxEnv {
    type AccessListItem = AccessListItem;
    type Authorization = SignedAuthorization;

    fn tx_type(&self) -> u8 {
        self.base.tx_type
    }

    fn kind(&self) -> TxKind {
        self.base.kind
    }

    fn caller(&self) -> Address {
        self.base.caller
    }

    fn gas_limit(&self) -> u64 {
        self.base.gas_limit
    }

    fn gas_price(&self) -> u128 {
        self.base.gas_price
    }

    fn value(&self) -> U256 {
        self.base.value
    }

    fn nonce(&self) -> u64 {
        self.base.nonce
    }

    fn chain_id(&self) -> Option<u64> {
        self.base.chain_id
    }

    fn access_list(&self) -> Option<impl Iterator<Item = &Self::AccessListItem>> {
        Some(self.base.access_list.0.iter())
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.base.gas_price
    }

    fn max_fee_per_blob_gas(&self) -> u128 {
        self.base.max_fee_per_blob_gas
    }

    fn authorization_list_len(&self) -> usize {
        self.base.authorization_list.len()
    }

    fn authorization_list(&self) -> impl Iterator<Item = &Self::Authorization> {
        self.base.authorization_list.iter()
    }

    fn input(&self) -> &Bytes {
        &self.base.data
    }

    fn blob_versioned_hashes(&self) -> &[B256] {
        &self.base.blob_hashes
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.base.gas_priority_fee
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::context::{TransactionType, TxEnv};

    fn effective_gas_setup(
        tx_type: TransactionType,
        gas_price: u128,
        gas_priority_fee: Option<u128>,
    ) -> u128 {
        let tx = CeloTxEnv {
            base: TxEnv {
                tx_type: tx_type as u8,
                gas_price,
                gas_priority_fee,
                ..Default::default()
            },
            fee_currency: None,
        };
        let base_fee = 100;
        tx.effective_gas_price(base_fee)
    }

    #[test]
    fn test_effective_gas_price() {
        assert_eq!(90, effective_gas_setup(TransactionType::Legacy, 90, None));
        assert_eq!(
            90,
            effective_gas_setup(TransactionType::Legacy, 90, Some(0))
        );
        assert_eq!(
            90,
            effective_gas_setup(TransactionType::Legacy, 90, Some(10))
        );
        assert_eq!(
            120,
            effective_gas_setup(TransactionType::Legacy, 120, Some(10))
        );
        assert_eq!(90, effective_gas_setup(TransactionType::Eip2930, 90, None));
        assert_eq!(
            90,
            effective_gas_setup(TransactionType::Eip2930, 90, Some(0))
        );
        assert_eq!(
            90,
            effective_gas_setup(TransactionType::Eip2930, 90, Some(10))
        );
        assert_eq!(
            120,
            effective_gas_setup(TransactionType::Eip2930, 120, Some(10))
        );
        assert_eq!(90, effective_gas_setup(TransactionType::Eip1559, 90, None));
        assert_eq!(
            90,
            effective_gas_setup(TransactionType::Eip1559, 90, Some(0))
        );
        assert_eq!(
            90,
            effective_gas_setup(TransactionType::Eip1559, 90, Some(10))
        );
        assert_eq!(
            110,
            effective_gas_setup(TransactionType::Eip1559, 120, Some(10))
        );
        assert_eq!(90, effective_gas_setup(TransactionType::Eip4844, 90, None));
        assert_eq!(
            90,
            effective_gas_setup(TransactionType::Eip4844, 90, Some(0))
        );
        assert_eq!(
            90,
            effective_gas_setup(TransactionType::Eip4844, 90, Some(10))
        );
        assert_eq!(
            110,
            effective_gas_setup(TransactionType::Eip4844, 120, Some(10))
        );
        assert_eq!(90, effective_gas_setup(TransactionType::Eip7702, 90, None));
        assert_eq!(
            90,
            effective_gas_setup(TransactionType::Eip7702, 90, Some(0))
        );
        assert_eq!(
            90,
            effective_gas_setup(TransactionType::Eip7702, 90, Some(10))
        );
        assert_eq!(
            110,
            effective_gas_setup(TransactionType::Eip7702, 120, Some(10))
        );
    }
}
