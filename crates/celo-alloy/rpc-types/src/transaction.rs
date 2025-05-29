//! Celo specific types related to transactions.

use alloy_consensus::{Transaction as _, Typed2718};
use alloy_eips::{eip2930::AccessList, eip7702::SignedAuthorization};
use alloy_primitives::{Address, B256, BlockHash, Bytes, ChainId, TxKind, U256};
use celo_alloy_consensus::CeloTxEnvelope;
use serde::{Deserialize, Serialize};

mod request;
pub use request::OpTransactionRequest;

/// Celo Transaction type
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, derive_more::Deref, derive_more::DerefMut,
)]
#[serde(
    try_from = "tx_serde::CeloTransactionSerdeHelper",
    into = "tx_serde::CeloTransactionSerdeHelper"
)]
#[cfg_attr(
    all(any(test, feature = "arbitrary"), feature = "k256"),
    derive(arbitrary::Arbitrary)
)]
pub struct CeloTransaction {
    /// Ethereum Transaction Types
    #[deref]
    #[deref_mut]
    pub inner: alloy_rpc_types_eth::Transaction<CeloTxEnvelope>,

    /// Nonce for deposit transactions. Only present in RPC responses.
    pub deposit_nonce: Option<u64>,

    /// Deposit receipt version for deposit transactions post-canyon
    pub deposit_receipt_version: Option<u64>,

    /// Address of the whitelisted currency to be used to pay for gas.
    pub fee_currency: Option<Address>,
}

impl Typed2718 for CeloTransaction {
    fn ty(&self) -> u8 {
        self.inner.ty()
    }
}

impl alloy_consensus::Transaction for CeloTransaction {
    fn chain_id(&self) -> Option<ChainId> {
        self.inner.chain_id()
    }

    fn nonce(&self) -> u64 {
        self.inner.nonce()
    }

    fn gas_limit(&self) -> u64 {
        self.inner.gas_limit()
    }

    fn gas_price(&self) -> Option<u128> {
        self.inner.gas_price()
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.inner.max_fee_per_gas()
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.inner.max_priority_fee_per_gas()
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        self.inner.max_fee_per_blob_gas()
    }

    fn priority_fee_or_price(&self) -> u128 {
        self.inner.priority_fee_or_price()
    }

    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        self.inner.effective_gas_price(base_fee)
    }

    fn is_dynamic_fee(&self) -> bool {
        self.inner.is_dynamic_fee()
    }

    fn kind(&self) -> TxKind {
        self.inner.kind()
    }

    fn is_create(&self) -> bool {
        self.inner.is_create()
    }

    fn to(&self) -> Option<Address> {
        self.inner.to()
    }

    fn value(&self) -> U256 {
        self.inner.value()
    }

    fn input(&self) -> &Bytes {
        self.inner.input()
    }

    fn access_list(&self) -> Option<&AccessList> {
        self.inner.access_list()
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        self.inner.blob_versioned_hashes()
    }

    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        self.inner.authorization_list()
    }
}

impl alloy_network_primitives::TransactionResponse for CeloTransaction {
    fn tx_hash(&self) -> alloy_primitives::TxHash {
        self.inner.tx_hash()
    }

    fn block_hash(&self) -> Option<BlockHash> {
        self.inner.block_hash()
    }

    fn block_number(&self) -> Option<u64> {
        self.inner.block_number()
    }

    fn transaction_index(&self) -> Option<u64> {
        self.inner.transaction_index()
    }

    fn from(&self) -> Address {
        self.inner.from()
    }
}

impl AsRef<CeloTxEnvelope> for CeloTransaction {
    fn as_ref(&self) -> &CeloTxEnvelope {
        self.inner.as_ref()
    }
}

mod tx_serde {
    //! Helper module for serializing and deserializing [`CeloTransaction`].
    //!
    //! This is needed because we might need to deserialize the `from` field into both
    //! [`alloy_consensus::transaction::Recovered::signer`] which resides in
    //! [`alloy_rpc_types_eth::Transaction::inner`] and `op_alloy_consensus::TxDeposit::from`.
    //!
    //! Additionaly, we need similar logic for the `gasPrice` field
    use super::*;
    use alloy_consensus::transaction::Recovered;
    use serde::de::Error;

    /// Helper struct which will be flattened into the transaction and will only contain `from`
    /// field if inner [`CeloTxEnvelope`] did not consume it.
    #[derive(Serialize, Deserialize)]
    struct OptionalFields {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<Address>,
        #[serde(
            default,
            rename = "gasPrice",
            skip_serializing_if = "Option::is_none",
            with = "alloy_serde::quantity::opt"
        )]
        effective_gas_price: Option<u128>,
        #[serde(
            default,
            rename = "nonce",
            skip_serializing_if = "Option::is_none",
            with = "alloy_serde::quantity::opt"
        )]
        deposit_nonce: Option<u64>,
        #[serde(
            default,
            rename = "feeCurrency",
            skip_serializing_if = "Option::is_none"
        )]
        fee_currency: Option<Address>,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub(crate) struct CeloTransactionSerdeHelper {
        #[serde(flatten)]
        inner: CeloTxEnvelope,
        #[serde(default)]
        block_hash: Option<BlockHash>,
        #[serde(default, with = "alloy_serde::quantity::opt")]
        block_number: Option<u64>,
        #[serde(default, with = "alloy_serde::quantity::opt")]
        transaction_index: Option<u64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "alloy_serde::quantity::opt"
        )]
        deposit_receipt_version: Option<u64>,

        #[serde(flatten)]
        other: OptionalFields,
    }

    impl From<CeloTransaction> for CeloTransactionSerdeHelper {
        fn from(value: CeloTransaction) -> Self {
            let CeloTransaction {
                inner:
                    alloy_rpc_types_eth::Transaction {
                        inner,
                        block_hash,
                        block_number,
                        transaction_index,
                        effective_gas_price,
                    },
                deposit_receipt_version,
                deposit_nonce,
                fee_currency,
            } = value;

            // if inner transaction is a deposit, then don't serialize `from` directly
            let from = if matches!(inner.inner(), CeloTxEnvelope::Deposit(_)) {
                None
            } else {
                Some(inner.signer())
            };

            // if inner transaction has its own `gasPrice` don't serialize it in this struct.
            let effective_gas_price = effective_gas_price.filter(|_| inner.gas_price().is_none());

            Self {
                inner: inner.into_inner(),
                block_hash,
                block_number,
                transaction_index,
                deposit_receipt_version,
                other: OptionalFields {
                    from,
                    effective_gas_price,
                    deposit_nonce,
                    fee_currency,
                },
            }
        }
    }

    impl TryFrom<CeloTransactionSerdeHelper> for CeloTransaction {
        type Error = serde_json::Error;

        fn try_from(value: CeloTransactionSerdeHelper) -> Result<Self, Self::Error> {
            let CeloTransactionSerdeHelper {
                inner,
                block_hash,
                block_number,
                transaction_index,
                deposit_receipt_version,
                other,
            } = value;

            // Try to get `from` field from inner envelope or from `MaybeFrom`, otherwise return
            // error
            let from = if let Some(from) = other.from {
                from
            } else {
                match &inner {
                    CeloTxEnvelope::Deposit(tx) => tx.from,
                    _ => {
                        return Err(serde_json::Error::custom("missing `from` field"));
                    }
                }
            };

            // Only serialize deposit_nonce if inner transaction is deposit to avoid duplicated keys
            let deposit_nonce = other.deposit_nonce.filter(|_| inner.is_deposit());

            let effective_gas_price = other.effective_gas_price.or(inner.gas_price());

            Ok(Self {
                inner: alloy_rpc_types_eth::Transaction {
                    inner: Recovered::new_unchecked(inner, from),
                    block_hash,
                    block_number,
                    transaction_index,
                    effective_gas_price,
                },
                deposit_receipt_version,
                deposit_nonce,
                fee_currency: other.fee_currency,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn can_deserialize_deposit() {
        // cast rpc eth_getTransactionByHash
        // 0xbc9329afac05556497441e2b3ee4c5d4da7ca0b2a4c212c212d0739e94a24df9 --rpc-url optimism
        let rpc_tx = r#"{"blockHash":"0x9d86bb313ebeedf4f9f82bf8a19b426be656a365648a7c089b618771311db9f9","blockNumber":"0x798ad0b","hash":"0xbc9329afac05556497441e2b3ee4c5d4da7ca0b2a4c212c212d0739e94a24df9","transactionIndex":"0x0","type":"0x7e","nonce":"0x152ea95","input":"0x440a5e200000146b000f79c50000000000000003000000006725333f000000000141e287000000000000000000000000000000000000000000000000000000012439ee7e0000000000000000000000000000000000000000000000000000000063f363e973e96e7145ff001c81b9562cba7b6104eeb12a2bc4ab9f07c27d45cd81a986620000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f32985","mint":"0x0","sourceHash":"0x04e9a69416471ead93b02f0c279ab11ca0b635db5c1726a56faf22623bafde52","r":"0x0","s":"0x0","v":"0x0","yParity":"0x0","gas":"0xf4240","from":"0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001","to":"0x4200000000000000000000000000000000000015","depositReceiptVersion":"0x1","value":"0x0","gasPrice":"0x0"}"#;

        let tx = serde_json::from_str::<CeloTransaction>(rpc_tx).unwrap();

        let CeloTxEnvelope::Deposit(inner) = tx.as_ref() else {
            panic!("Expected deposit transaction");
        };
        assert_eq!(tx.inner.inner.signer(), inner.from);
        assert_eq!(tx.deposit_nonce, Some(22211221));
        assert_eq!(tx.inner.effective_gas_price, Some(0));

        let deserialized = serde_json::to_value(&tx).unwrap();
        let expected = serde_json::from_str::<serde_json::Value>(rpc_tx).unwrap();
        similar_asserts::assert_eq!(deserialized, expected);
    }

    #[test]
    fn can_deserialize_cip64() {
        // cast rpc eth_getTransactionByHash
        // 0x2403eb8e9dd5230ee0c89e430591b804bb2d4e218af479e07cc8ff0b4c3611e7 --rpc-url celo
        let rpc_tx = r#"{"accessList":[],"blockHash":"0x2a1c29764370aa197a2344507e4573e8bd1fbe757c10bd92e34eb9ad4934f391","blockNumber":"0x22d41c3","chainId":"0xa4ec","feeCurrency":"0x0e2a3e05bc9a16f5292a6170456a710cb89c6f72","from":"0x7fda9576b9256c5bbe7cc487a0e49da7f038e2f3","gas":"0x3d97c","gasPrice":"0x22a4c71a0","hash":"0x2403eb8e9dd5230ee0c89e430591b804bb2d4e218af479e07cc8ff0b4c3611e7","input":"0xcac35c7a290decd9548b62a8d60345a988386fc84ba6bc95484008f6362f93160ef3e563","maxFeePerGas":"0x315373261","maxPriorityFeePerGas":"0x63e4b","nonce":"0x133","r":"0x9de05cbec31a4fcf01c652408e51c58f82aae6c66513320dd9f06b77abfe1494","s":"0x363706d206c4649165bb27b54e6286cf4cedac8b7fdd78d9cb1e00047240e293","to":"0xa0e9096b8e5ad2701f51ca1cb11684aaad91993a","transactionIndex":"0x6","type":"0x7b","v":"0x1","value":"0x0","yParity":"0x1"}"#;

        let tx = serde_json::from_str::<CeloTransaction>(rpc_tx).unwrap();

        let CeloTxEnvelope::Cip64(_inner) = tx.as_ref() else {
            panic!("Expected CIP-64 transaction");
        };
        assert_eq!(
            tx.fee_currency,
            Some(address!("0x0e2a3e05bc9a16f5292a6170456a710cb89c6f72"))
        );

        let deserialized = serde_json::to_value(&tx).unwrap();
        let expected = serde_json::from_str::<serde_json::Value>(rpc_tx).unwrap();
        similar_asserts::assert_eq!(deserialized, expected);
    }
}
