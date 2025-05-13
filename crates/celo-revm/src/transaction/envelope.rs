use crate::{CeloTxType, CeloTypedTransaction, TxCip64};
use alloy_consensus::{
    Sealable, Sealed, SignableTransaction, Signed, Transaction, TxEip1559, TxEip2930, TxEip7702,
    TxEnvelope, TxLegacy, Typed2718, transaction::RlpEcdsaDecodableTx,
};
use alloy_eips::{
    eip2718::{Decodable2718, Eip2718Error, Eip2718Result, Encodable2718, IsTyped2718},
    eip2930::AccessList,
    eip7702::SignedAuthorization,
};
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256};
use alloy_rlp::{Decodable, Encodable};
use op_alloy_consensus::{OpTxType, TxDeposit};

/// The Ethereum [EIP-2718] Transaction Envelope, modified for Celo.
///
/// # Note:
///
/// This enum distinguishes between tagged and untagged legacy transactions, as
/// the in-protocol merkle tree may commit to EITHER 0-prefixed or raw.
/// Therefore we must ensure that encoding returns the precise byte-array that
/// was decoded, preserving the presence or absence of the `TransactionType`
/// flag.
///
/// [EIP-2718]: https://eips.ethereum.org/EIPS/eip-2718
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(
        into = "serde_from::TaggedTxEnvelope",
        from = "serde_from::MaybeTaggedTxEnvelope"
    )
)]
#[cfg_attr(
    all(any(test, feature = "arbitrary"), feature = "k256"),
    derive(arbitrary::Arbitrary)
)]
pub enum CeloTxEnvelope {
    /// An untagged [`TxLegacy`].
    Legacy(Signed<TxLegacy>),
    /// A [`TxEip2930`] tagged with type 1.
    Eip2930(Signed<TxEip2930>),
    /// A [`TxEip1559`] tagged with type 2.
    Eip1559(Signed<TxEip1559>),
    /// A [`TxEip7702`] tagged with type 4.
    Eip7702(Signed<TxEip7702>),
    /// A [`TxCip64`] tagged with type 0x7B.
    Cip64(Signed<TxCip64>),
    /// A [`TxDeposit`] tagged with type 0x7E.
    Deposit(Sealed<TxDeposit>),
}

impl AsRef<Self> for CeloTxEnvelope {
    fn as_ref(&self) -> &Self {
        self
    }
}

impl From<Signed<TxLegacy>> for CeloTxEnvelope {
    fn from(v: Signed<TxLegacy>) -> Self {
        Self::Legacy(v)
    }
}

impl From<Signed<TxEip2930>> for CeloTxEnvelope {
    fn from(v: Signed<TxEip2930>) -> Self {
        Self::Eip2930(v)
    }
}

impl From<Signed<TxEip1559>> for CeloTxEnvelope {
    fn from(v: Signed<TxEip1559>) -> Self {
        Self::Eip1559(v)
    }
}

impl From<Signed<TxEip7702>> for CeloTxEnvelope {
    fn from(v: Signed<TxEip7702>) -> Self {
        Self::Eip7702(v)
    }
}

impl From<Signed<TxCip64>> for CeloTxEnvelope {
    fn from(v: Signed<TxCip64>) -> Self {
        Self::Cip64(v)
    }
}

impl From<TxDeposit> for CeloTxEnvelope {
    fn from(v: TxDeposit) -> Self {
        v.seal_slow().into()
    }
}

impl From<Signed<CeloTypedTransaction>> for CeloTxEnvelope {
    fn from(value: Signed<CeloTypedTransaction>) -> Self {
        let (tx, sig, hash) = value.into_parts();
        match tx {
            CeloTypedTransaction::Legacy(tx_legacy) => {
                let tx = Signed::new_unchecked(tx_legacy, sig, hash);
                Self::Legacy(tx)
            }
            CeloTypedTransaction::Eip2930(tx_eip2930) => {
                let tx = Signed::new_unchecked(tx_eip2930, sig, hash);
                Self::Eip2930(tx)
            }
            CeloTypedTransaction::Eip1559(tx_eip1559) => {
                let tx = Signed::new_unchecked(tx_eip1559, sig, hash);
                Self::Eip1559(tx)
            }
            CeloTypedTransaction::Eip7702(tx_eip7702) => {
                let tx = Signed::new_unchecked(tx_eip7702, sig, hash);
                Self::Eip7702(tx)
            }
            CeloTypedTransaction::Cip64(tx_cip64) => {
                let tx = Signed::new_unchecked(tx_cip64, sig, hash);
                Self::Cip64(tx)
            }
            CeloTypedTransaction::Deposit(tx) => Self::Deposit(Sealed::new_unchecked(tx, hash)),
        }
    }
}

impl From<(CeloTypedTransaction, Signature)> for CeloTxEnvelope {
    fn from(value: (CeloTypedTransaction, Signature)) -> Self {
        Self::new_unhashed(value.0, value.1)
    }
}

impl From<Sealed<TxDeposit>> for CeloTxEnvelope {
    fn from(v: Sealed<TxDeposit>) -> Self {
        Self::Deposit(v)
    }
}

impl TryFrom<TxEnvelope> for CeloTxEnvelope {
    type Error = TxEnvelope;

    fn try_from(value: TxEnvelope) -> Result<Self, Self::Error> {
        Self::try_from_eth_envelope(value)
    }
}

impl TryFrom<CeloTxEnvelope> for TxEnvelope {
    type Error = CeloTxEnvelope;

    fn try_from(value: CeloTxEnvelope) -> Result<Self, Self::Error> {
        value.try_into_eth_envelope()
    }
}

impl TryFrom<CeloTxEnvelope> for Signed<CeloTypedTransaction> {
    type Error = CeloTxEnvelope;

    fn try_from(value: CeloTxEnvelope) -> Result<Self, Self::Error> {
        value.try_into_signed()
    }
}

impl Typed2718 for CeloTxEnvelope {
    fn ty(&self) -> u8 {
        match self {
            Self::Legacy(tx) => tx.tx().ty(),
            Self::Eip2930(tx) => tx.tx().ty(),
            Self::Eip1559(tx) => tx.tx().ty(),
            Self::Eip7702(tx) => tx.tx().ty(),
            Self::Cip64(tx) => tx.tx().ty(),
            Self::Deposit(tx) => tx.ty(),
        }
    }
}

impl IsTyped2718 for CeloTxEnvelope {
    fn is_type(type_id: u8) -> bool {
        <CeloTxType as IsTyped2718>::is_type(type_id)
    }
}

impl Transaction for CeloTxEnvelope {
    fn chain_id(&self) -> Option<u64> {
        match self {
            Self::Legacy(tx) => tx.tx().chain_id(),
            Self::Eip2930(tx) => tx.tx().chain_id(),
            Self::Eip1559(tx) => tx.tx().chain_id(),
            Self::Eip7702(tx) => tx.tx().chain_id(),
            Self::Cip64(tx) => tx.tx().chain_id(),
            Self::Deposit(tx) => tx.chain_id(),
        }
    }

    fn nonce(&self) -> u64 {
        match self {
            Self::Legacy(tx) => tx.tx().nonce(),
            Self::Eip2930(tx) => tx.tx().nonce(),
            Self::Eip1559(tx) => tx.tx().nonce(),
            Self::Eip7702(tx) => tx.tx().nonce(),
            Self::Cip64(tx) => tx.tx().nonce(),
            Self::Deposit(tx) => tx.nonce(),
        }
    }

    fn gas_limit(&self) -> u64 {
        match self {
            Self::Legacy(tx) => tx.tx().gas_limit(),
            Self::Eip2930(tx) => tx.tx().gas_limit(),
            Self::Eip1559(tx) => tx.tx().gas_limit(),
            Self::Eip7702(tx) => tx.tx().gas_limit(),
            Self::Cip64(tx) => tx.tx().gas_limit(),
            Self::Deposit(tx) => tx.gas_limit(),
        }
    }

    fn gas_price(&self) -> Option<u128> {
        match self {
            Self::Legacy(tx) => tx.tx().gas_price(),
            Self::Eip2930(tx) => tx.tx().gas_price(),
            Self::Eip1559(tx) => tx.tx().gas_price(),
            Self::Eip7702(tx) => tx.tx().gas_price(),
            Self::Cip64(tx) => tx.tx().gas_price(),
            Self::Deposit(tx) => tx.gas_price(),
        }
    }

    fn max_fee_per_gas(&self) -> u128 {
        match self {
            Self::Legacy(tx) => tx.tx().max_fee_per_gas(),
            Self::Eip2930(tx) => tx.tx().max_fee_per_gas(),
            Self::Eip1559(tx) => tx.tx().max_fee_per_gas(),
            Self::Eip7702(tx) => tx.tx().max_fee_per_gas(),
            Self::Cip64(tx) => tx.tx().max_fee_per_gas(),
            Self::Deposit(tx) => tx.max_fee_per_gas(),
        }
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        match self {
            Self::Legacy(tx) => tx.tx().max_priority_fee_per_gas(),
            Self::Eip2930(tx) => tx.tx().max_priority_fee_per_gas(),
            Self::Eip1559(tx) => tx.tx().max_priority_fee_per_gas(),
            Self::Eip7702(tx) => tx.tx().max_priority_fee_per_gas(),
            Self::Cip64(tx) => tx.tx().max_priority_fee_per_gas(),
            Self::Deposit(tx) => tx.max_priority_fee_per_gas(),
        }
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        match self {
            Self::Legacy(tx) => tx.tx().max_fee_per_blob_gas(),
            Self::Eip2930(tx) => tx.tx().max_fee_per_blob_gas(),
            Self::Eip1559(tx) => tx.tx().max_fee_per_blob_gas(),
            Self::Eip7702(tx) => tx.tx().max_fee_per_blob_gas(),
            Self::Cip64(tx) => tx.tx().max_fee_per_blob_gas(),
            Self::Deposit(tx) => tx.max_fee_per_blob_gas(),
        }
    }

    fn priority_fee_or_price(&self) -> u128 {
        match self {
            Self::Legacy(tx) => tx.tx().priority_fee_or_price(),
            Self::Eip2930(tx) => tx.tx().priority_fee_or_price(),
            Self::Eip1559(tx) => tx.tx().priority_fee_or_price(),
            Self::Eip7702(tx) => tx.tx().priority_fee_or_price(),
            Self::Cip64(tx) => tx.tx().priority_fee_or_price(),
            Self::Deposit(tx) => tx.priority_fee_or_price(),
        }
    }

    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        match self {
            Self::Legacy(tx) => tx.tx().effective_gas_price(base_fee),
            Self::Eip2930(tx) => tx.tx().effective_gas_price(base_fee),
            Self::Eip1559(tx) => tx.tx().effective_gas_price(base_fee),
            Self::Eip7702(tx) => tx.tx().effective_gas_price(base_fee),
            Self::Cip64(tx) => tx.tx().effective_gas_price(base_fee),
            Self::Deposit(tx) => tx.effective_gas_price(base_fee),
        }
    }

    fn is_dynamic_fee(&self) -> bool {
        match self {
            Self::Legacy(tx) => tx.tx().is_dynamic_fee(),
            Self::Eip2930(tx) => tx.tx().is_dynamic_fee(),
            Self::Eip1559(tx) => tx.tx().is_dynamic_fee(),
            Self::Eip7702(tx) => tx.tx().is_dynamic_fee(),
            Self::Cip64(tx) => tx.tx().is_dynamic_fee(),
            Self::Deposit(tx) => tx.is_dynamic_fee(),
        }
    }

    fn kind(&self) -> TxKind {
        match self {
            Self::Legacy(tx) => tx.tx().kind(),
            Self::Eip2930(tx) => tx.tx().kind(),
            Self::Eip1559(tx) => tx.tx().kind(),
            Self::Eip7702(tx) => tx.tx().kind(),
            Self::Cip64(tx) => tx.tx().kind(),
            Self::Deposit(tx) => tx.kind(),
        }
    }

    fn is_create(&self) -> bool {
        match self {
            Self::Legacy(tx) => tx.tx().is_create(),
            Self::Eip2930(tx) => tx.tx().is_create(),
            Self::Eip1559(tx) => tx.tx().is_create(),
            Self::Eip7702(tx) => tx.tx().is_create(),
            Self::Cip64(tx) => tx.tx().is_create(),
            Self::Deposit(tx) => tx.is_create(),
        }
    }

    fn to(&self) -> Option<Address> {
        match self {
            Self::Legacy(tx) => tx.tx().to(),
            Self::Eip2930(tx) => tx.tx().to(),
            Self::Eip1559(tx) => tx.tx().to(),
            Self::Eip7702(tx) => tx.tx().to(),
            Self::Cip64(tx) => tx.tx().to(),
            Self::Deposit(tx) => tx.to(),
        }
    }

    fn value(&self) -> U256 {
        match self {
            Self::Legacy(tx) => tx.tx().value(),
            Self::Eip2930(tx) => tx.tx().value(),
            Self::Eip1559(tx) => tx.tx().value(),
            Self::Eip7702(tx) => tx.tx().value(),
            Self::Cip64(tx) => tx.tx().value(),
            Self::Deposit(tx) => tx.value(),
        }
    }

    fn input(&self) -> &Bytes {
        match self {
            Self::Legacy(tx) => tx.tx().input(),
            Self::Eip2930(tx) => tx.tx().input(),
            Self::Eip1559(tx) => tx.tx().input(),
            Self::Eip7702(tx) => tx.tx().input(),
            Self::Cip64(tx) => tx.tx().input(),
            Self::Deposit(tx) => tx.input(),
        }
    }

    fn access_list(&self) -> Option<&AccessList> {
        match self {
            Self::Legacy(tx) => tx.tx().access_list(),
            Self::Eip2930(tx) => tx.tx().access_list(),
            Self::Eip1559(tx) => tx.tx().access_list(),
            Self::Eip7702(tx) => tx.tx().access_list(),
            Self::Cip64(tx) => tx.tx().access_list(),
            Self::Deposit(tx) => tx.access_list(),
        }
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        match self {
            Self::Legacy(tx) => tx.tx().blob_versioned_hashes(),
            Self::Eip2930(tx) => tx.tx().blob_versioned_hashes(),
            Self::Eip1559(tx) => tx.tx().blob_versioned_hashes(),
            Self::Eip7702(tx) => tx.tx().blob_versioned_hashes(),
            Self::Cip64(tx) => tx.tx().blob_versioned_hashes(),
            Self::Deposit(tx) => tx.blob_versioned_hashes(),
        }
    }

    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        match self {
            Self::Legacy(tx) => tx.tx().authorization_list(),
            Self::Eip2930(tx) => tx.tx().authorization_list(),
            Self::Eip1559(tx) => tx.tx().authorization_list(),
            Self::Eip7702(tx) => tx.tx().authorization_list(),
            Self::Cip64(tx) => tx.tx().authorization_list(),
            Self::Deposit(tx) => tx.authorization_list(),
        }
    }
}

impl CeloTxEnvelope {
    /// Creates a new enveloped transaction from the given transaction, signature and hash.
    ///
    /// Caution: This assumes the given hash is the correct transaction hash.
    pub fn new_unchecked(
        transaction: CeloTypedTransaction,
        signature: Signature,
        hash: B256,
    ) -> Self {
        Signed::new_unchecked(transaction, signature, hash).into()
    }

    /// Creates a new signed transaction from the given typed transaction and signature without the
    /// hash.
    ///
    /// Note: this only calculates the hash on the first [`CeloTxEnvelope::hash`] call.
    pub fn new_unhashed(transaction: CeloTypedTransaction, signature: Signature) -> Self {
        transaction.into_signed(signature).into()
    }

    /// Consumes the type, removes the signature and returns the transaction.
    #[inline]
    pub fn into_typed_transaction(self) -> CeloTypedTransaction {
        match self {
            Self::Legacy(tx) => CeloTypedTransaction::Legacy(tx.into_parts().0),
            Self::Eip2930(tx) => CeloTypedTransaction::Eip2930(tx.into_parts().0),
            Self::Eip1559(tx) => CeloTypedTransaction::Eip1559(tx.into_parts().0),
            Self::Eip7702(tx) => CeloTypedTransaction::Eip7702(tx.into_parts().0),
            Self::Cip64(tx) => CeloTypedTransaction::Cip64(tx.into_parts().0),
            Self::Deposit(tx) => CeloTypedTransaction::Deposit(tx.into_parts().0),
        }
    }

    /// Returns true if the transaction is a legacy transaction.
    #[inline]
    pub const fn is_legacy(&self) -> bool {
        matches!(self, Self::Legacy(_))
    }

    /// Returns true if the transaction is an EIP-2930 transaction.
    #[inline]
    pub const fn is_eip2930(&self) -> bool {
        matches!(self, Self::Eip2930(_))
    }

    /// Returns true if the transaction is an EIP-1559 transaction.
    #[inline]
    pub const fn is_eip1559(&self) -> bool {
        matches!(self, Self::Eip1559(_))
    }

    /// Returns true if the transaction is a CIP-64 transaction.
    #[inline]
    pub const fn is_cip64(&self) -> bool {
        matches!(self, Self::Cip64(_))
    }

    /// Returns true if the transaction is a system transaction.
    #[inline]
    pub const fn is_system_transaction(&self) -> bool {
        match self {
            Self::Deposit(tx) => tx.inner().is_system_transaction,
            _ => false,
        }
    }

    /// Attempts to convert the celo variant into an ethereum [`TxEnvelope`].
    ///
    /// Returns the envelope as error if it is a variant unsupported on ethereum: [`TxDeposit`, `TxCip64`]
    pub fn try_into_eth_envelope(self) -> Result<TxEnvelope, Self> {
        match self {
            Self::Legacy(tx) => Ok(tx.into()),
            Self::Eip2930(tx) => Ok(tx.into()),
            Self::Eip1559(tx) => Ok(tx.into()),
            Self::Eip7702(tx) => Ok(tx.into()),
            tx @ Self::Cip64(_) => Err(tx),
            tx @ Self::Deposit(_) => Err(tx),
        }
    }

    /// Attempts to convert an ethereum [`TxEnvelope`] into the celo variant.
    ///
    /// Returns the given envelope as error if [`CeloTxEnvelope`] doesn't support the variant
    /// (EIP-4844)
    pub fn try_from_eth_envelope(tx: TxEnvelope) -> Result<Self, TxEnvelope> {
        match tx {
            TxEnvelope::Legacy(tx) => Ok(tx.into()),
            TxEnvelope::Eip2930(tx) => Ok(tx.into()),
            TxEnvelope::Eip1559(tx) => Ok(tx.into()),
            tx @ TxEnvelope::Eip4844(_) => Err(tx),
            TxEnvelope::Eip7702(tx) => Ok(tx.into()),
        }
    }

    /// Recover the signer of the transaction.
    ///
    /// If this transaction is a [`TxDeposit`] transaction this returns the deposit transaction's
    /// `from` address.
    #[cfg(feature = "k256")]
    pub fn recover_signer(&self) -> Result<Address, alloy_primitives::SignatureError> {
        match self {
            Self::Legacy(tx) => tx.recover_signer(),
            Self::Eip2930(tx) => tx.recover_signer(),
            Self::Eip1559(tx) => tx.recover_signer(),
            Self::Eip7702(tx) => tx.recover_signer(),
            Self::Cip64(tx) => tx.recover_signer(),
            Self::Deposit(tx) => Ok(tx.inner().from),
        }
    }

    /// Recover the signer and return a new [`alloy_consensus::transaction::Recovered`] instance
    /// containing both the transaction and the recovered signer address.
    ///
    /// If this transaction is a [`TxDeposit`] transaction this returns the deposit transaction's
    /// `from` address.
    #[cfg(feature = "k256")]
    pub fn try_into_recovered(
        self,
    ) -> Result<alloy_consensus::transaction::Recovered<Self>, alloy_primitives::SignatureError>
    {
        let signer = self.recover_signer()?;
        Ok(alloy_consensus::transaction::Recovered::new_unchecked(
            self, signer,
        ))
    }

    /// Recover the signer of the transaction and returns a `Recovered<&Self>`
    #[cfg(feature = "k256")]
    pub fn try_to_recovered_ref(
        &self,
    ) -> Result<alloy_consensus::transaction::Recovered<&Self>, alloy_primitives::SignatureError>
    {
        let signer = self.recover_signer()?;
        Ok(alloy_consensus::transaction::Recovered::new_unchecked(
            self, signer,
        ))
    }

    /// Returns mutable access to the input bytes.
    ///
    /// Caution: modifying this will cause side-effects on the hash.
    #[doc(hidden)]
    pub fn input_mut(&mut self) -> &mut Bytes {
        match self {
            Self::Eip1559(tx) => &mut tx.tx_mut().input,
            Self::Eip2930(tx) => &mut tx.tx_mut().input,
            Self::Legacy(tx) => &mut tx.tx_mut().input,
            Self::Eip7702(tx) => &mut tx.tx_mut().input,
            Self::Cip64(tx) => &mut tx.tx_mut().input,
            Self::Deposit(tx) => &mut tx.inner_mut().input,
        }
    }

    /// Returns true if the transaction is a deposit transaction.
    #[inline]
    pub const fn is_deposit(&self) -> bool {
        matches!(self, Self::Deposit(_))
    }

    /// Returns the [`TxLegacy`] variant if the transaction is a legacy transaction.
    pub const fn as_legacy(&self) -> Option<&Signed<TxLegacy>> {
        match self {
            Self::Legacy(tx) => Some(tx),
            _ => None,
        }
    }

    /// Returns the [`TxEip2930`] variant if the transaction is an EIP-2930 transaction.
    pub const fn as_eip2930(&self) -> Option<&Signed<TxEip2930>> {
        match self {
            Self::Eip2930(tx) => Some(tx),
            _ => None,
        }
    }

    /// Returns the [`TxEip1559`] variant if the transaction is an EIP-1559 transaction.
    pub const fn as_eip1559(&self) -> Option<&Signed<TxEip1559>> {
        match self {
            Self::Eip1559(tx) => Some(tx),
            _ => None,
        }
    }

    /// Returns the [`TxCip64`] variant if the transaction is a CIP-64 transaction.
    pub const fn as_cip64(&self) -> Option<&Signed<TxCip64>> {
        match self {
            Self::Cip64(tx) => Some(tx),
            _ => None,
        }
    }

    /// Returns the [`TxDeposit`] variant if the transaction is a deposit transaction.
    pub const fn as_deposit(&self) -> Option<&Sealed<TxDeposit>> {
        match self {
            Self::Deposit(tx) => Some(tx),
            _ => None,
        }
    }

    /// Return the [`CeloTxType`] of the inner txn.
    pub const fn tx_type(&self) -> CeloTxType {
        match self {
            Self::Legacy(_) => CeloTxType::NonCeloTx(OpTxType::Legacy),
            Self::Eip2930(_) => CeloTxType::NonCeloTx(OpTxType::Eip2930),
            Self::Eip1559(_) => CeloTxType::NonCeloTx(OpTxType::Eip1559),
            Self::Eip7702(_) => CeloTxType::NonCeloTx(OpTxType::Eip7702),
            Self::Cip64(_) => CeloTxType::Cip64,
            Self::Deposit(_) => CeloTxType::NonCeloTx(OpTxType::Deposit),
        }
    }

    /// Attempts to consume the type into a [`Signed`].
    ///
    /// Returns the envelope as error if it is a variant not converted to signed txn: [`TxDeposit`]
    pub fn try_into_signed(self) -> Result<Signed<CeloTypedTransaction>, Self> {
        match self {
            Self::Legacy(tx) => Ok(tx.convert()),
            Self::Eip2930(tx) => Ok(tx.convert()),
            Self::Eip1559(tx) => Ok(tx.convert()),
            Self::Eip7702(tx) => Ok(tx.convert()),
            Self::Cip64(tx) => Ok(tx.convert()),
            tx @ Self::Deposit(_) => Err(tx),
        }
    }

    /// Return the hash of the inner Signed.
    #[doc(alias = "transaction_hash")]
    pub fn tx_hash(&self) -> &B256 {
        match self {
            Self::Legacy(tx) => tx.hash(),
            Self::Eip2930(tx) => tx.hash(),
            Self::Eip1559(tx) => tx.hash(),
            Self::Eip7702(tx) => tx.hash(),
            Self::Cip64(tx) => tx.hash(),
            Self::Deposit(tx) => tx.hash_ref(),
        }
    }

    /// Reference to transaction hash. Used to identify transaction.
    pub fn hash(&self) -> &B256 {
        match self {
            Self::Legacy(tx) => tx.hash(),
            Self::Eip2930(tx) => tx.hash(),
            Self::Eip1559(tx) => tx.hash(),
            Self::Eip7702(tx) => tx.hash(),
            Self::Cip64(tx) => tx.hash(),
            Self::Deposit(tx) => tx.hash_ref(),
        }
    }

    /// Return the length of the inner txn, including type byte length
    pub fn eip2718_encoded_length(&self) -> usize {
        match self {
            Self::Legacy(t) => t.eip2718_encoded_length(),
            Self::Eip2930(t) => t.eip2718_encoded_length(),
            Self::Eip1559(t) => t.eip2718_encoded_length(),
            Self::Eip7702(t) => t.eip2718_encoded_length(),
            Self::Cip64(t) => t.eip2718_encoded_length(),
            Self::Deposit(t) => t.eip2718_encoded_length(),
        }
    }
}

impl Encodable for CeloTxEnvelope {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.network_encode(out)
    }

    fn length(&self) -> usize {
        self.network_len()
    }
}

impl Decodable for CeloTxEnvelope {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Ok(Self::network_decode(buf)?)
    }
}

impl Decodable2718 for CeloTxEnvelope {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        match ty
            .try_into()
            .map_err(|_| Eip2718Error::UnexpectedType(ty))?
        {
            CeloTxType::NonCeloTx(OpTxType::Eip2930) => {
                Ok(Self::Eip2930(TxEip2930::rlp_decode_signed(buf)?))
            }
            CeloTxType::NonCeloTx(OpTxType::Eip1559) => {
                Ok(Self::Eip1559(TxEip1559::rlp_decode_signed(buf)?))
            }
            CeloTxType::NonCeloTx(OpTxType::Eip7702) => {
                Ok(Self::Eip7702(TxEip7702::rlp_decode_signed(buf)?))
            }
            CeloTxType::Cip64 => Ok(Self::Cip64(TxCip64::rlp_decode_signed(buf)?)),
            CeloTxType::NonCeloTx(OpTxType::Deposit) => {
                Ok(Self::Deposit(TxDeposit::decode(buf)?.seal_slow()))
            }
            CeloTxType::NonCeloTx(OpTxType::Legacy) => Err(alloy_rlp::Error::Custom(
                "type-0 eip2718 transactions are not supported",
            )
            .into()),
        }
    }

    fn fallback_decode(buf: &mut &[u8]) -> Eip2718Result<Self> {
        Ok(Self::Legacy(TxLegacy::rlp_decode_signed(buf)?))
    }
}

impl Encodable2718 for CeloTxEnvelope {
    fn type_flag(&self) -> Option<u8> {
        match self {
            Self::Legacy(_) => None,
            Self::Eip2930(_) => Some(OpTxType::Eip2930 as u8),
            Self::Eip1559(_) => Some(OpTxType::Eip1559 as u8),
            Self::Eip7702(_) => Some(OpTxType::Eip7702 as u8),
            Self::Cip64(_) => Some(CeloTxType::Cip64.into()),
            Self::Deposit(_) => Some(OpTxType::Deposit as u8),
        }
    }

    fn encode_2718_len(&self) -> usize {
        self.eip2718_encoded_length()
    }

    fn encode_2718(&self, out: &mut dyn alloy_rlp::BufMut) {
        match self {
            // Legacy transactions have no difference between network and 2718
            Self::Legacy(tx) => tx.eip2718_encode(out),
            Self::Eip2930(tx) => {
                tx.eip2718_encode(out);
            }
            Self::Eip1559(tx) => {
                tx.eip2718_encode(out);
            }
            Self::Eip7702(tx) => {
                tx.eip2718_encode(out);
            }
            Self::Cip64(tx) => {
                tx.eip2718_encode(out);
            }
            Self::Deposit(tx) => {
                tx.encode_2718(out);
            }
        }
    }

    fn trie_hash(&self) -> B256 {
        match self {
            Self::Legacy(tx) => *tx.hash(),
            Self::Eip1559(tx) => *tx.hash(),
            Self::Eip2930(tx) => *tx.hash(),
            Self::Eip7702(tx) => *tx.hash(),
            Self::Cip64(tx) => *tx.hash(),
            Self::Deposit(tx) => tx.seal(),
        }
    }
}

#[cfg(feature = "serde")]
mod serde_from {
    //! NB: Why do we need this?
    //!
    //! Because the tag may be missing, we need an abstraction over tagged (with
    //! type) and untagged (always legacy). This is [`MaybeTaggedTxEnvelope`].
    //!
    //! The tagged variant is [`TaggedTxEnvelope`], which always has a type tag.
    //!
    //! We serialize via [`TaggedTxEnvelope`] and deserialize via
    //! [`MaybeTaggedTxEnvelope`].
    use super::*;

    #[derive(Debug, serde::Deserialize)]
    #[serde(untagged)]
    pub(crate) enum MaybeTaggedTxEnvelope {
        Tagged(TaggedTxEnvelope),
        #[serde(with = "alloy_consensus::transaction::signed_legacy_serde")]
        Untagged(Signed<TxLegacy>),
    }

    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    #[serde(tag = "type")]
    pub(crate) enum TaggedTxEnvelope {
        #[serde(
            rename = "0x0",
            alias = "0x00",
            with = "alloy_consensus::transaction::signed_legacy_serde"
        )]
        Legacy(Signed<TxLegacy>),
        #[serde(rename = "0x1", alias = "0x01")]
        Eip2930(Signed<TxEip2930>),
        #[serde(rename = "0x2", alias = "0x02")]
        Eip1559(Signed<TxEip1559>),
        #[serde(rename = "0x4", alias = "0x04")]
        Eip7702(Signed<TxEip7702>),
        #[serde(rename = "0x7b", alias = "0x7B")]
        Cip64(Signed<TxCip64>),
        #[serde(
            rename = "0x7e",
            alias = "0x7E",
            serialize_with = "op_alloy_consensus::serde_deposit_tx_rpc"
        )]
        Deposit(Sealed<TxDeposit>),
    }

    impl From<MaybeTaggedTxEnvelope> for CeloTxEnvelope {
        fn from(value: MaybeTaggedTxEnvelope) -> Self {
            match value {
                MaybeTaggedTxEnvelope::Tagged(tagged) => tagged.into(),
                MaybeTaggedTxEnvelope::Untagged(tx) => Self::Legacy(tx),
            }
        }
    }

    impl From<TaggedTxEnvelope> for CeloTxEnvelope {
        fn from(value: TaggedTxEnvelope) -> Self {
            match value {
                TaggedTxEnvelope::Legacy(signed) => Self::Legacy(signed),
                TaggedTxEnvelope::Eip2930(signed) => Self::Eip2930(signed),
                TaggedTxEnvelope::Eip1559(signed) => Self::Eip1559(signed),
                TaggedTxEnvelope::Eip7702(signed) => Self::Eip7702(signed),
                TaggedTxEnvelope::Cip64(signed) => Self::Cip64(signed),
                TaggedTxEnvelope::Deposit(tx) => Self::Deposit(tx),
            }
        }
    }

    impl From<CeloTxEnvelope> for TaggedTxEnvelope {
        fn from(value: CeloTxEnvelope) -> Self {
            match value {
                CeloTxEnvelope::Legacy(signed) => Self::Legacy(signed),
                CeloTxEnvelope::Eip2930(signed) => Self::Eip2930(signed),
                CeloTxEnvelope::Eip1559(signed) => Self::Eip1559(signed),
                CeloTxEnvelope::Eip7702(signed) => Self::Eip7702(signed),
                CeloTxEnvelope::Cip64(signed) => Self::Cip64(signed),
                CeloTxEnvelope::Deposit(tx) => Self::Deposit(tx),
            }
        }
    }
}

/// Bincode-compatible serde implementation for CeloTxEnvelope.
#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub mod serde_bincode_compat {
    use crate::transaction::serde_bincode_compat::TxCip64;
    use alloy_consensus::{
        Sealed, Signed,
        transaction::serde_bincode_compat::{TxEip1559, TxEip2930, TxEip7702, TxLegacy},
    };
    use alloy_primitives::{B256, Signature};
    use op_alloy_consensus::serde_bincode_compat::TxDeposit;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_with::{DeserializeAs, SerializeAs};

    /// Bincode-compatible representation of a CeloTxEnvelope.
    #[derive(Debug, Serialize, Deserialize)]
    pub enum CeloTxEnvelope<'a> {
        /// Legacy variant.
        Legacy {
            /// Transaction signature.
            signature: Signature,
            /// Borrowed legacy transaction data.
            transaction: TxLegacy<'a>,
        },
        /// EIP-2930 variant.
        Eip2930 {
            /// Transaction signature.
            signature: Signature,
            /// Borrowed EIP-2930 transaction data.
            transaction: TxEip2930<'a>,
        },
        /// EIP-1559 variant.
        Eip1559 {
            /// Transaction signature.
            signature: Signature,
            /// Borrowed EIP-1559 transaction data.
            transaction: TxEip1559<'a>,
        },
        /// EIP-7702 variant.
        Eip7702 {
            /// Transaction signature.
            signature: Signature,
            /// Borrowed EIP-7702 transaction data.
            transaction: TxEip7702<'a>,
        },
        /// CIP-64 variant.
        Cip64 {
            /// Transaction signature.
            signature: Signature,
            /// Borrowed CIP-64 transaction data.
            transaction: TxCip64<'a>,
        },
        /// Deposit variant.
        Deposit {
            /// Precomputed hash.
            hash: B256,
            /// Borrowed deposit transaction data.
            transaction: TxDeposit<'a>,
        },
    }

    impl<'a> From<&'a super::CeloTxEnvelope> for CeloTxEnvelope<'a> {
        fn from(value: &'a super::CeloTxEnvelope) -> Self {
            match value {
                super::CeloTxEnvelope::Legacy(signed_legacy) => Self::Legacy {
                    signature: *signed_legacy.signature(),
                    transaction: signed_legacy.tx().into(),
                },
                super::CeloTxEnvelope::Eip2930(signed_2930) => Self::Eip2930 {
                    signature: *signed_2930.signature(),
                    transaction: signed_2930.tx().into(),
                },
                super::CeloTxEnvelope::Eip1559(signed_1559) => Self::Eip1559 {
                    signature: *signed_1559.signature(),
                    transaction: signed_1559.tx().into(),
                },
                super::CeloTxEnvelope::Eip7702(signed_7702) => Self::Eip7702 {
                    signature: *signed_7702.signature(),
                    transaction: signed_7702.tx().into(),
                },
                super::CeloTxEnvelope::Cip64(signed_cip64) => Self::Cip64 {
                    signature: *signed_cip64.signature(),
                    transaction: signed_cip64.tx().into(),
                },
                super::CeloTxEnvelope::Deposit(sealed_deposit) => Self::Deposit {
                    hash: sealed_deposit.seal(),
                    transaction: sealed_deposit.inner().into(),
                },
            }
        }
    }

    impl<'a> From<CeloTxEnvelope<'a>> for super::CeloTxEnvelope {
        fn from(value: CeloTxEnvelope<'a>) -> Self {
            match value {
                CeloTxEnvelope::Legacy {
                    signature,
                    transaction,
                } => Self::Legacy(Signed::new_unhashed(transaction.into(), signature)),
                CeloTxEnvelope::Eip2930 {
                    signature,
                    transaction,
                } => Self::Eip2930(Signed::new_unhashed(transaction.into(), signature)),
                CeloTxEnvelope::Eip1559 {
                    signature,
                    transaction,
                } => Self::Eip1559(Signed::new_unhashed(transaction.into(), signature)),
                CeloTxEnvelope::Eip7702 {
                    signature,
                    transaction,
                } => Self::Eip7702(Signed::new_unhashed(transaction.into(), signature)),
                CeloTxEnvelope::Cip64 {
                    signature,
                    transaction,
                } => Self::Cip64(Signed::new_unhashed(transaction.into(), signature)),
                CeloTxEnvelope::Deposit { hash, transaction } => {
                    Self::Deposit(Sealed::new_unchecked(transaction.into(), hash))
                }
            }
        }
    }

    impl SerializeAs<super::CeloTxEnvelope> for CeloTxEnvelope<'_> {
        fn serialize_as<S>(source: &super::CeloTxEnvelope, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            let borrowed = CeloTxEnvelope::from(source);
            borrowed.serialize(serializer)
        }
    }

    impl<'de> DeserializeAs<'de, super::CeloTxEnvelope> for CeloTxEnvelope<'de> {
        fn deserialize_as<D>(deserializer: D) -> Result<super::CeloTxEnvelope, D::Error>
        where
            D: Deserializer<'de>,
        {
            let borrowed = CeloTxEnvelope::deserialize(deserializer)?;
            Ok(borrowed.into())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use arbitrary::Arbitrary;
        use rand::Rng;
        use serde::{Deserialize, Serialize};
        use serde_with::serde_as;

        /// Tests a bincode round-trip for CeloTxEnvelope using an arbitrary instance.
        #[test]
        fn test_celo_tx_envelope_bincode_roundtrip_arbitrary() {
            #[serde_as]
            #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
            struct Data {
                // Use the bincode-compatible representation defined in this module.
                #[serde_as(as = "CeloTxEnvelope<'_>")]
                envelope: super::super::CeloTxEnvelope,
            }

            let mut bytes = [0u8; 1024];
            rand::rng().fill(bytes.as_mut_slice());
            let data = Data {
                envelope: super::super::CeloTxEnvelope::arbitrary(
                    &mut arbitrary::Unstructured::new(&bytes),
                )
                .unwrap(),
            };

            let encoded = bincode::serde::encode_to_vec(&data, bincode::config::legacy()).unwrap();
            let (decoded, _) =
                bincode::serde::decode_from_slice::<Data, _>(&encoded, bincode::config::legacy())
                    .unwrap();
            assert_eq!(decoded, data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::SignableTransaction;
    use alloy_primitives::{Address, Signature, U256, address, hex};
    use std::vec;

    #[test]
    fn test_cip64() {
        let tx = TxCip64::default();
        let sig = Signature::test_signature();
        let tx_envelope = CeloTxEnvelope::Cip64(tx.into_signed(sig));
        assert!(tx_envelope.is_cip64());
    }

    #[test]
    fn test_encode_decode_cip64() {
        let tx = TxCip64 {
            chain_id: 0xa4ec,
            nonce: 0x705,
            gas_limit: 0x3644c,
            to: address!("0x7a1e295c4babdf229776680c93ed0f73d069abc0").into(),
            value: U256::from(0_u64),
            input: hex!(
                "0xcac35c7a290decd9548b62a8d60345a988386fc84ba6bc95484008f6362f93160ef3e563"
            )
            .into(),
            max_fee_per_gas: 0x26442dbed,
            max_priority_fee_per_gas: 0x4d7ee,
            access_list: AccessList::default(),
            // fee_currency: Some(address!("0x2f25deb3848c207fc8e0c34035b3ba7fc157602b")),
            fee_currency: address!("0x2f25deb3848c207fc8e0c34035b3ba7fc157602b"),
        };
        let signature = Signature::test_signature();
        let tx_signed = tx.into_signed(signature);
        let tx_envelope: CeloTxEnvelope = tx_signed.into();
        let encoded = tx_envelope.encoded_2718();
        let decoded = CeloTxEnvelope::decode_2718(&mut encoded.as_ref()).unwrap();
        assert_eq!(encoded.len(), tx_envelope.encode_2718_len());
        assert_eq!(decoded, tx_envelope);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_serde_roundtrip_cip64() {
        let tx = TxCip64 {
            chain_id: 0xa4ec,
            nonce: 0x705,
            gas_limit: 0x3644c,
            to: address!("0x7a1e295c4babdf229776680c93ed0f73d069abc0").into(),
            value: U256::from(0_u64),
            input: hex!(
                "0xcac35c7a290decd9548b62a8d60345a988386fc84ba6bc95484008f6362f93160ef3e563"
            )
            .into(),
            max_fee_per_gas: 0x26442dbed,
            max_priority_fee_per_gas: 0x4d7ee,
            access_list: AccessList::default(),
            // fee_currency: Some(address!("0x2f25deb3848c207fc8e0c34035b3ba7fc157602b")),
            fee_currency: address!("0x2f25deb3848c207fc8e0c34035b3ba7fc157602b"),
        };
        let signature = Signature::test_signature();
        let tx_envelope: CeloTxEnvelope = tx.into_signed(signature).into();

        let serialized = serde_json::to_string(&tx_envelope).unwrap();
        let deserialized: CeloTxEnvelope = serde_json::from_str(&serialized).unwrap();

        assert_eq!(tx_envelope, deserialized);
    }

    #[test]
    fn eip2718_cip64_decode() {
        // <https://celoscan.io/tx/0x419a20802ba15b1716889499c7f49f504838fde7b00fa7285ee0682b1ed864d3>
        let b = hex!(
            "7bf8cb82a4ec83041d96849502f900850c393e6d008301e11f9448065fbbe25f71c9282ddf5e1cd6d6a887483d5e80b844a9059cbb00000000000000000000000089a976d66f6325cb8454f5eae6fea895ff125bf70000000000000000000000000000000000000000000000000000000000004e20c0940e2a3e05bc9a16f5292a6170456a710cb89c6f7280a0cae194da527abf3feb0b23294759c750d9ddcb242127e16e5fe1d2b703c9c8b3a05b86afcdc3846f4bb78db25a4980abe180377ae743c01bd01f593ea558baee60"
        );

        let tx = CeloTxEnvelope::decode_2718(&mut b[..].as_ref()).unwrap();
        let cip64 = tx.as_cip64().unwrap();
        assert_ne!(cip64.tx().fee_currency, Address::ZERO);
    }

    #[test]
    fn eip1559_decode() {
        let tx = TxEip1559 {
            chain_id: 1u64,
            nonce: 2,
            max_fee_per_gas: 3,
            max_priority_fee_per_gas: 4,
            gas_limit: 5,
            to: Address::left_padding_from(&[6]).into(),
            value: U256::from(7_u64),
            input: vec![8].into(),
            access_list: Default::default(),
        };
        let sig = Signature::test_signature();
        let tx_signed = tx.into_signed(sig);
        let envelope: CeloTxEnvelope = tx_signed.into();
        let encoded = envelope.encoded_2718();
        let mut slice = encoded.as_slice();
        let decoded = CeloTxEnvelope::decode_2718(&mut slice).unwrap();
        assert!(matches!(decoded, CeloTxEnvelope::Eip1559(_)));
    }
}
