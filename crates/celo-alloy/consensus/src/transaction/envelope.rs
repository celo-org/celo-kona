//! The Ethereum [EIP-2718] Transaction Envelope, modified for Celo.

use crate::TxCip64;
use alloy_consensus::{
    EthereumTxEnvelope, Sealable, Sealed, SignableTransaction, Signed, TransactionEnvelope,
    TxEip1559, TxEip2930, TxEip7702, TxEnvelope, TxLegacy,
};
#[cfg(feature = "k256")]
use alloy_primitives::Address;
use alloy_primitives::{B256, Bytes, Signature};
use op_alloy_consensus::TxDeposit;

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
#[derive(Debug, Clone, TransactionEnvelope)]
#[envelope(tx_type_name = CeloTxType, typed = CeloTypedTransaction, serde_cfg(feature = "serde"))]
pub enum CeloTxEnvelope {
    /// An untagged [`TxLegacy`].
    #[envelope(ty = 0)]
    Legacy(Signed<TxLegacy>),
    /// A [`TxEip2930`] tagged with type 1.
    #[envelope(ty = 1)]
    Eip2930(Signed<TxEip2930>),
    /// A [`TxEip1559`] tagged with type 2.
    #[envelope(ty = 2)]
    Eip1559(Signed<TxEip1559>),
    /// A [`TxEip7702`] tagged with type 4.
    #[envelope(ty = 4)]
    Eip7702(Signed<TxEip7702>),
    /// A [`TxCip64`] tagged with type 0x7B.
    #[envelope(ty = 123)]
    Cip64(Signed<TxCip64>),
    /// A [`TxDeposit`] tagged with type 0x7E.
    #[envelope(ty = 126)]
    #[serde(serialize_with = "op_alloy_consensus::serde_deposit_tx_rpc")]
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

impl From<Sealed<TxDeposit>> for CeloTxEnvelope {
    fn from(v: Sealed<TxDeposit>) -> Self {
        Self::Deposit(v)
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

impl<T> TryFrom<EthereumTxEnvelope<T>> for CeloTxEnvelope {
    type Error = EthereumTxEnvelope<T>;

    fn try_from(value: EthereumTxEnvelope<T>) -> Result<Self, Self::Error> {
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

// =============================================================================
// CeloTxEnvelope additional methods
// =============================================================================

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
    /// Returns the envelope as error if it is a variant unsupported on ethereum: [`TxDeposit`,
    /// `TxCip64`]
    #[allow(clippy::result_large_err)]
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
    #[allow(clippy::result_large_err)]
    pub fn try_from_eth_envelope<T>(
        tx: EthereumTxEnvelope<T>,
    ) -> Result<Self, EthereumTxEnvelope<T>> {
        match tx {
            EthereumTxEnvelope::Legacy(tx) => Ok(tx.into()),
            EthereumTxEnvelope::Eip2930(tx) => Ok(tx.into()),
            EthereumTxEnvelope::Eip1559(tx) => Ok(tx.into()),
            tx @ EthereumTxEnvelope::<T>::Eip4844(_) => Err(tx),
            EthereumTxEnvelope::Eip7702(tx) => Ok(tx.into()),
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
    ) -> Result<alloy_consensus::transaction::Recovered<Self>, alloy_consensus::crypto::RecoveryError>
    {
        let signer = self.recover_signer()?;
        Ok(alloy_consensus::transaction::Recovered::new_unchecked(self, signer))
    }

    /// Recover the signer of the transaction and returns a `Recovered<&Self>`
    #[cfg(feature = "k256")]
    pub fn try_to_recovered_ref(
        &self,
    ) -> Result<alloy_consensus::transaction::Recovered<&Self>, alloy_primitives::SignatureError>
    {
        let signer = self.recover_signer()?;
        Ok(alloy_consensus::transaction::Recovered::new_unchecked(self, signer))
    }

    /// Returns mutable access to the input bytes.
    ///
    /// Caution: modifying this will cause side-effects on the hash.
    #[doc(hidden)]
    pub const fn input_mut(&mut self) -> &mut Bytes {
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
            Self::Legacy(_) => CeloTxType::Legacy,
            Self::Eip2930(_) => CeloTxType::Eip2930,
            Self::Eip1559(_) => CeloTxType::Eip1559,
            Self::Eip7702(_) => CeloTxType::Eip7702,
            Self::Cip64(_) => CeloTxType::Cip64,
            Self::Deposit(_) => CeloTxType::Deposit,
        }
    }

    /// Attempts to consume the type into a [`Signed`].
    ///
    /// Returns the envelope as error if it is a variant not converted to signed txn: [`TxDeposit`]
    #[allow(clippy::result_large_err)]
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
                CeloTxEnvelope::Legacy { signature, transaction } => {
                    Self::Legacy(Signed::new_unhashed(transaction.into(), signature))
                }
                CeloTxEnvelope::Eip2930 { signature, transaction } => {
                    Self::Eip2930(Signed::new_unhashed(transaction.into(), signature))
                }
                CeloTxEnvelope::Eip1559 { signature, transaction } => {
                    Self::Eip1559(Signed::new_unhashed(transaction.into(), signature))
                }
                CeloTxEnvelope::Eip7702 { signature, transaction } => {
                    Self::Eip7702(Signed::new_unhashed(transaction.into(), signature))
                }
                CeloTxEnvelope::Cip64 { signature, transaction } => {
                    Self::Cip64(Signed::new_unhashed(transaction.into(), signature))
                }
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
    use alloy_eips::{Decodable2718, Encodable2718, eip2930::AccessList};
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
            fee_currency: Some(address!("0x2f25deb3848c207fc8e0c34035b3ba7fc157602b")),
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
            fee_currency: Some(address!("0x2f25deb3848c207fc8e0c34035b3ba7fc157602b")),
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
        assert!(
            cip64.tx().fee_currency.is_some() && cip64.tx().fee_currency != Some(Address::ZERO)
        );
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

    #[test]
    fn test_celo_tx_type_all() {
        assert_eq!(CeloTxType::ALL.len(), 6);
        let all = vec![
            CeloTxType::Legacy,
            CeloTxType::Eip2930,
            CeloTxType::Eip1559,
            CeloTxType::Eip7702,
            CeloTxType::Cip64,
            CeloTxType::Deposit,
        ];
        assert_eq!(CeloTxType::ALL.to_vec(), all);
    }

    #[test]
    fn test_celo_tx_type_is_deposit() {
        assert!(!CeloTxType::Legacy.is_deposit());
        assert!(!CeloTxType::Eip2930.is_deposit());
        assert!(!CeloTxType::Eip1559.is_deposit());
        assert!(!CeloTxType::Eip7702.is_deposit());
        assert!(!CeloTxType::Cip64.is_deposit());
        assert!(CeloTxType::Deposit.is_deposit());
    }
}
