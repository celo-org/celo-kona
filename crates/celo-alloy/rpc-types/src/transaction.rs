//! Celo specific types related to transactions.

use alloy_consensus::{Transaction as _, Typed2718};
use alloy_eips::{eip2930::AccessList, eip7702::SignedAuthorization};
use alloy_primitives::{Address, B256, BlockHash, Bytes, ChainId, TxKind, U256};
use celo_alloy_consensus::CeloTxEnvelope;
use serde::{Deserialize, Serialize};

mod request;
pub use request::CeloTransactionRequest;

/// Celo Transaction type
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, derive_more::Deref, derive_more::DerefMut,
)]
#[serde(
    try_from = "tx_serde::CeloTransactionSerdeHelper",
    into = "tx_serde::CeloTransactionSerdeHelper"
)]
#[cfg_attr(all(any(test, feature = "arbitrary"), feature = "k256"), derive(arbitrary::Arbitrary))]
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
        #[serde(default, rename = "feeCurrency", skip_serializing_if = "Option::is_none")]
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
                other: OptionalFields { from, effective_gas_price, deposit_nonce, fee_currency },
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
    use alloy_consensus::{SignableTransaction, TxEip7702, transaction::Recovered};
    use alloy_eips::{
        eip2930::AccessListItem,
        eip7702::{Authorization, SignedAuthorization},
    };
    use alloy_primitives::{Bytes, Signature, address, b256, hex};
    use celo_alloy_consensus::TxCip64;

    fn cip64_tx() -> TxCip64 {
        TxCip64 {
            chain_id: 0xa4ec,
            nonce: 0x705,
            gas_limit: 0x3644c,
            to: address!("0x7a1e295c4babdf229776680c93ed0f73d069abc0").into(),
            // Non-zero, non-default value to pin the `value -> Default` mutation
            // on the wrapper's forwarding accessor.
            value: U256::from(0xabc_u64),
            input: Bytes::copy_from_slice(&hex!(
                "0xcac35c7a290decd9548b62a8d60345a988386fc84ba6bc95484008f6362f93160ef3e563"
            )),
            max_fee_per_gas: 0x26442dbed,
            max_priority_fee_per_gas: 0x4d7ee,
            access_list: AccessList::default(),
            fee_currency: Some(address!("0x2f25deb3848c207fc8e0c34035b3ba7fc157602b")),
        }
    }

    fn cip64_sig() -> Signature {
        // Same canonical scalars used by `cip64_tx`'s real-world fixture in
        // `celo-alloy-consensus`. The signature is invalid for our edited
        // value/input but the wrapper accessors don't verify signatures.
        Signature::from_scalars_and_parity(
            b256!("0xaa0cfaa3df893578b3504062b862428f0e4a94046370cf2a4fd6c392c0760dd8"),
            b256!("0x1337d022bbb8faed78a9707e6c38d51f575816e7f85aa540f3d37a9081c58a71"),
            false,
        )
    }

    fn cip64_signer() -> Address {
        address!("0xefe945ee33ce4ab037ff4d1e1384d0efcd95f37b")
    }

    /// Builds a `CeloTransaction` with deliberately distinct, non-default
    /// values for every accessor. Pins the trait forwarding methods against
    /// constant-replacement mutations.
    fn populated_cip64_tx() -> CeloTransaction {
        let envelope = CeloTxEnvelope::Cip64(cip64_tx().into_signed(cip64_sig()));
        CeloTransaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: Recovered::new_unchecked(envelope, cip64_signer()),
                block_hash: Some(b256!(
                    "0x3333333333333333333333333333333333333333333333333333333333333333"
                )),
                block_number: Some(789),
                transaction_index: Some(7),
                effective_gas_price: Some(0x100_000_000),
            },
            deposit_nonce: None,
            deposit_receipt_version: None,
            fee_currency: Some(address!("0x2f25deb3848c207fc8e0c34035b3ba7fc157602b")),
        }
    }

    #[test]
    fn typed2718_ty_forwards_envelope_type() {
        let tx = populated_cip64_tx();
        // CIP-64 tx type is 0x7b — distinct from the 0/1 constant mutants.
        assert_eq!(tx.ty(), 0x7b);
    }

    #[test]
    fn transaction_trait_forwards_every_field() {
        let tx = populated_cip64_tx();
        assert_eq!(<CeloTransaction as alloy_consensus::Transaction>::chain_id(&tx), Some(0xa4ec));
        assert_eq!(<CeloTransaction as alloy_consensus::Transaction>::nonce(&tx), 0x705);
        assert_eq!(<CeloTransaction as alloy_consensus::Transaction>::gas_limit(&tx), 0x3644c);
        // CIP-64 has no legacy gas_price field — the wrapper returns None.
        // The `gas_price -> None` mutant is exercised by
        // `transaction_gas_price_returns_some_for_legacy` below.
        assert_eq!(<CeloTransaction as alloy_consensus::Transaction>::gas_price(&tx), None);
        assert_eq!(
            <CeloTransaction as alloy_consensus::Transaction>::max_fee_per_gas(&tx),
            0x26442dbed,
        );
        assert_eq!(
            <CeloTransaction as alloy_consensus::Transaction>::max_priority_fee_per_gas(&tx),
            Some(0x4d7ee),
        );
        // CeloTxEnvelope cannot hold an EIP-4844 tx; max_fee_per_blob_gas /
        // blob_versioned_hashes always return None. The corresponding
        // `-> None` mutations are equivalent and excluded in `.cargo/mutants.toml`.
        assert_eq!(
            <CeloTransaction as alloy_consensus::Transaction>::max_fee_per_blob_gas(&tx),
            None,
        );
        assert_eq!(
            <CeloTransaction as alloy_consensus::Transaction>::priority_fee_or_price(&tx),
            0x4d7ee,
        );
        assert_eq!(
            <CeloTransaction as alloy_consensus::Transaction>::effective_gas_price(&tx, Some(100)),
            // base_fee=100 + max_priority=0x4d7ee, capped by max_fee=0x26442dbed.
            100 + 0x4d7ee,
        );
        assert!(<CeloTransaction as alloy_consensus::Transaction>::is_dynamic_fee(&tx));
        assert_eq!(
            <CeloTransaction as alloy_consensus::Transaction>::kind(&tx),
            TxKind::Call(address!("0x7a1e295c4babdf229776680c93ed0f73d069abc0")),
        );
        assert!(!<CeloTransaction as alloy_consensus::Transaction>::is_create(&tx));
        assert_eq!(
            <CeloTransaction as alloy_consensus::Transaction>::to(&tx),
            Some(address!("0x7a1e295c4babdf229776680c93ed0f73d069abc0")),
        );
        assert_eq!(
            <CeloTransaction as alloy_consensus::Transaction>::value(&tx),
            U256::from(0xabc_u64),
        );
        assert_eq!(
            <CeloTransaction as alloy_consensus::Transaction>::input(&tx).len(),
            cip64_tx().input.len(),
        );
        // CIP-64 has an access list field but our fixture leaves it empty;
        // assert Some(empty) so the `-> None` mutation is caught.
        assert!(
            <CeloTransaction as alloy_consensus::Transaction>::access_list(&tx)
                .expect("Some access list")
                .iter()
                .next()
                .is_none()
        );
        assert_eq!(
            <CeloTransaction as alloy_consensus::Transaction>::blob_versioned_hashes(&tx),
            None,
        );
        // CIP-64 has no authorization list; the wrapper returns None.
        assert_eq!(
            <CeloTransaction as alloy_consensus::Transaction>::authorization_list(&tx),
            None,
        );
    }

    /// Pins `gas_price -> None` against a Legacy envelope (which carries a
    /// real gas_price). The CIP-64 forwarding test above pins the
    /// `Some(0)` / `Some(1)` mutants by asserting `None`.
    #[test]
    fn transaction_gas_price_returns_some_for_legacy() {
        use alloy_consensus::TxLegacy;
        let legacy = TxLegacy {
            chain_id: Some(0xa4ec),
            nonce: 0,
            gas_price: 0x12345,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Bytes::new(),
        };
        let envelope = CeloTxEnvelope::Legacy(legacy.into_signed(Signature::test_signature()));
        let tx = CeloTransaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: Recovered::new_unchecked(envelope, cip64_signer()),
                block_hash: None,
                block_number: None,
                transaction_index: None,
                effective_gas_price: None,
            },
            deposit_nonce: None,
            deposit_receipt_version: None,
            fee_currency: None,
        };
        assert_eq!(
            <CeloTransaction as alloy_consensus::Transaction>::gas_price(&tx),
            Some(0x12345),
        );
    }

    /// Pins `is_dynamic_fee -> false` by exercising it on a Legacy envelope
    /// (which is not dynamic-fee), and by asserting `true` for the CIP-64
    /// envelope above.
    #[test]
    fn transaction_is_dynamic_fee_returns_false_for_legacy() {
        // Build a Legacy envelope with a no-op signature; the type never
        // reads it back.
        use alloy_consensus::TxLegacy;
        let legacy = TxLegacy {
            chain_id: Some(0xa4ec),
            nonce: 0,
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Bytes::new(),
        };
        let signed = legacy.into_signed(Signature::test_signature());
        let envelope = CeloTxEnvelope::Legacy(signed);
        let tx = CeloTransaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: Recovered::new_unchecked(envelope, cip64_signer()),
                block_hash: None,
                block_number: None,
                transaction_index: None,
                effective_gas_price: Some(1),
            },
            deposit_nonce: None,
            deposit_receipt_version: None,
            fee_currency: None,
        };
        assert!(!<CeloTransaction as alloy_consensus::Transaction>::is_dynamic_fee(&tx));
    }

    /// Pins `is_create -> true` and `to -> None` by using a contract-creation
    /// envelope (TxKind::Create).
    #[test]
    fn transaction_is_create_returns_true_for_creation() {
        let mut inner_tx = cip64_tx();
        inner_tx.to = TxKind::Create;
        let envelope = CeloTxEnvelope::Cip64(inner_tx.into_signed(cip64_sig()));
        let tx = CeloTransaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: Recovered::new_unchecked(envelope, cip64_signer()),
                block_hash: None,
                block_number: None,
                transaction_index: None,
                effective_gas_price: None,
            },
            deposit_nonce: None,
            deposit_receipt_version: None,
            fee_currency: None,
        };
        assert!(<CeloTransaction as alloy_consensus::Transaction>::is_create(&tx));
        assert_eq!(<CeloTransaction as alloy_consensus::Transaction>::to(&tx), None);
        assert_eq!(<CeloTransaction as alloy_consensus::Transaction>::kind(&tx), TxKind::Create,);
    }

    /// Pins `access_list -> None` and `-> Some(empty)` against an envelope
    /// with a populated access list.
    #[test]
    fn transaction_access_list_returns_populated_list() {
        let access = AccessList(vec![AccessListItem {
            address: address!("0xdd00000000000000000000000000000000000004"),
            storage_keys: vec![b256!(
                "0x4444444444444444444444444444444444444444444444444444444444444444"
            )],
        }]);
        let mut inner_tx = cip64_tx();
        inner_tx.access_list = access;
        let envelope = CeloTxEnvelope::Cip64(inner_tx.into_signed(cip64_sig()));
        let tx = CeloTransaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: Recovered::new_unchecked(envelope, cip64_signer()),
                block_hash: None,
                block_number: None,
                transaction_index: None,
                effective_gas_price: None,
            },
            deposit_nonce: None,
            deposit_receipt_version: None,
            fee_currency: None,
        };
        let list = <CeloTransaction as alloy_consensus::Transaction>::access_list(&tx)
            .expect("Some access list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].address, address!("0xdd00000000000000000000000000000000000004"));
    }

    /// Pins `authorization_list -> None` and `-> Some(empty)` against an
    /// EIP-7702 envelope that has a populated authorization list.
    #[test]
    fn transaction_authorization_list_returns_populated_list() {
        let auth = SignedAuthorization::new_unchecked(
            Authorization {
                chain_id: U256::from(0xa4ec_u64),
                address: address!("0xff00000000000000000000000000000000000005"),
                nonce: 99,
            },
            0,
            U256::from(1_u64),
            U256::from(2_u64),
        );
        let inner = TxEip7702 {
            chain_id: 0xa4ec,
            nonce: 1,
            gas_limit: 21_000,
            max_fee_per_gas: 1_000,
            max_priority_fee_per_gas: 100,
            to: address!("0x7a1e295c4babdf229776680c93ed0f73d069abc0"),
            value: U256::ZERO,
            access_list: AccessList::default(),
            authorization_list: vec![auth],
            input: Bytes::new(),
        };
        let envelope = CeloTxEnvelope::Eip7702(inner.into_signed(Signature::test_signature()));
        let tx = CeloTransaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: Recovered::new_unchecked(envelope, cip64_signer()),
                block_hash: None,
                block_number: None,
                transaction_index: None,
                effective_gas_price: None,
            },
            deposit_nonce: None,
            deposit_receipt_version: None,
            fee_currency: None,
        };
        let list = <CeloTransaction as alloy_consensus::Transaction>::authorization_list(&tx)
            .expect("Some authorization list");
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn transaction_response_forwards_every_field() {
        let tx = populated_cip64_tx();
        // tx_hash is derived from the signed envelope; just assert it's non-zero.
        assert_ne!(
            <CeloTransaction as alloy_network_primitives::TransactionResponse>::tx_hash(&tx),
            B256::ZERO,
        );
        assert_eq!(
            <CeloTransaction as alloy_network_primitives::TransactionResponse>::block_hash(&tx),
            Some(b256!("0x3333333333333333333333333333333333333333333333333333333333333333")),
        );
        assert_eq!(
            <CeloTransaction as alloy_network_primitives::TransactionResponse>::block_number(&tx),
            Some(789),
        );
        assert_eq!(
            <CeloTransaction as alloy_network_primitives::TransactionResponse>::transaction_index(
                &tx
            ),
            Some(7),
        );
        assert_eq!(
            <CeloTransaction as alloy_network_primitives::TransactionResponse>::from(&tx),
            cip64_signer(),
        );
    }

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
        assert_eq!(tx.fee_currency, Some(address!("0x0e2a3e05bc9a16f5292a6170456a710cb89c6f72")));

        let deserialized = serde_json::to_value(&tx).unwrap();
        let expected = serde_json::from_str::<serde_json::Value>(rpc_tx).unwrap();
        similar_asserts::assert_eq!(deserialized, expected);
    }
}
