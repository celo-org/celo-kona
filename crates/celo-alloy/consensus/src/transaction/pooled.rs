//! Celo pooled transaction type.
//!
//! Defines the exact transaction variants that are allowed to be propagated
//! over the p2p protocol. This mirrors [`op_alloy_consensus::OpPooledTransaction`]
//! but adds the CIP-64 variant. Deposits are NOT pooled.

use crate::transaction::{CeloTxEnvelope, cip64::TxCip64};
use alloy_consensus::{
    Signed, TransactionEnvelope,
    error::ValueError,
    transaction::{TxEip1559, TxEip2930, TxHashRef, TxLegacy},
};
use alloy_primitives::{B256, Signature, TxHash, bytes};

/// All possible transactions that can be included in a response to `GetPooledTransactions`
/// on Celo. This is the same as [`CeloTxEnvelope`] minus `Deposit` (deposits come from the
/// engine, not the pool).
#[derive(Clone, Debug, TransactionEnvelope)]
#[envelope(tx_type_name = CeloPooledTxType, serde_cfg(feature = "serde"))]
pub enum CeloPooledTransaction {
    /// An untagged [`TxLegacy`].
    #[envelope(ty = 0)]
    Legacy(Signed<TxLegacy>),
    /// A [`TxEip2930`] transaction tagged with type 1.
    #[envelope(ty = 1)]
    Eip2930(Signed<TxEip2930>),
    /// A [`TxEip1559`] transaction tagged with type 2.
    #[envelope(ty = 2)]
    Eip1559(Signed<TxEip1559>),
    /// A [`TxEip7702`] transaction tagged with type 4.
    #[envelope(ty = 4)]
    Eip7702(Signed<alloy_consensus::TxEip7702>),
    /// A [`TxCip64`] tagged with type 0x7B.
    #[envelope(ty = 123)]
    Cip64(Signed<TxCip64>),
}

impl CeloPooledTransaction {
    /// Heavy operation that returns the signature hash over rlp encoded transaction.
    pub fn signature_hash(&self) -> B256 {
        match self {
            Self::Legacy(tx) => tx.signature_hash(),
            Self::Eip2930(tx) => tx.signature_hash(),
            Self::Eip1559(tx) => tx.signature_hash(),
            Self::Eip7702(tx) => tx.signature_hash(),
            Self::Cip64(tx) => tx.signature_hash(),
        }
    }

    /// Reference to transaction hash.
    pub fn hash(&self) -> &TxHash {
        match self {
            Self::Legacy(tx) => tx.hash(),
            Self::Eip2930(tx) => tx.hash(),
            Self::Eip1559(tx) => tx.hash(),
            Self::Eip7702(tx) => tx.hash(),
            Self::Cip64(tx) => tx.hash(),
        }
    }

    /// Returns the signature of the transaction.
    pub const fn signature(&self) -> &Signature {
        match self {
            Self::Legacy(tx) => tx.signature(),
            Self::Eip2930(tx) => tx.signature(),
            Self::Eip1559(tx) => tx.signature(),
            Self::Eip7702(tx) => tx.signature(),
            Self::Cip64(tx) => tx.signature(),
        }
    }

    /// This encodes the transaction _without_ the signature, and is only suitable for creating a
    /// hash intended for signing.
    pub fn encode_for_signing(&self, out: &mut dyn bytes::BufMut) {
        use alloy_consensus::SignableTransaction;
        match self {
            Self::Legacy(tx) => tx.tx().encode_for_signing(out),
            Self::Eip2930(tx) => tx.tx().encode_for_signing(out),
            Self::Eip1559(tx) => tx.tx().encode_for_signing(out),
            Self::Eip7702(tx) => tx.tx().encode_for_signing(out),
            Self::Cip64(tx) => tx.tx().encode_for_signing(out),
        }
    }

    /// Converts the transaction into a [`CeloTxEnvelope`].
    pub fn into_envelope(self) -> CeloTxEnvelope {
        match self {
            Self::Legacy(tx) => CeloTxEnvelope::Legacy(tx),
            Self::Eip2930(tx) => CeloTxEnvelope::Eip2930(tx),
            Self::Eip1559(tx) => CeloTxEnvelope::Eip1559(tx),
            Self::Eip7702(tx) => CeloTxEnvelope::Eip7702(tx),
            Self::Cip64(tx) => CeloTxEnvelope::Cip64(tx),
        }
    }
}

impl From<Signed<TxLegacy>> for CeloPooledTransaction {
    fn from(v: Signed<TxLegacy>) -> Self {
        Self::Legacy(v)
    }
}

impl From<Signed<TxEip2930>> for CeloPooledTransaction {
    fn from(v: Signed<TxEip2930>) -> Self {
        Self::Eip2930(v)
    }
}

impl From<Signed<TxEip1559>> for CeloPooledTransaction {
    fn from(v: Signed<TxEip1559>) -> Self {
        Self::Eip1559(v)
    }
}

impl From<Signed<alloy_consensus::TxEip7702>> for CeloPooledTransaction {
    fn from(v: Signed<alloy_consensus::TxEip7702>) -> Self {
        Self::Eip7702(v)
    }
}

impl From<Signed<TxCip64>> for CeloPooledTransaction {
    fn from(v: Signed<TxCip64>) -> Self {
        Self::Cip64(v)
    }
}

impl TxHashRef for CeloPooledTransaction {
    fn tx_hash(&self) -> &B256 {
        Self::hash(self)
    }
}

#[cfg(feature = "k256")]
impl alloy_consensus::transaction::SignerRecoverable for CeloPooledTransaction {
    fn recover_signer(
        &self,
    ) -> Result<alloy_primitives::Address, alloy_consensus::crypto::RecoveryError> {
        let signature_hash = self.signature_hash();
        alloy_consensus::crypto::secp256k1::recover_signer(self.signature(), signature_hash)
    }

    fn recover_signer_unchecked(
        &self,
    ) -> Result<alloy_primitives::Address, alloy_consensus::crypto::RecoveryError> {
        let signature_hash = self.signature_hash();
        alloy_consensus::crypto::secp256k1::recover_signer_unchecked(
            self.signature(),
            signature_hash,
        )
    }
}

impl From<CeloPooledTransaction> for CeloTxEnvelope {
    fn from(tx: CeloPooledTransaction) -> Self {
        tx.into_envelope()
    }
}

impl TryFrom<CeloTxEnvelope> for CeloPooledTransaction {
    type Error = ValueError<CeloTxEnvelope>;

    fn try_from(tx: CeloTxEnvelope) -> Result<Self, Self::Error> {
        match tx {
            CeloTxEnvelope::Legacy(tx) => Ok(Self::Legacy(tx)),
            CeloTxEnvelope::Eip2930(tx) => Ok(Self::Eip2930(tx)),
            CeloTxEnvelope::Eip1559(tx) => Ok(Self::Eip1559(tx)),
            CeloTxEnvelope::Eip7702(tx) => Ok(Self::Eip7702(tx)),
            CeloTxEnvelope::Cip64(tx) => Ok(Self::Cip64(tx)),
            other => Err(ValueError::new_static(
                other,
                "Deposit transactions cannot be pooled",
            )),
        }
    }
}
