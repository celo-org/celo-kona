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
}

impl CeloTransaction {
    /// Build an RPC [`CeloTransaction`] from a recovered consensus tx and the metadata
    /// produced by `OpTxInfoMapper`.
    ///
    /// Mirrors [`op_alloy_rpc_types::Transaction::from_transaction`]: deposits report
    /// `gasPrice = 0`; non-deposits report `effective_tip + base_fee`, falling back to
    /// `max_fee_per_gas` when no base fee is known. `deposit_nonce` and
    /// `deposit_receipt_version` are pulled from the receipt via the mapper.
    pub fn from_transaction(
        tx: alloy_consensus::transaction::Recovered<CeloTxEnvelope>,
        tx_info: op_alloy_consensus::transaction::OpTransactionInfo,
    ) -> Self {
        let base_fee = tx_info.inner.base_fee;
        let is_deposit = tx.inner().is_deposit();

        let effective_gas_price = if is_deposit {
            0
        } else {
            base_fee
                .map(|bf| tx.effective_tip_per_gas(bf).unwrap_or_default() + bf as u128)
                .unwrap_or_else(|| tx.max_fee_per_gas())
        };

        Self {
            inner: alloy_rpc_types_eth::Transaction {
                inner: tx,
                block_hash: tx_info.inner.block_hash,
                block_number: tx_info.inner.block_number,
                transaction_index: tx_info.inner.index,
                effective_gas_price: Some(effective_gas_price),
                block_timestamp: tx_info.inner.block_timestamp,
            },
            deposit_nonce: tx_info.deposit_meta.deposit_nonce,
            deposit_receipt_version: tx_info.deposit_meta.deposit_receipt_version,
        }
    }

    /// Address of the whitelisted ERC-20 currency used to pay for gas, or `None` for
    /// native-CELO transactions.
    ///
    /// Derived from the inner [`CeloTxEnvelope::Cip64`] variant; non-CIP-64 envelopes
    /// always return `None`.
    pub fn fee_currency(&self) -> Option<Address> {
        self.as_ref().as_cip64().and_then(|signed| signed.tx().fee_currency)
    }
}

/// Per-tx metadata consumed when building a [`CeloTransaction`] RPC response from a
/// consensus tx.
///
/// Wraps [`op_alloy_consensus::transaction::OpTransactionInfo`] (which carries deposit metadata)
/// and adds the fee-currency-denominated base fee for CIP-64 transactions, sourced from the
/// CIP-64 receipt.
/// The FC base fee lets us report a meaningful `gasPrice` for CIP-64 RPC responses; without
/// it we'd be forced to mix native and FC units (see [`cip64_effective_gas_price`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CeloTransactionInfo {
    /// Standard OP transaction info (block context + deposit metadata).
    pub inner: op_alloy_consensus::transaction::OpTransactionInfo,
    /// FC-denominated base fee from the CIP-64 receipt. `None` for non-CIP-64 txs or when the
    /// receipt isn't available.
    pub cip64_fc_base_fee: Option<u128>,
}

/// Compute the effective gas price for a CIP-64 tx given its fee-currency-denominated
/// base fee.
///
/// Equivalent to `min(max_fee_per_gas, base_fee + max_priority_fee_per_gas)` — same
/// formula used elsewhere in reth/alloy, but entirely in `u128` space.
///
/// Avoids narrowing `base_fee_in_erc20` (a `u128`) to `u64` before calling the
/// alloy-consensus `effective_gas_price` helper: for fee currencies with high
/// exchange-rate numerators (cheap token, expensive CELO), `base_fee_in_erc20` can
/// exceed `u64::MAX`, and the narrowing cast would wrap and report an incorrect
/// `effectiveGasPrice` in RPC receipts.
#[inline]
pub fn cip64_effective_gas_price(
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    base_fee: u128,
) -> u128 {
    max_fee_per_gas.min(base_fee.saturating_add(max_priority_fee_per_gas))
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
    ///
    /// `feeCurrency` is intentionally **not** carried here: the inner [`CeloTxEnvelope::Cip64`]
    /// already serializes it via the flattened envelope, so duplicating it on this helper would
    /// emit the JSON key twice in `serde_json::to_string` output.
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
        block_timestamp: Option<u64>,
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
                        block_timestamp,
                    },
                deposit_receipt_version,
                deposit_nonce,
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
                block_timestamp,
                deposit_receipt_version,
                other: OptionalFields { from, effective_gas_price, deposit_nonce },
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
                block_timestamp,
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
                    block_timestamp,
                },
                deposit_receipt_version,
                deposit_nonce,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{
        SignableTransaction, TxEip1559, TxEip2930, TxEip7702, TxLegacy, transaction::Recovered,
    };
    use alloy_eips::eip2930::AccessList;
    use alloy_primitives::{Signature, address, b256, bytes};
    use celo_alloy_consensus::TxCip64;
    use op_alloy_consensus::TxDeposit;

    // Golden-byte JSON tests assert the exact `serde_json::to_string` output of every
    // `CeloTxEnvelope` variant. Unlike `to_value`-based round-trips, they catch
    // duplicate-key regressions (cf. d6c83feb, where flattened CIP-64 + a top-level
    // helper field both emitted `"feeCurrency"`) and missing-required-field regressions
    // (cf. 0d5324d7, where deposit RPC responses lacked `nonce` and
    // `depositReceiptVersion`).
    //
    // Field-order caveat: the golden strings reflect struct-declaration order of
    // `CeloTransactionSerdeHelper` (line 189) and the flattened `CeloTxEnvelope` variant.
    // Reordering those fields will fail these tests and require regenerating the
    // expected literals. Use the `_regenerate_goldens` test below to re-emit them.

    fn sample_block_hash() -> B256 {
        b256!("0x2a1c29764370aa197a2344507e4573e8bd1fbe757c10bd92e34eb9ad4934f391")
    }
    fn sample_signer() -> Address {
        address!("0x7fda9576b9256c5bbe7cc487a0e49da7f038e2f3")
    }
    fn sample_to() -> Address {
        address!("0x00000000000000000000000000000000deadbeef")
    }
    fn wrap_envelope(env: CeloTxEnvelope, signer: Address, gas_price: u128) -> CeloTransaction {
        CeloTransaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: Recovered::new_unchecked(env, signer),
                block_hash: Some(sample_block_hash()),
                block_number: Some(1),
                transaction_index: Some(0),
                effective_gas_price: Some(gas_price),
                block_timestamp: None,
            },
            deposit_nonce: None,
            deposit_receipt_version: None,
        }
    }

    fn fx_legacy() -> CeloTransaction {
        let env: CeloTxEnvelope = TxLegacy {
            chain_id: Some(42220),
            nonce: 1,
            gas_price: 100_000_000_000,
            gas_limit: 21_000,
            to: sample_to().into(),
            value: U256::from(1u64),
            input: Bytes::new(),
        }
        .into_signed(Signature::test_signature())
        .into();
        wrap_envelope(env, sample_signer(), 100_000_000_000)
    }

    fn fx_eip2930() -> CeloTransaction {
        let env: CeloTxEnvelope = TxEip2930 {
            chain_id: 42220,
            nonce: 2,
            gas_price: 100_000_000_000,
            gas_limit: 21_000,
            to: sample_to().into(),
            value: U256::from(1u64),
            access_list: AccessList::default(),
            input: Bytes::new(),
        }
        .into_signed(Signature::test_signature())
        .into();
        wrap_envelope(env, sample_signer(), 100_000_000_000)
    }

    fn fx_eip1559() -> CeloTransaction {
        let env: CeloTxEnvelope = TxEip1559 {
            chain_id: 42220,
            nonce: 3,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            gas_limit: 21_000,
            to: sample_to().into(),
            value: U256::from(1u64),
            access_list: AccessList::default(),
            input: Bytes::new(),
        }
        .into_signed(Signature::test_signature())
        .into();
        wrap_envelope(env, sample_signer(), 100_000_000_000)
    }

    fn fx_eip7702() -> CeloTransaction {
        let env: CeloTxEnvelope = TxEip7702 {
            chain_id: 42220,
            nonce: 4,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            gas_limit: 21_000,
            to: sample_to(),
            value: U256::from(1u64),
            access_list: AccessList::default(),
            authorization_list: vec![],
            input: Bytes::new(),
        }
        .into_signed(Signature::test_signature())
        .into();
        wrap_envelope(env, sample_signer(), 100_000_000_000)
    }

    const CIP64_WITH_FC_INPUT: &str = r#"{"accessList":[],"blockHash":"0x2a1c29764370aa197a2344507e4573e8bd1fbe757c10bd92e34eb9ad4934f391","blockNumber":"0x22d41c3","chainId":"0xa4ec","feeCurrency":"0x0e2a3e05bc9a16f5292a6170456a710cb89c6f72","from":"0x7fda9576b9256c5bbe7cc487a0e49da7f038e2f3","gas":"0x3d97c","gasPrice":"0x22a4c71a0","hash":"0x2403eb8e9dd5230ee0c89e430591b804bb2d4e218af479e07cc8ff0b4c3611e7","input":"0xcac35c7a290decd9548b62a8d60345a988386fc84ba6bc95484008f6362f93160ef3e563","maxFeePerGas":"0x315373261","maxPriorityFeePerGas":"0x63e4b","nonce":"0x133","r":"0x9de05cbec31a4fcf01c652408e51c58f82aae6c66513320dd9f06b77abfe1494","s":"0x363706d206c4649165bb27b54e6286cf4cedac8b7fdd78d9cb1e00047240e293","to":"0xa0e9096b8e5ad2701f51ca1cb11684aaad91993a","transactionIndex":"0x6","type":"0x7b","v":"0x1","value":"0x0","yParity":"0x1"}"#;

    fn fx_cip64_with_fc() -> CeloTransaction {
        serde_json::from_str(CIP64_WITH_FC_INPUT).unwrap()
    }

    fn fx_cip64_no_fc() -> CeloTransaction {
        let env: CeloTxEnvelope = TxCip64 {
            chain_id: 42220,
            nonce: 5,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            gas_limit: 21_000,
            to: sample_to().into(),
            value: U256::from(1u64),
            access_list: AccessList::default(),
            fee_currency: None,
            input: Bytes::new(),
        }
        .into_signed(Signature::test_signature())
        .into();
        wrap_envelope(env, sample_signer(), 100_000_000_000)
    }

    const DEPOSIT_POST_CANYON_INPUT: &str = r#"{"blockHash":"0x9d86bb313ebeedf4f9f82bf8a19b426be656a365648a7c089b618771311db9f9","blockNumber":"0x798ad0b","hash":"0xbc9329afac05556497441e2b3ee4c5d4da7ca0b2a4c212c212d0739e94a24df9","transactionIndex":"0x0","type":"0x7e","nonce":"0x152ea95","input":"0x440a5e200000146b000f79c50000000000000003000000006725333f000000000141e287000000000000000000000000000000000000000000000000000000012439ee7e0000000000000000000000000000000000000000000000000000000063f363e973e96e7145ff001c81b9562cba7b6104eeb12a2bc4ab9f07c27d45cd81a986620000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f32985","mint":"0x0","sourceHash":"0x04e9a69416471ead93b02f0c279ab11ca0b635db5c1726a56faf22623bafde52","r":"0x0","s":"0x0","v":"0x0","yParity":"0x0","gas":"0xf4240","from":"0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001","to":"0x4200000000000000000000000000000000000015","depositReceiptVersion":"0x1","value":"0x0","gasPrice":"0x0"}"#;

    fn fx_deposit_post_canyon() -> CeloTransaction {
        serde_json::from_str(DEPOSIT_POST_CANYON_INPUT).unwrap()
    }

    fn fx_deposit_pre_canyon() -> CeloTransaction {
        let env: CeloTxEnvelope = TxDeposit {
            source_hash: b256!(
                "0x04e9a69416471ead93b02f0c279ab11ca0b635db5c1726a56faf22623bafde52"
            ),
            from: address!("0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001"),
            to: address!("0x4200000000000000000000000000000000000015").into(),
            mint: 0,
            value: U256::ZERO,
            gas_limit: 1_000_000,
            is_system_transaction: false,
            input: bytes!("0x440a5e20"),
        }
        .into();
        CeloTransaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: Recovered::new_unchecked(
                    env,
                    address!("0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001"),
                ),
                block_hash: Some(sample_block_hash()),
                block_number: Some(1),
                transaction_index: Some(0),
                effective_gas_price: Some(0),
                block_timestamp: None,
            },
            deposit_nonce: Some(99),
            deposit_receipt_version: None,
        }
    }

    #[test]
    fn golden_legacy_tx() {
        let expected = r#"{"type":"0x0","chainId":"0xa4ec","nonce":"0x1","gasPrice":"0x174876e800","gas":"0x5208","to":"0x00000000000000000000000000000000deadbeef","value":"0x1","input":"0x","r":"0x840cfc572845f5786e702984c2a582528cad4b49b2a10b9db1be7fca90058565","s":"0x25e7109ceb98168d95b09b18bbf6b685130e0562f233877d492b94eee0c5b6d1","v":"0x149fb","hash":"0x8731cdf69bf60003051f802c9492a19712a9cdf69a8288e1a7719a085d8fe192","blockHash":"0x2a1c29764370aa197a2344507e4573e8bd1fbe757c10bd92e34eb9ad4934f391","blockNumber":"0x1","transactionIndex":"0x0","from":"0x7fda9576b9256c5bbe7cc487a0e49da7f038e2f3"}"#;
        assert_eq!(serde_json::to_string(&fx_legacy()).unwrap(), expected);
    }

    #[test]
    fn golden_eip2930_tx() {
        let expected = r#"{"type":"0x1","chainId":"0xa4ec","nonce":"0x2","gasPrice":"0x174876e800","gas":"0x5208","to":"0x00000000000000000000000000000000deadbeef","value":"0x1","accessList":[],"input":"0x","r":"0x840cfc572845f5786e702984c2a582528cad4b49b2a10b9db1be7fca90058565","s":"0x25e7109ceb98168d95b09b18bbf6b685130e0562f233877d492b94eee0c5b6d1","yParity":"0x0","v":"0x0","hash":"0x037a7231e6d8311f603d27a2f5f557a79841111537cce8d7e6887e5b69b9cfce","blockHash":"0x2a1c29764370aa197a2344507e4573e8bd1fbe757c10bd92e34eb9ad4934f391","blockNumber":"0x1","transactionIndex":"0x0","from":"0x7fda9576b9256c5bbe7cc487a0e49da7f038e2f3"}"#;
        assert_eq!(serde_json::to_string(&fx_eip2930()).unwrap(), expected);
    }

    #[test]
    fn golden_eip1559_tx() {
        let expected = r#"{"type":"0x2","chainId":"0xa4ec","nonce":"0x3","gas":"0x5208","maxFeePerGas":"0x174876e800","maxPriorityFeePerGas":"0x3b9aca00","to":"0x00000000000000000000000000000000deadbeef","value":"0x1","accessList":[],"input":"0x","r":"0x840cfc572845f5786e702984c2a582528cad4b49b2a10b9db1be7fca90058565","s":"0x25e7109ceb98168d95b09b18bbf6b685130e0562f233877d492b94eee0c5b6d1","yParity":"0x0","v":"0x0","hash":"0xa2b40a876c192a188abc12accea27591ca31a0545eea1a5ba8369d04ba0b73b7","blockHash":"0x2a1c29764370aa197a2344507e4573e8bd1fbe757c10bd92e34eb9ad4934f391","blockNumber":"0x1","transactionIndex":"0x0","from":"0x7fda9576b9256c5bbe7cc487a0e49da7f038e2f3","gasPrice":"0x174876e800"}"#;
        assert_eq!(serde_json::to_string(&fx_eip1559()).unwrap(), expected);
    }

    #[test]
    fn golden_eip7702_tx() {
        let expected = r#"{"type":"0x4","chainId":"0xa4ec","nonce":"0x4","gas":"0x5208","maxFeePerGas":"0x174876e800","maxPriorityFeePerGas":"0x3b9aca00","to":"0x00000000000000000000000000000000deadbeef","value":"0x1","accessList":[],"authorizationList":[],"input":"0x","r":"0x840cfc572845f5786e702984c2a582528cad4b49b2a10b9db1be7fca90058565","s":"0x25e7109ceb98168d95b09b18bbf6b685130e0562f233877d492b94eee0c5b6d1","yParity":"0x0","v":"0x0","hash":"0x7e67dcd0583c475a2f064275334cea277a2c63e7cff44353937a5912478f076f","blockHash":"0x2a1c29764370aa197a2344507e4573e8bd1fbe757c10bd92e34eb9ad4934f391","blockNumber":"0x1","transactionIndex":"0x0","from":"0x7fda9576b9256c5bbe7cc487a0e49da7f038e2f3","gasPrice":"0x174876e800"}"#;
        assert_eq!(serde_json::to_string(&fx_eip7702()).unwrap(), expected);
    }

    // Regression test for d6c83feb: prior to that fix, this assertion failed because
    // `"feeCurrency"` was emitted twice — once from the flattened CIP-64 envelope and
    // once from a top-level helper field. The `to_value` round-trip in
    // `can_deserialize_cip64` couldn't see this because `serde_json::Value` dedupes
    // duplicate JSON keys silently.
    #[test]
    fn golden_cip64_tx_with_fee_currency() {
        let expected = r#"{"type":"0x7b","chainId":"0xa4ec","nonce":"0x133","gas":"0x3d97c","maxFeePerGas":"0x315373261","maxPriorityFeePerGas":"0x63e4b","to":"0xa0e9096b8e5ad2701f51ca1cb11684aaad91993a","value":"0x0","accessList":[],"feeCurrency":"0x0e2a3e05bc9a16f5292a6170456a710cb89c6f72","input":"0xcac35c7a290decd9548b62a8d60345a988386fc84ba6bc95484008f6362f93160ef3e563","r":"0x9de05cbec31a4fcf01c652408e51c58f82aae6c66513320dd9f06b77abfe1494","s":"0x363706d206c4649165bb27b54e6286cf4cedac8b7fdd78d9cb1e00047240e293","yParity":"0x1","v":"0x1","hash":"0x2403eb8e9dd5230ee0c89e430591b804bb2d4e218af479e07cc8ff0b4c3611e7","blockHash":"0x2a1c29764370aa197a2344507e4573e8bd1fbe757c10bd92e34eb9ad4934f391","blockNumber":"0x22d41c3","transactionIndex":"0x6","from":"0x7fda9576b9256c5bbe7cc487a0e49da7f038e2f3","gasPrice":"0x22a4c71a0"}"#;
        assert_eq!(serde_json::to_string(&fx_cip64_with_fc()).unwrap(), expected);
    }

    // CIP-64 with `fee_currency = None` emits `"feeCurrency":null` rather than omitting
    // the field — locks the wire-shape decision so a future `skip_serializing_if` change
    // is a deliberate, golden-bumping update rather than an accidental client break.
    #[test]
    fn golden_cip64_tx_without_fee_currency() {
        let expected = r#"{"type":"0x7b","chainId":"0xa4ec","nonce":"0x5","gas":"0x5208","maxFeePerGas":"0x174876e800","maxPriorityFeePerGas":"0x3b9aca00","to":"0x00000000000000000000000000000000deadbeef","value":"0x1","accessList":[],"feeCurrency":null,"input":"0x","r":"0x840cfc572845f5786e702984c2a582528cad4b49b2a10b9db1be7fca90058565","s":"0x25e7109ceb98168d95b09b18bbf6b685130e0562f233877d492b94eee0c5b6d1","yParity":"0x0","v":"0x0","hash":"0xc6475d44bdf6229f48be13bd96e567745558555e7e136dad1dfbbd39d50a3142","blockHash":"0x2a1c29764370aa197a2344507e4573e8bd1fbe757c10bd92e34eb9ad4934f391","blockNumber":"0x1","transactionIndex":"0x0","from":"0x7fda9576b9256c5bbe7cc487a0e49da7f038e2f3","gasPrice":"0x174876e800"}"#;
        assert_eq!(serde_json::to_string(&fx_cip64_no_fc()).unwrap(), expected);
    }

    // Regression test for 0d5324d7: prior to that fix the deposit-tx serializer omitted
    // `"nonce"` and `"depositReceiptVersion"` entirely, breaking viem / web3.py / ethers.
    #[test]
    fn golden_deposit_tx_post_canyon() {
        let expected = r#"{"type":"0x7e","sourceHash":"0x04e9a69416471ead93b02f0c279ab11ca0b635db5c1726a56faf22623bafde52","from":"0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001","to":"0x4200000000000000000000000000000000000015","mint":"0x0","value":"0x0","gas":"0xf4240","input":"0x440a5e200000146b000f79c50000000000000003000000006725333f000000000141e287000000000000000000000000000000000000000000000000000000012439ee7e0000000000000000000000000000000000000000000000000000000063f363e973e96e7145ff001c81b9562cba7b6104eeb12a2bc4ab9f07c27d45cd81a986620000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f32985","hash":"0xbc9329afac05556497441e2b3ee4c5d4da7ca0b2a4c212c212d0739e94a24df9","r":"0x0","s":"0x0","yParity":"0x0","v":"0x0","blockHash":"0x9d86bb313ebeedf4f9f82bf8a19b426be656a365648a7c089b618771311db9f9","blockNumber":"0x798ad0b","transactionIndex":"0x0","depositReceiptVersion":"0x1","gasPrice":"0x0","nonce":"0x152ea95"}"#;
        assert_eq!(serde_json::to_string(&fx_deposit_post_canyon()).unwrap(), expected);
    }

    // Pre-canyon deposit: `depositReceiptVersion` is omitted entirely (not null).
    #[test]
    fn golden_deposit_tx_pre_canyon() {
        let expected = r#"{"type":"0x7e","sourceHash":"0x04e9a69416471ead93b02f0c279ab11ca0b635db5c1726a56faf22623bafde52","from":"0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001","to":"0x4200000000000000000000000000000000000015","mint":"0x0","value":"0x0","gas":"0xf4240","input":"0x440a5e20","hash":"0x73bae408587f1f228fad672762f1a219e63f13c415469e14e866a844912b6ec3","r":"0x0","s":"0x0","yParity":"0x0","v":"0x0","blockHash":"0x2a1c29764370aa197a2344507e4573e8bd1fbe757c10bd92e34eb9ad4934f391","blockNumber":"0x1","transactionIndex":"0x0","gasPrice":"0x0","nonce":"0x63"}"#;
        assert_eq!(serde_json::to_string(&fx_deposit_pre_canyon()).unwrap(), expected);
    }

    // Maintainer utility: prints fresh golden-byte strings for every fixture. Run with
    // `cargo test -p celo-alloy-rpc-types _regenerate_goldens -- --ignored --nocapture`
    // when `CeloTransactionSerdeHelper` field order, the flattened `CeloTxEnvelope`
    // shape, or any of the underlying tx-type structs change. Always fails so it
    // doesn't run in normal suites.
    #[test]
    #[ignore = "generator: re-emit golden literals when struct order changes"]
    fn _regenerate_goldens() {
        for (name, json) in [
            ("legacy", serde_json::to_string(&fx_legacy()).unwrap()),
            ("eip2930", serde_json::to_string(&fx_eip2930()).unwrap()),
            ("eip1559", serde_json::to_string(&fx_eip1559()).unwrap()),
            ("eip7702", serde_json::to_string(&fx_eip7702()).unwrap()),
            ("cip64_with_fc", serde_json::to_string(&fx_cip64_with_fc()).unwrap()),
            ("cip64_no_fc", serde_json::to_string(&fx_cip64_no_fc()).unwrap()),
            ("deposit_post_canyon", serde_json::to_string(&fx_deposit_post_canyon()).unwrap()),
            ("deposit_pre_canyon", serde_json::to_string(&fx_deposit_pre_canyon()).unwrap()),
        ] {
            eprintln!("GOLDEN[{name}]: {json}");
        }
        panic!("regenerator only — copy GOLDEN[...] lines into each golden_* test literal");
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
        assert_eq!(tx.fee_currency(), Some(address!("0x0e2a3e05bc9a16f5292a6170456a710cb89c6f72")));

        let deserialized = serde_json::to_value(&tx).unwrap();
        let expected = serde_json::from_str::<serde_json::Value>(rpc_tx).unwrap();
        similar_asserts::assert_eq!(deserialized, expected);
    }

    /// `serde_json::to_value` collapses duplicate keys into a `Map`, so the existing
    /// roundtrip tests can't see the wire-shape regression where `feeCurrency` is
    /// emitted twice — once from the flattened CIP-64 envelope and once from the
    /// top-level helper. Assert against the actual `to_string` byte stream.
    #[test]
    fn cip64_to_string_emits_fee_currency_once() {
        let rpc_tx = r#"{"accessList":[],"blockHash":"0x2a1c29764370aa197a2344507e4573e8bd1fbe757c10bd92e34eb9ad4934f391","blockNumber":"0x22d41c3","chainId":"0xa4ec","feeCurrency":"0x0e2a3e05bc9a16f5292a6170456a710cb89c6f72","from":"0x7fda9576b9256c5bbe7cc487a0e49da7f038e2f3","gas":"0x3d97c","gasPrice":"0x22a4c71a0","hash":"0x2403eb8e9dd5230ee0c89e430591b804bb2d4e218af479e07cc8ff0b4c3611e7","input":"0xcac35c7a290decd9548b62a8d60345a988386fc84ba6bc95484008f6362f93160ef3e563","maxFeePerGas":"0x315373261","maxPriorityFeePerGas":"0x63e4b","nonce":"0x133","r":"0x9de05cbec31a4fcf01c652408e51c58f82aae6c66513320dd9f06b77abfe1494","s":"0x363706d206c4649165bb27b54e6286cf4cedac8b7fdd78d9cb1e00047240e293","to":"0xa0e9096b8e5ad2701f51ca1cb11684aaad91993a","transactionIndex":"0x6","type":"0x7b","v":"0x1","value":"0x0","yParity":"0x1"}"#;

        let tx = serde_json::from_str::<CeloTransaction>(rpc_tx).unwrap();
        assert_eq!(tx.fee_currency(), Some(address!("0x0e2a3e05bc9a16f5292a6170456a710cb89c6f72")));

        let s = serde_json::to_string(&tx).unwrap();
        assert_eq!(
            s.matches("\"feeCurrency\"").count(),
            1,
            "feeCurrency must appear exactly once in the JSON wire output, got: {s}"
        );
    }
}
