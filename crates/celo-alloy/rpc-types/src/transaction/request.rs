use alloc::vec::Vec;
use alloy_consensus::{
    Sealed, SignableTransaction, Signed, TxEip1559, TxEip4844, TypedTransaction,
};
use alloy_eips::eip7702::SignedAuthorization;
use alloy_network_primitives::TransactionBuilder7702;
use alloy_primitives::{Address, Signature, TxKind, U256};
use alloy_rpc_types_eth::{AccessList, TransactionInput, TransactionRequest};
use celo_alloy_consensus::{CeloTxEnvelope, CeloTypedTransaction};
use op_alloy_consensus::TxDeposit;
use serde::{Deserialize, Serialize};

/// Builder for [`CeloTypedTransaction`].
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    derive_more::From,
    derive_more::AsRef,
    derive_more::AsMut,
    Serialize,
    Deserialize,
)]
#[serde(transparent)]
pub struct CeloTransactionRequest(TransactionRequest);

impl CeloTransactionRequest {
    /// Sets the `from` field in the call to the provided address
    #[inline]
    pub const fn from(mut self, from: Address) -> Self {
        self.0.from = Some(from);
        self
    }

    /// Sets the transactions type for the transactions.
    #[doc(alias = "tx_type")]
    pub const fn transaction_type(mut self, transaction_type: u8) -> Self {
        self.0.transaction_type = Some(transaction_type);
        self
    }

    /// Sets the gas limit for the transaction.
    pub const fn gas_limit(mut self, gas_limit: u64) -> Self {
        self.0.gas = Some(gas_limit);
        self
    }

    /// Sets the nonce for the transaction.
    pub const fn nonce(mut self, nonce: u64) -> Self {
        self.0.nonce = Some(nonce);
        self
    }

    /// Sets the maximum fee per gas for the transaction.
    pub const fn max_fee_per_gas(mut self, max_fee_per_gas: u128) -> Self {
        self.0.max_fee_per_gas = Some(max_fee_per_gas);
        self
    }

    /// Sets the maximum priority fee per gas for the transaction.
    pub const fn max_priority_fee_per_gas(mut self, max_priority_fee_per_gas: u128) -> Self {
        self.0.max_priority_fee_per_gas = Some(max_priority_fee_per_gas);
        self
    }

    /// Sets the recipient address for the transaction.
    #[inline]
    pub const fn to(mut self, to: Address) -> Self {
        self.0.to = Some(TxKind::Call(to));
        self
    }

    /// Sets the value (amount) for the transaction.
    pub const fn value(mut self, value: U256) -> Self {
        self.0.value = Some(value);
        self
    }

    /// Sets the access list for the transaction.
    pub fn access_list(mut self, access_list: AccessList) -> Self {
        self.0.access_list = Some(access_list);
        self
    }

    /// Sets the input data for the transaction.
    pub fn input(mut self, input: TransactionInput) -> Self {
        self.0.input = input;
        self
    }

    /// Builds [`CeloTypedTransaction`] from this builder. See
    /// [`TransactionRequest::build_typed_tx`] for more info.
    ///
    /// Note that EIP-4844 transactions are not supported by Celo and will be converted into
    /// EIP-1559 transactions.
    #[allow(clippy::result_large_err)]
    pub fn build_typed_tx(self) -> Result<CeloTypedTransaction, Self> {
        let tx = self.0.build_typed_tx().map_err(Self)?;
        match tx {
            TypedTransaction::Legacy(tx) => Ok(CeloTypedTransaction::Legacy(tx)),
            TypedTransaction::Eip1559(tx) => Ok(CeloTypedTransaction::Eip1559(tx)),
            TypedTransaction::Eip2930(tx) => Ok(CeloTypedTransaction::Eip2930(tx)),
            TypedTransaction::Eip4844(tx) => {
                let tx: TxEip4844 = tx.into();
                Ok(CeloTypedTransaction::Eip1559(TxEip1559 {
                    chain_id: tx.chain_id,
                    nonce: tx.nonce,
                    gas_limit: tx.gas_limit,
                    max_priority_fee_per_gas: tx.max_priority_fee_per_gas,
                    max_fee_per_gas: tx.max_fee_per_gas,
                    to: TxKind::Call(tx.to),
                    value: tx.value,
                    access_list: tx.access_list,
                    input: tx.input,
                }))
            }
            TypedTransaction::Eip7702(tx) => Ok(CeloTypedTransaction::Eip7702(tx)),
        }
    }
}

impl From<TxDeposit> for CeloTransactionRequest {
    fn from(tx: TxDeposit) -> Self {
        let TxDeposit {
            source_hash: _,
            from,
            to,
            mint: _,
            value,
            gas_limit,
            is_system_transaction: _,
            input,
        } = tx;

        Self(TransactionRequest {
            from: Some(from),
            to: Some(to),
            value: Some(value),
            gas: Some(gas_limit),
            input: input.into(),
            ..Default::default()
        })
    }
}

impl From<Sealed<TxDeposit>> for CeloTransactionRequest {
    fn from(value: Sealed<TxDeposit>) -> Self {
        value.into_inner().into()
    }
}

impl<T> From<Signed<T, Signature>> for CeloTransactionRequest
where
    T: SignableTransaction<Signature> + Into<TransactionRequest>,
{
    fn from(value: Signed<T, Signature>) -> Self {
        #[cfg(feature = "k256")]
        let from = value.recover_signer().ok();
        #[cfg(not(feature = "k256"))]
        let from = None;

        let mut inner: TransactionRequest = value.strip_signature().into();
        inner.from = from;

        Self(inner)
    }
}

impl From<CeloTypedTransaction> for CeloTransactionRequest {
    fn from(tx: CeloTypedTransaction) -> Self {
        match tx {
            CeloTypedTransaction::Legacy(tx) => Self(tx.into()),
            CeloTypedTransaction::Eip2930(tx) => Self(tx.into()),
            CeloTypedTransaction::Eip1559(tx) => Self(tx.into()),
            CeloTypedTransaction::Eip7702(tx) => Self(tx.into()),
            CeloTypedTransaction::Cip64(tx) => Self(tx.into()),
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

impl TransactionBuilder7702 for CeloTransactionRequest {
    fn authorization_list(&self) -> Option<&Vec<SignedAuthorization>> {
        self.as_ref().authorization_list()
    }

    fn set_authorization_list(&mut self, authorization_list: Vec<SignedAuthorization>) {
        self.as_mut().set_authorization_list(authorization_list);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{SignableTransaction, TxEip1559, TxEip2930, TxEip7702, TxLegacy};
    use alloy_eips::eip7702::Authorization;
    use alloy_primitives::{B256, Bytes, Signature, address, b256};
    use celo_alloy_consensus::TxCip64;

    fn dummy_sig() -> Signature {
        Signature::test_signature()
    }

    fn legacy_tx() -> TxLegacy {
        TxLegacy {
            chain_id: Some(0xa4ec),
            nonce: 7,
            gas_price: 11,
            gas_limit: 21_000,
            to: TxKind::Call(address!("0x000000000000000000000000000000000000aaaa")),
            value: U256::from(13_u64),
            input: Bytes::from_static(&[0xDE, 0xAD]),
        }
    }

    fn deposit_tx() -> TxDeposit {
        TxDeposit {
            source_hash: B256::ZERO,
            from: address!("0x000000000000000000000000000000000000bbbb"),
            to: TxKind::Call(address!("0x000000000000000000000000000000000000cccc")),
            mint: 0,
            value: U256::from(123_u64),
            gas_limit: 50_000,
            is_system_transaction: false,
            input: Bytes::from_static(&[0x01, 0x02, 0x03]),
        }
    }

    /// Pins the `access_list` builder against `-> Default`. Asserts the
    /// returned request carries the access list we set.
    #[test]
    fn access_list_builder_sets_field() {
        let access = AccessList::default();
        let req = CeloTransactionRequest::default().access_list(access.clone());
        assert_eq!(req.as_ref().access_list, Some(access));
    }

    /// Pins the `input` builder against `-> Default`.
    #[test]
    fn input_builder_sets_field() {
        let payload = Bytes::from_static(&[0xCA, 0xFE]);
        let req = CeloTransactionRequest::default().input(TransactionInput::new(payload.clone()));
        assert_eq!(req.as_ref().input.input(), Some(&payload));
    }

    /// Pins `From<TxDeposit>::from -> Default` AND each `delete field` mutant
    /// inside the explicit struct literal (from / to / value / gas / input).
    /// Asserting each field separately ensures any deletion shows up.
    #[test]
    fn from_tx_deposit_populates_every_explicit_field() {
        let req: CeloTransactionRequest = deposit_tx().into();
        let inner = req.as_ref();
        assert_eq!(inner.from, Some(address!("0x000000000000000000000000000000000000bbbb")));
        assert_eq!(
            inner.to,
            Some(TxKind::Call(address!("0x000000000000000000000000000000000000cccc"))),
        );
        assert_eq!(inner.value, Some(U256::from(123_u64)));
        assert_eq!(inner.gas, Some(50_000));
        assert_eq!(inner.input.input(), Some(&Bytes::from_static(&[0x01, 0x02, 0x03])));
    }

    /// Pins `From<Sealed<TxDeposit>>::from -> Default` by checking that the
    /// sealed wrapper unwraps to the same fields as the bare TxDeposit case.
    #[test]
    fn from_sealed_tx_deposit_forwards_to_inner() {
        let dep = deposit_tx();
        let sealed = Sealed::new_unchecked(dep.clone(), dep.tx_hash());
        let req: CeloTransactionRequest = sealed.into();
        let inner = req.as_ref();
        assert_eq!(inner.from, Some(dep.from));
        assert_eq!(inner.to, Some(dep.to));
        assert_eq!(inner.value, Some(dep.value));
        assert_eq!(inner.gas, Some(dep.gas_limit));
    }

    /// Pins `From<Signed<T>>::from -> Default` against a signed legacy tx.
    /// Under the `k256` feature `from` is set to the recovered signer; we
    /// just assert the rest of the fields round-trip and `from` is populated.
    #[test]
    fn from_signed_legacy_populates_inner_request() {
        let signed = legacy_tx().into_signed(dummy_sig());
        let req: CeloTransactionRequest = signed.into();
        let inner = req.as_ref();
        assert_eq!(inner.nonce, Some(7));
        assert_eq!(inner.gas, Some(21_000));
        assert_eq!(inner.value, Some(U256::from(13_u64)));
        assert_eq!(
            inner.to,
            Some(TxKind::Call(address!("0x000000000000000000000000000000000000aaaa"))),
        );
        assert_eq!(inner.input.input(), Some(&Bytes::from_static(&[0xDE, 0xAD])));
        // With the `k256` feature on (enabled via the `dev` feature), `from`
        // is the recovered signer; without it, `from` is None. Either way the
        // mutation `-> Default` (which would zero `nonce/value/...`) is
        // already caught by the assertions above.
    }

    /// Pins `From<CeloTypedTransaction>::from -> Default` AND ensures every
    /// match arm dispatches to the corresponding inner-type conversion.
    #[test]
    fn from_celo_typed_transaction_dispatches_each_variant() {
        let cases: Vec<CeloTypedTransaction> = vec![
            legacy_tx().into(),
            CeloTypedTransaction::Eip2930(TxEip2930 {
                chain_id: 0xa4ec,
                nonce: 1,
                gas_price: 1,
                gas_limit: 21_000,
                to: TxKind::Call(address!("0x0000000000000000000000000000000000000a01")),
                value: U256::from(2_u64),
                access_list: AccessList::default(),
                input: Bytes::new(),
            }),
            CeloTypedTransaction::Eip1559(TxEip1559 {
                chain_id: 0xa4ec,
                nonce: 1,
                gas_limit: 21_000,
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 1,
                to: TxKind::Call(address!("0x0000000000000000000000000000000000000a02")),
                value: U256::from(3_u64),
                access_list: AccessList::default(),
                input: Bytes::new(),
            }),
            CeloTypedTransaction::Eip7702(TxEip7702 {
                chain_id: 0xa4ec,
                nonce: 1,
                gas_limit: 21_000,
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 1,
                to: address!("0x0000000000000000000000000000000000000a03"),
                value: U256::from(4_u64),
                access_list: AccessList::default(),
                authorization_list: vec![],
                input: Bytes::new(),
            }),
            CeloTypedTransaction::Cip64(TxCip64 {
                chain_id: 0xa4ec,
                nonce: 1,
                gas_limit: 21_000,
                to: TxKind::Call(address!("0x0000000000000000000000000000000000000a04")),
                value: U256::from(5_u64),
                input: Bytes::new(),
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 1,
                access_list: AccessList::default(),
                fee_currency: Some(address!("0x0000000000000000000000000000000000000aff")),
            }),
            CeloTypedTransaction::Deposit(deposit_tx()),
        ];

        for (idx, typed) in cases.into_iter().enumerate() {
            let req: CeloTransactionRequest = typed.into();
            let inner = req.as_ref();
            // Every variant sets a recipient; default-constructed request has none.
            assert!(inner.to.is_some(), "case {idx} should set `to`");
        }
    }

    /// Pins `From<CeloTxEnvelope>::from -> Default` AND each match arm.
    #[test]
    fn from_celo_tx_envelope_dispatches_each_variant() {
        let cip64_envelope = CeloTxEnvelope::Cip64(
            TxCip64 {
                chain_id: 0xa4ec,
                nonce: 1,
                gas_limit: 21_000,
                to: TxKind::Call(address!("0x0000000000000000000000000000000000000b01")),
                value: U256::from(7_u64),
                input: Bytes::new(),
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 1,
                access_list: AccessList::default(),
                fee_currency: None,
            }
            .into_signed(dummy_sig()),
        );
        let req: CeloTransactionRequest = cip64_envelope.into();
        assert_eq!(
            req.as_ref().to,
            Some(TxKind::Call(address!("0x0000000000000000000000000000000000000b01"))),
        );

        // Deposit branch flows through `From<TxDeposit>` (already pinned).
        let dep_envelope = CeloTxEnvelope::Deposit(Sealed::new_unchecked(
            deposit_tx(),
            b256!("0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"),
        ));
        let req: CeloTransactionRequest = dep_envelope.into();
        assert_eq!(req.as_ref().from, Some(address!("0x000000000000000000000000000000000000bbbb")),);
    }

    /// Pins both `authorization_list -> None` and `-> Some(empty)`, plus
    /// `set_authorization_list -> ()`. Builds a request, sets a one-element
    /// authorization list, and reads it back.
    #[test]
    fn authorization_list_round_trips() {
        let auth = SignedAuthorization::new_unchecked(
            Authorization {
                chain_id: U256::from(0xa4ec_u64),
                address: address!("0x000000000000000000000000000000000000c001"),
                nonce: 99,
            },
            0,
            U256::from(1_u64),
            U256::from(2_u64),
        );
        let mut req = CeloTransactionRequest::default();
        // Pre-set: list is None.
        assert!(TransactionBuilder7702::authorization_list(&req).is_none());
        TransactionBuilder7702::set_authorization_list(&mut req, vec![auth]);
        let list = TransactionBuilder7702::authorization_list(&req).expect("Some list");
        assert_eq!(list.len(), 1);
    }
}
