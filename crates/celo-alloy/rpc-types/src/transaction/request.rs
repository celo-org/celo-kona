use alloc::vec::Vec;
use alloy_consensus::{
    Sealed, SignableTransaction, Signed, TxEip1559, TxEip4844, TxEip4844Variant, TypedTransaction,
};
use alloy_eips::eip7702::SignedAuthorization;
use alloy_network_primitives::TransactionBuilder7702;
use alloy_primitives::{Address, Signature, TxKind, U256};
use alloy_rpc_types_eth::{AccessList, TransactionInput, TransactionRequest};
use celo_alloy_consensus::{CeloTxEnvelope, CeloTypedTransaction, TxCip64};
use op_alloy_consensus::TxDeposit;
use op_alloy_rpc_types::OpTransactionRequest;
use serde::{Deserialize, Serialize};

/// Builder for [`CeloTypedTransaction`].
///
/// Wraps [`OpTransactionRequest`] and adds the Celo CIP-64 `feeCurrency` field, which the
/// standard request types silently drop. `fee_currency = None` means the fee is paid in
/// native CELO.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct CeloTransactionRequest {
    /// The wrapped OP-stack transaction request.
    pub inner: OpTransactionRequest,
    /// Celo CIP-64 fee currency address (`feeCurrency` in JSON).
    pub fee_currency: Option<Address>,
}

/// Helper struct for serializing [`CeloTransactionRequest`].
///
/// Emits the canonical `feeCurrency` key, and omits it for a zero address so peers that
/// don't normalize cannot misread native CELO as a real fee currency. Deserialization is
/// manual (see below) to match the key case-insensitively.
#[derive(Serialize)]
struct CeloTransactionRequestHelper {
    #[serde(flatten)]
    inner: OpTransactionRequest,
    #[serde(rename = "feeCurrency", skip_serializing_if = "Option::is_none")]
    fee_currency: Option<Address>,
}

impl Serialize for CeloTransactionRequest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        CeloTransactionRequestHelper {
            inner: self.inner.clone(),
            fee_currency: self.non_native_fee_currency(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CeloTransactionRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        // go-ethereum matches JSON keys case-insensitively, so op-geth binds any casing of
        // `feeCurrency` to the same field. serde does not, so pull the key out by hand:
        // otherwise a request using minipay's `feecurrency` reads as a native-fee tx and
        // gets a surcharge-free, and thus too low, gas estimate.
        let mut value = serde_json::Value::deserialize(deserializer)?;

        // A missing key or JSON `null` means native fee. Anything else that fails to parse
        // is a hard error, so a CIP-64 request never falls back to native fees unnoticed.
        let mut fee_currency = None;
        if let Some(obj) = value.as_object_mut() {
            let keys: Vec<_> =
                obj.keys().filter(|key| key.eq_ignore_ascii_case("feecurrency")).cloned().collect();
            if keys.len() > 1 {
                return Err(D::Error::custom("duplicate feeCurrency fields"));
            }
            if let Some(key) = keys.into_iter().next() {
                let raw = obj.remove(&key).expect("key just found above");
                fee_currency = serde_json::from_value(raw).map_err(D::Error::custom)?;
            }
        }

        let inner = serde_json::from_value(value).map_err(D::Error::custom)?;
        Ok(Self { inner, fee_currency })
    }
}

impl CeloTransactionRequest {
    /// Sets the `from` field in the call to the provided address
    #[inline]
    pub fn from(mut self, from: Address) -> Self {
        self.inner = self.inner.from(from);
        self
    }

    /// Sets the transactions type for the transactions.
    #[doc(alias = "tx_type")]
    pub fn transaction_type(mut self, transaction_type: u8) -> Self {
        self.inner = self.inner.transaction_type(transaction_type);
        self
    }

    /// Sets the gas limit for the transaction.
    pub fn gas_limit(mut self, gas_limit: u64) -> Self {
        self.inner = self.inner.gas_limit(gas_limit);
        self
    }

    /// Sets the nonce for the transaction.
    pub fn nonce(mut self, nonce: u64) -> Self {
        self.inner = self.inner.nonce(nonce);
        self
    }

    /// Sets the maximum fee per gas for the transaction.
    ///
    /// For CIP-64 transactions (see [`Self::fee_currency`]) this value is denominated in
    /// units of the fee currency, not native CELO wei.
    pub fn max_fee_per_gas(mut self, max_fee_per_gas: u128) -> Self {
        self.inner = self.inner.max_fee_per_gas(max_fee_per_gas);
        self
    }

    /// Sets the maximum priority fee per gas for the transaction.
    ///
    /// For CIP-64 transactions (see [`Self::fee_currency`]) this value is denominated in
    /// units of the fee currency, not native CELO wei.
    pub fn max_priority_fee_per_gas(mut self, max_priority_fee_per_gas: u128) -> Self {
        self.inner = self.inner.max_priority_fee_per_gas(max_priority_fee_per_gas);
        self
    }

    /// Sets the recipient address for the transaction.
    #[inline]
    pub fn to(mut self, to: Address) -> Self {
        self.inner = self.inner.to(to);
        self
    }

    /// Sets the value (amount) for the transaction.
    pub fn value(mut self, value: U256) -> Self {
        self.inner = self.inner.value(value);
        self
    }

    /// Sets the access list for the transaction.
    pub fn access_list(mut self, access_list: AccessList) -> Self {
        self.inner = self.inner.access_list(access_list);
        self
    }

    /// Sets the input data for the transaction.
    pub fn input(mut self, input: TransactionInput) -> Self {
        self.inner = self.inner.input(input);
        self
    }

    /// Sets the CIP-64 fee currency, making this a type `0x7b` transaction whose gas is
    /// paid in the given whitelisted ERC-20 token.
    ///
    /// The fee fields are then denominated in that currency, not in native CELO wei.
    /// `Address::ZERO` denotes native CELO (see [`Self::non_native_fee_currency`]).
    pub const fn fee_currency(mut self, fee_currency: Address) -> Self {
        self.fee_currency = Some(fee_currency);
        self
    }

    /// The fee currency, `Some` only for a real ERC20: `None` and `Address::ZERO` both
    /// denote native CELO. Same rule as `celo_revm::non_native_fee_currency`, which the
    /// node applies on its side of the RPC boundary.
    pub fn non_native_fee_currency(&self) -> Option<Address> {
        self.fee_currency.filter(|fee_currency| *fee_currency != Address::ZERO)
    }

    /// Whether this request builds a CIP-64 (type `0x7b`) transaction: a non-native fee
    /// currency is set, or the caller tagged the type explicitly. The tag covers native-fee
    /// CIP-64 (`fee_currency = None`), which otherwise looks like an EIP-1559 request and
    /// would rebuild as one, under a different signing hash.
    pub fn is_cip64(&self) -> bool {
        self.non_native_fee_currency().is_some()
            || self.inner.as_ref().transaction_type
                == Some(celo_alloy_consensus::CeloTxType::Cip64 as u8)
    }

    /// Builds [`CeloTypedTransaction`] from this builder. See
    /// [`TransactionRequest::build_typed_tx`] for more info.
    ///
    /// Celo has no EIP-4844: blob requests build as EIP-1559, or CIP-64 with a fee currency.
    /// A CIP-64 request (see [`Self::is_cip64`]) builds only from the EIP-1559 shape; legacy
    /// and EIP-2930 (`gasPrice`) and EIP-7702 (`authorizationList`) conflict and return
    /// `Err`.
    #[allow(clippy::result_large_err)]
    pub fn build_typed_tx(self) -> Result<CeloTypedTransaction, Self> {
        let Ok(tx) = self.inner.as_ref().clone().build_typed_tx() else {
            return Err(self);
        };

        // EIP-4844 is unsupported on Celo; downgrade to the equivalent EIP-1559 shape.
        let tx = match tx {
            TypedTransaction::Eip4844(tx) => {
                let tx: TxEip4844 = tx.into();
                TypedTransaction::Eip1559(TxEip1559 {
                    chain_id: tx.chain_id,
                    nonce: tx.nonce,
                    gas_limit: tx.gas_limit,
                    max_priority_fee_per_gas: tx.max_priority_fee_per_gas,
                    max_fee_per_gas: tx.max_fee_per_gas,
                    to: TxKind::Call(tx.to),
                    value: tx.value,
                    access_list: tx.access_list,
                    input: tx.input,
                })
            }
            tx => tx,
        };

        if !self.is_cip64() {
            return Ok(match tx {
                TypedTransaction::Legacy(tx) => CeloTypedTransaction::Legacy(tx),
                TypedTransaction::Eip2930(tx) => CeloTypedTransaction::Eip2930(tx),
                TypedTransaction::Eip1559(tx) => CeloTypedTransaction::Eip1559(tx),
                TypedTransaction::Eip7702(tx) => CeloTypedTransaction::Eip7702(tx),
                TypedTransaction::Eip4844(_) => unreachable!("downgraded to EIP-1559 above"),
            });
        }

        // Rejecting the conflicting shapes rather than dropping their fields mirrors
        // celo-reth's `Cip64Conflict` handling.
        match tx {
            TypedTransaction::Eip1559(tx) => Ok(CeloTypedTransaction::Cip64(TxCip64 {
                chain_id: tx.chain_id,
                nonce: tx.nonce,
                gas_limit: tx.gas_limit,
                max_fee_per_gas: tx.max_fee_per_gas,
                max_priority_fee_per_gas: tx.max_priority_fee_per_gas,
                to: tx.to,
                value: tx.value,
                access_list: tx.access_list,
                fee_currency: self.non_native_fee_currency(),
                input: tx.input,
            })),
            _ => Err(self),
        }
    }
}

impl From<TransactionRequest> for CeloTransactionRequest {
    fn from(inner: TransactionRequest) -> Self {
        Self { inner: inner.into(), fee_currency: None }
    }
}

impl From<OpTransactionRequest> for CeloTransactionRequest {
    fn from(inner: OpTransactionRequest) -> Self {
        Self { inner, fee_currency: None }
    }
}

impl From<TxDeposit> for CeloTransactionRequest {
    fn from(tx: TxDeposit) -> Self {
        Self { inner: tx.into(), fee_currency: None }
    }
}

impl From<Sealed<TxDeposit>> for CeloTransactionRequest {
    fn from(value: Sealed<TxDeposit>) -> Self {
        value.into_inner().into()
    }
}

/// Shared body for the `From<Signed<...>>` impls below. Not a blanket impl: that would be
/// public API through which `Signed<TxCip64>` loses its fee currency, and coherence forbids
/// specializing it for that one type.
fn from_signed<T>(value: Signed<T, Signature>) -> CeloTransactionRequest
where
    T: SignableTransaction<Signature> + Into<TransactionRequest>,
{
    #[cfg(feature = "k256")]
    let from = value.recover_signer().ok();
    #[cfg(not(feature = "k256"))]
    let from = None;

    let mut inner: TransactionRequest = value.strip_signature().into();
    inner.from = from;

    inner.into()
}

impl From<Signed<alloy_consensus::TxLegacy>> for CeloTransactionRequest {
    fn from(value: Signed<alloy_consensus::TxLegacy>) -> Self {
        from_signed(value)
    }
}

impl From<Signed<alloy_consensus::TxEip2930>> for CeloTransactionRequest {
    fn from(value: Signed<alloy_consensus::TxEip2930>) -> Self {
        from_signed(value)
    }
}

impl From<Signed<TxEip1559>> for CeloTransactionRequest {
    fn from(value: Signed<TxEip1559>) -> Self {
        from_signed(value)
    }
}

impl From<Signed<TxEip4844>> for CeloTransactionRequest {
    fn from(value: Signed<TxEip4844>) -> Self {
        from_signed(value)
    }
}

impl From<Signed<TxEip4844Variant>> for CeloTransactionRequest {
    fn from(value: Signed<TxEip4844Variant>) -> Self {
        from_signed(value)
    }
}

impl From<Signed<alloy_consensus::TxEip7702>> for CeloTransactionRequest {
    fn from(value: Signed<alloy_consensus::TxEip7702>) -> Self {
        from_signed(value)
    }
}

impl From<Signed<TxCip64>> for CeloTransactionRequest {
    fn from(value: Signed<TxCip64>) -> Self {
        // Extract before the inner conversion drops it.
        let fee_currency = value.tx().fee_currency;
        let mut req = from_signed(value);
        req.fee_currency = fee_currency;
        req
    }
}

impl From<CeloTypedTransaction> for CeloTransactionRequest {
    fn from(tx: CeloTypedTransaction) -> Self {
        match tx {
            CeloTypedTransaction::Legacy(tx) => {
                let inner: TransactionRequest = tx.into();
                inner.into()
            }
            CeloTypedTransaction::Eip2930(tx) => {
                let inner: TransactionRequest = tx.into();
                inner.into()
            }
            CeloTypedTransaction::Eip1559(tx) => {
                let inner: TransactionRequest = tx.into();
                inner.into()
            }
            CeloTypedTransaction::Eip7702(tx) => {
                let inner: TransactionRequest = tx.into();
                inner.into()
            }
            CeloTypedTransaction::Cip64(tx) => {
                // The inner conversion has no field for `fee_currency` and drops it;
                // re-inject it so the round-trip stays CIP-64.
                let fee_currency = tx.fee_currency;
                let inner: TransactionRequest = tx.into();
                let mut req: Self = inner.into();
                req.fee_currency = fee_currency;
                req
            }
            CeloTypedTransaction::Deposit(tx) => tx.into(),
        }
    }
}

impl From<CeloTxEnvelope> for CeloTransactionRequest {
    fn from(value: CeloTxEnvelope) -> Self {
        match value {
            CeloTxEnvelope::Legacy(tx) => tx.into(),
            CeloTxEnvelope::Eip2930(tx) => tx.into(),
            CeloTxEnvelope::Eip1559(tx) => tx.into(),
            CeloTxEnvelope::Eip7702(tx) => tx.into(),
            CeloTxEnvelope::Cip64(tx) => tx.into(),
            CeloTxEnvelope::Deposit(tx) => tx.into(),
        }
    }
}

impl From<super::CeloTransaction> for CeloTransactionRequest {
    fn from(tx: super::CeloTransaction) -> Self {
        let recovered = tx.inner.into_recovered();
        let from = recovered.signer();
        let mut req: Self = recovered.into_inner().into();
        req.as_mut().from = Some(from);
        req
    }
}

impl AsRef<TransactionRequest> for CeloTransactionRequest {
    fn as_ref(&self) -> &TransactionRequest {
        self.inner.as_ref()
    }
}

impl AsMut<TransactionRequest> for CeloTransactionRequest {
    fn as_mut(&mut self) -> &mut TransactionRequest {
        self.inner.as_mut()
    }
}

impl TransactionBuilder7702 for CeloTransactionRequest {
    fn authorization_list(&self) -> Option<&Vec<SignedAuthorization>> {
        self.as_ref().authorization_list()
    }

    fn set_authorization_list(&mut self, authorization_list: Vec<SignedAuthorization>) {
        self.as_mut().set_authorization_list(authorization_list);
    }
}

#[cfg(feature = "std")]
impl alloy_network::TransactionBuilder for CeloTransactionRequest {
    fn chain_id(&self) -> Option<alloy_primitives::ChainId> {
        self.as_ref().chain_id()
    }

    fn set_chain_id(&mut self, chain_id: alloy_primitives::ChainId) {
        self.as_mut().set_chain_id(chain_id);
    }

    fn nonce(&self) -> Option<u64> {
        self.as_ref().nonce()
    }

    fn set_nonce(&mut self, nonce: u64) {
        self.as_mut().set_nonce(nonce);
    }

    fn take_nonce(&mut self) -> Option<u64> {
        self.as_mut().take_nonce()
    }

    fn input(&self) -> Option<&alloy_primitives::Bytes> {
        self.as_ref().input()
    }

    fn set_input<T: Into<alloy_primitives::Bytes>>(&mut self, input: T) {
        self.as_mut().set_input(input);
    }

    fn from(&self) -> Option<Address> {
        self.as_ref().from()
    }

    fn set_from(&mut self, from: Address) {
        self.as_mut().set_from(from);
    }

    fn kind(&self) -> Option<TxKind> {
        self.as_ref().kind()
    }

    fn clear_kind(&mut self) {
        self.as_mut().clear_kind();
    }

    fn set_kind(&mut self, kind: TxKind) {
        self.as_mut().set_kind(kind);
    }

    fn value(&self) -> Option<U256> {
        self.as_ref().value()
    }

    fn set_value(&mut self, value: U256) {
        self.as_mut().set_value(value);
    }

    fn gas_price(&self) -> Option<u128> {
        self.as_ref().gas_price()
    }

    fn set_gas_price(&mut self, gas_price: u128) {
        self.as_mut().set_gas_price(gas_price);
    }

    fn max_fee_per_gas(&self) -> Option<u128> {
        self.as_ref().max_fee_per_gas()
    }

    fn set_max_fee_per_gas(&mut self, max_fee_per_gas: u128) {
        self.as_mut().set_max_fee_per_gas(max_fee_per_gas);
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.as_ref().max_priority_fee_per_gas()
    }

    fn set_max_priority_fee_per_gas(&mut self, max_priority_fee_per_gas: u128) {
        self.as_mut().set_max_priority_fee_per_gas(max_priority_fee_per_gas);
    }

    fn gas_limit(&self) -> Option<u64> {
        self.as_ref().gas_limit()
    }

    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.as_mut().set_gas_limit(gas_limit);
    }

    fn access_list(&self) -> Option<&AccessList> {
        self.as_ref().access_list()
    }

    fn set_access_list(&mut self, access_list: AccessList) {
        self.as_mut().set_access_list(access_list);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    fn sample_fc() -> Address {
        address!("0x765DE816845861e75A25fCA122bb6898B8B1282a")
    }

    // -----------------------------------------------------------------------
    // Serde: same wire contract as celo-reth's server-side request type.
    // -----------------------------------------------------------------------

    #[test]
    fn serde_roundtrip_with_fee_currency() {
        let req = CeloTransactionRequest {
            inner: OpTransactionRequest::default().to(Address::ZERO).value(U256::from(100)),
            fee_currency: Some(sample_fc()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let deser: CeloTransactionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.fee_currency, Some(sample_fc()));
        assert_eq!(deser.inner.as_ref().value, req.inner.as_ref().value);
    }

    #[test]
    fn serde_roundtrip_without_fee_currency() {
        let req = CeloTransactionRequest {
            inner: OpTransactionRequest::default().to(Address::ZERO),
            fee_currency: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("feeCurrency"), "None must omit the key entirely: {json}");
        let deser: CeloTransactionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.fee_currency, None);
    }

    #[test]
    fn serde_zero_fee_currency_serializes_as_native() {
        // A peer that doesn't normalize would misread a zero address as a real fee currency.
        let req = CeloTransactionRequest {
            inner: OpTransactionRequest::default().to(Address::ZERO),
            fee_currency: Some(Address::ZERO),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("feeCurrency"), "zero must serialize as native (no key): {json}");
    }

    #[test]
    fn serde_null_fee_currency_deserializes_as_none() {
        let json = r#"{"feeCurrency": null, "to": "0x0000000000000000000000000000000000000000"}"#;
        let deser: CeloTransactionRequest = serde_json::from_str(json).unwrap();
        assert_eq!(deser.fee_currency, None);
    }

    #[test]
    fn serde_malformed_fee_currency_errors() {
        // An invalid `feeCurrency` must error, not silently sign and submit a CELO-fee tx.
        let json = r#"{"feeCurrency": "0xnot-an-address", "to": "0x0000000000000000000000000000000000000000"}"#;
        assert!(serde_json::from_str::<CeloTransactionRequest>(json).is_err());
    }

    #[test]
    fn serde_wrong_length_fee_currency_errors() {
        // Too-short hex must error too, not become None.
        let json =
            r#"{"feeCurrency": "0x1234", "to": "0x0000000000000000000000000000000000000000"}"#;
        assert!(serde_json::from_str::<CeloTransactionRequest>(json).is_err());
    }

    #[test]
    fn serde_fee_currency_key_is_case_insensitive() {
        // op-geth accepts any casing, so client and node must agree on the same contract.
        for key in ["feeCurrency", "feecurrency", "FeeCurrency", "FEECURRENCY", "feeCURRENCY"] {
            let json = format!(
                r#"{{"{key}": "0x765DE816845861e75A25fCA122bb6898B8B1282a", "to": "0x0000000000000000000000000000000000000000"}}"#
            );
            let deser: CeloTransactionRequest = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("key {key:?} should deserialize: {e}"));
            assert_eq!(deser.fee_currency, Some(sample_fc()), "key {key:?} did not bind");
        }
    }

    #[test]
    fn serde_lowercase_fee_currency_reserializes_canonically() {
        // A non-canonical key must come back out canonical, so forwarded requests are
        // uniform.
        let json = r#"{"feecurrency": "0x765DE816845861e75A25fCA122bb6898B8B1282a", "to": "0x0000000000000000000000000000000000000000"}"#;
        let deser: CeloTransactionRequest = serde_json::from_str(json).unwrap();
        let reser = serde_json::to_string(&deser).unwrap();
        assert!(reser.contains("\"feeCurrency\""), "expected canonical key, got: {reser}");
        assert!(!reser.contains("feecurrency"), "lowercase key leaked: {reser}");
    }

    #[test]
    fn serde_duplicate_fee_currency_aliases_error() {
        let json = r#"{
            "feeCurrency": "0x765DE816845861e75A25fCA122bb6898B8B1282a",
            "FeeCurrency": "0x0000000000000000000000000000000000000000",
            "to": "0x0000000000000000000000000000000000000000"
        }"#;
        let err = serde_json::from_str::<CeloTransactionRequest>(json).unwrap_err();
        assert!(err.to_string().contains("duplicate feeCurrency fields"));
    }

    // -----------------------------------------------------------------------
    // Building CIP-64 typed transactions.
    // -----------------------------------------------------------------------

    fn cip64_request() -> CeloTransactionRequest {
        CeloTransactionRequest::default()
            .to(Address::ZERO)
            .value(U256::from(1u64))
            .nonce(7)
            .gas_limit(100_000)
            .max_fee_per_gas(2_000_000)
            .max_priority_fee_per_gas(1_000)
            .fee_currency(sample_fc())
    }

    #[test]
    fn build_typed_tx_emits_cip64_with_fee_currency() {
        let tx = cip64_request().build_typed_tx().expect("should build");
        let CeloTypedTransaction::Cip64(tx) = tx else {
            panic!("expected CIP-64, got {tx:?}");
        };
        assert_eq!(tx.fee_currency, Some(sample_fc()));
        assert_eq!(tx.nonce, 7);
        assert_eq!(tx.max_fee_per_gas, 2_000_000);
        assert_eq!(tx.max_priority_fee_per_gas, 1_000);
    }

    #[test]
    fn build_typed_tx_without_fee_currency_emits_eip1559() {
        let req = CeloTransactionRequest::default()
            .to(Address::ZERO)
            .nonce(7)
            .gas_limit(100_000)
            .max_fee_per_gas(2_000_000)
            .max_priority_fee_per_gas(1_000);
        let tx = req.build_typed_tx().expect("should build");
        assert!(matches!(tx, CeloTypedTransaction::Eip1559(_)));
    }

    #[test]
    fn zero_fee_currency_builds_eip1559() {
        // A zero fee currency is native CELO, so the request is not CIP-64 at all.
        let req = CeloTransactionRequest::default()
            .to(Address::ZERO)
            .nonce(7)
            .gas_limit(100_000)
            .max_fee_per_gas(2_000_000)
            .max_priority_fee_per_gas(1_000)
            .fee_currency(Address::ZERO);
        assert!(!req.is_cip64(), "a zero fee currency must not mark the request as CIP-64");
        let tx = req.build_typed_tx().expect("should build");
        assert!(matches!(tx, CeloTypedTransaction::Eip1559(_)));
    }

    #[test]
    fn build_typed_tx_rejects_fee_currency_with_gas_price() {
        // gasPrice makes the inner request legacy-shaped, which conflicts with CIP-64.
        let mut req = cip64_request();
        req.as_mut().gas_price = Some(1_000_000);
        req.as_mut().max_fee_per_gas = None;
        req.as_mut().max_priority_fee_per_gas = None;
        assert!(req.build_typed_tx().is_err());
    }

    #[test]
    fn build_typed_tx_rejects_fee_currency_with_authorization_list() {
        let mut req = cip64_request();
        req.as_mut().authorization_list = Some(alloc::vec![]);
        assert!(req.build_typed_tx().is_err());
    }

    // -----------------------------------------------------------------------
    // Conversions must not lose the fee currency.
    // -----------------------------------------------------------------------

    #[test]
    fn from_typed_transaction_keeps_fee_currency() {
        let typed = cip64_request().build_typed_tx().unwrap();
        let req: CeloTransactionRequest = typed.into();
        assert_eq!(req.fee_currency, Some(sample_fc()));
        // And it still builds back into a CIP-64 tx.
        let rebuilt = req.build_typed_tx().expect("round-trip should build");
        assert!(matches!(rebuilt, CeloTypedTransaction::Cip64(_)));
    }

    #[test]
    fn native_fee_cip64_roundtrips_as_cip64() {
        // `TxCip64 { fee_currency: None }` is a valid native-fee CIP-64. The 0x7b tag must
        // survive the round-trip, or it rebuilds as EIP-1559 under a different signing hash.
        let CeloTypedTransaction::Cip64(mut tx) = cip64_request().build_typed_tx().unwrap() else {
            panic!("expected CIP-64");
        };
        tx.fee_currency = None;
        let req: CeloTransactionRequest = CeloTypedTransaction::Cip64(tx).into();
        assert!(req.is_cip64(), "0x7b tag must mark the request as CIP-64");
        let rebuilt = req.build_typed_tx().expect("round-trip should build");
        let CeloTypedTransaction::Cip64(rebuilt) = rebuilt else {
            panic!("expected CIP-64, got {rebuilt:?}");
        };
        assert_eq!(rebuilt.fee_currency, None);
    }

    #[test]
    fn explicit_type_tag_builds_native_fee_cip64() {
        let req = CeloTransactionRequest::default()
            .to(Address::ZERO)
            .nonce(7)
            .gas_limit(100_000)
            .max_fee_per_gas(2_000_000)
            .max_priority_fee_per_gas(1_000)
            .transaction_type(0x7b);
        let tx = req.build_typed_tx().expect("should build");
        let CeloTypedTransaction::Cip64(tx) = tx else {
            panic!("expected CIP-64, got {tx:?}");
        };
        assert_eq!(tx.fee_currency, None);
    }

    #[test]
    fn explicit_type_tag_with_zero_fee_currency_builds_native_fee_cip64() {
        // The 0x7b tag keeps the request CIP-64; the zero fee currency normalizes to `None`.
        let req = CeloTransactionRequest::default()
            .to(Address::ZERO)
            .nonce(7)
            .gas_limit(100_000)
            .max_fee_per_gas(2_000_000)
            .max_priority_fee_per_gas(1_000)
            .transaction_type(0x7b)
            .fee_currency(Address::ZERO);
        let tx = req.build_typed_tx().expect("should build");
        let CeloTypedTransaction::Cip64(tx) = tx else {
            panic!("expected CIP-64, got {tx:?}");
        };
        assert_eq!(tx.fee_currency, None);
    }

    #[test]
    fn from_signed_cip64_keeps_fee_currency() {
        // The direct conversion, not via the envelope, must keep the fee currency too.
        let CeloTypedTransaction::Cip64(tx) = cip64_request().build_typed_tx().unwrap() else {
            panic!("expected CIP-64");
        };
        let signed = tx.into_signed(Signature::test_signature());
        let req: CeloTransactionRequest = signed.into();
        assert_eq!(req.fee_currency, Some(sample_fc()));
    }

    #[test]
    fn from_signed_eip4844_variants_remain_supported() {
        let tx = TxEip4844 { max_fee_per_blob_gas: 42, ..Default::default() };
        let signed = tx.clone().into_signed(Signature::test_signature());
        let req: CeloTransactionRequest = signed.into();
        assert_eq!(req.as_ref().transaction_type, Some(3));
        assert_eq!(req.as_ref().max_fee_per_blob_gas, Some(42));

        let variant = TxEip4844Variant::TxEip4844(tx);
        let signed = variant.into_signed(Signature::test_signature());
        let req: CeloTransactionRequest = signed.into();
        assert_eq!(req.as_ref().transaction_type, Some(3));
        assert_eq!(req.as_ref().max_fee_per_blob_gas, Some(42));
    }

    #[test]
    fn from_envelope_keeps_fee_currency() {
        let CeloTypedTransaction::Cip64(tx) = cip64_request().build_typed_tx().unwrap() else {
            panic!("expected CIP-64");
        };
        let envelope: CeloTxEnvelope = tx.into_signed(Signature::test_signature()).into();
        let req: CeloTransactionRequest = envelope.into();
        assert_eq!(req.fee_currency, Some(sample_fc()));
    }
}
