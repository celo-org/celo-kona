//! Celo typed transaction additional implementations.

use crate::{
    TxCip64,
    transaction::envelope::{CeloTxEnvelope, CeloTxType, CeloTypedTransaction},
};
use alloy_consensus::{SignableTransaction, Signed, TxEip1559, TxEip2930, TxEip7702, TxLegacy};
use alloy_primitives::{B256, Signature};
use op_alloy_consensus::TxDeposit;

// =============================================================================
// CeloTypedTransaction additional methods
// =============================================================================

impl CeloTypedTransaction {
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

    /// Calculates the signing hash for the transaction.
    ///
    /// Returns `None` if the tx is a deposit transaction.
    pub fn checked_signature_hash(&self) -> Option<B256> {
        match self {
            Self::Legacy(tx) => Some(tx.signature_hash()),
            Self::Eip2930(tx) => Some(tx.signature_hash()),
            Self::Eip1559(tx) => Some(tx.signature_hash()),
            Self::Eip7702(tx) => Some(tx.signature_hash()),
            Self::Cip64(tx) => Some(tx.signature_hash()),
            Self::Deposit(_) => None,
        }
    }

    /// Return the inner legacy transaction if it exists.
    pub const fn legacy(&self) -> Option<&TxLegacy> {
        match self {
            Self::Legacy(tx) => Some(tx),
            _ => None,
        }
    }

    /// Return the inner EIP-2930 transaction if it exists.
    pub const fn eip2930(&self) -> Option<&TxEip2930> {
        match self {
            Self::Eip2930(tx) => Some(tx),
            _ => None,
        }
    }

    /// Return the inner EIP-1559 transaction if it exists.
    pub const fn eip1559(&self) -> Option<&TxEip1559> {
        match self {
            Self::Eip1559(tx) => Some(tx),
            _ => None,
        }
    }

    /// Return the inner CIP-64 transaction if it exists.
    pub const fn cip64(&self) -> Option<&TxCip64> {
        match self {
            Self::Cip64(tx) => Some(tx),
            _ => None,
        }
    }

    /// Return the inner deposit transaction if it exists.
    pub const fn deposit(&self) -> Option<&TxDeposit> {
        match self {
            Self::Deposit(tx) => Some(tx),
            _ => None,
        }
    }

    /// Returns `true` if transaction is deposit transaction.
    pub const fn is_deposit(&self) -> bool {
        matches!(self, Self::Deposit(_))
    }

    /// Calculate the transaction hash for the given signature.
    ///
    /// Note: Returns the regular tx hash if this is a deposit variant
    pub fn tx_hash(&self, signature: &Signature) -> B256 {
        use alloy_consensus::transaction::RlpEcdsaEncodableTx;
        match self {
            Self::Legacy(tx) => tx.tx_hash(signature),
            Self::Eip2930(tx) => tx.tx_hash(signature),
            Self::Eip1559(tx) => tx.tx_hash(signature),
            Self::Eip7702(tx) => tx.tx_hash(signature),
            Self::Cip64(tx) => tx.tx_hash(signature),
            Self::Deposit(tx) => tx.tx_hash(),
        }
    }
}

// =============================================================================
// From implementations
// =============================================================================

impl From<CeloTxEnvelope> for CeloTypedTransaction {
    fn from(envelope: CeloTxEnvelope) -> Self {
        match envelope {
            CeloTxEnvelope::Legacy(tx) => Self::Legacy(tx.strip_signature()),
            CeloTxEnvelope::Eip2930(tx) => Self::Eip2930(tx.strip_signature()),
            CeloTxEnvelope::Eip1559(tx) => Self::Eip1559(tx.strip_signature()),
            CeloTxEnvelope::Eip7702(tx) => Self::Eip7702(tx.strip_signature()),
            CeloTxEnvelope::Cip64(tx) => Self::Cip64(tx.strip_signature()),
            CeloTxEnvelope::Deposit(tx) => Self::Deposit(tx.into_inner()),
        }
    }
}

impl From<TxLegacy> for CeloTypedTransaction {
    fn from(tx: TxLegacy) -> Self {
        Self::Legacy(tx)
    }
}

impl From<TxEip2930> for CeloTypedTransaction {
    fn from(tx: TxEip2930) -> Self {
        Self::Eip2930(tx)
    }
}

impl From<TxEip1559> for CeloTypedTransaction {
    fn from(tx: TxEip1559) -> Self {
        Self::Eip1559(tx)
    }
}

impl From<TxEip7702> for CeloTypedTransaction {
    fn from(tx: TxEip7702) -> Self {
        Self::Eip7702(tx)
    }
}

impl From<TxCip64> for CeloTypedTransaction {
    fn from(tx: TxCip64) -> Self {
        Self::Cip64(tx)
    }
}

impl From<TxDeposit> for CeloTypedTransaction {
    fn from(tx: TxDeposit) -> Self {
        Self::Deposit(tx)
    }
}

// =============================================================================
// RlpEcdsaEncodableTx implementation
// =============================================================================

impl alloy_consensus::transaction::RlpEcdsaEncodableTx for CeloTypedTransaction {
    fn rlp_encoded_fields_length(&self) -> usize {
        use alloy_rlp::Encodable as _;
        match self {
            Self::Legacy(tx) => tx.rlp_encoded_fields_length(),
            Self::Eip2930(tx) => tx.rlp_encoded_fields_length(),
            Self::Eip1559(tx) => tx.rlp_encoded_fields_length(),
            Self::Eip7702(tx) => tx.rlp_encoded_fields_length(),
            Self::Cip64(tx) => tx.rlp_encoded_fields_length(),
            // TxDeposit doesn't expose rlp_encoded_fields_length, compute manually
            Self::Deposit(tx) => {
                tx.source_hash.length()
                    + tx.from.length()
                    + tx.to.length()
                    + tx.mint.length()
                    + tx.value.length()
                    + tx.gas_limit.length()
                    + tx.is_system_transaction.length()
                    + tx.input.0.length()
            }
        }
    }

    fn rlp_encode_fields(&self, out: &mut dyn alloy_rlp::BufMut) {
        use alloy_rlp::Encodable as _;
        match self {
            Self::Legacy(tx) => tx.rlp_encode_fields(out),
            Self::Eip2930(tx) => tx.rlp_encode_fields(out),
            Self::Eip1559(tx) => tx.rlp_encode_fields(out),
            Self::Eip7702(tx) => tx.rlp_encode_fields(out),
            Self::Cip64(tx) => tx.rlp_encode_fields(out),
            // TxDeposit doesn't expose rlp_encode_fields, encode manually
            Self::Deposit(tx) => {
                tx.source_hash.encode(out);
                tx.from.encode(out);
                tx.to.encode(out);
                tx.mint.encode(out);
                tx.value.encode(out);
                tx.gas_limit.encode(out);
                tx.is_system_transaction.encode(out);
                tx.input.encode(out);
            }
        }
    }

    fn eip2718_encode_with_type(
        &self,
        signature: &Signature,
        _ty: u8,
        out: &mut dyn alloy_rlp::BufMut,
    ) {
        use alloy_consensus::Typed2718 as _;
        use alloy_eips::Encodable2718 as _;
        match self {
            Self::Legacy(tx) => tx.eip2718_encode_with_type(signature, tx.ty(), out),
            Self::Eip2930(tx) => tx.eip2718_encode_with_type(signature, tx.ty(), out),
            Self::Eip1559(tx) => tx.eip2718_encode_with_type(signature, tx.ty(), out),
            Self::Eip7702(tx) => tx.eip2718_encode_with_type(signature, tx.ty(), out),
            Self::Cip64(tx) => tx.eip2718_encode_with_type(signature, tx.ty(), out),
            Self::Deposit(tx) => tx.encode_2718(out),
        }
    }

    fn eip2718_encode(&self, signature: &Signature, out: &mut dyn alloy_rlp::BufMut) {
        use alloy_eips::Encodable2718 as _;
        match self {
            Self::Legacy(tx) => tx.eip2718_encode(signature, out),
            Self::Eip2930(tx) => tx.eip2718_encode(signature, out),
            Self::Eip1559(tx) => tx.eip2718_encode(signature, out),
            Self::Eip7702(tx) => tx.eip2718_encode(signature, out),
            Self::Cip64(tx) => tx.eip2718_encode(signature, out),
            Self::Deposit(tx) => tx.encode_2718(out),
        }
    }

    fn network_encode_with_type(
        &self,
        signature: &Signature,
        _ty: u8,
        out: &mut dyn alloy_rlp::BufMut,
    ) {
        use alloy_consensus::Typed2718 as _;
        match self {
            Self::Legacy(tx) => tx.network_encode_with_type(signature, tx.ty(), out),
            Self::Eip2930(tx) => tx.network_encode_with_type(signature, tx.ty(), out),
            Self::Eip1559(tx) => tx.network_encode_with_type(signature, tx.ty(), out),
            Self::Eip7702(tx) => tx.network_encode_with_type(signature, tx.ty(), out),
            Self::Cip64(tx) => tx.network_encode_with_type(signature, tx.ty(), out),
            Self::Deposit(tx) => tx.network_encode(out),
        }
    }

    fn network_encode(&self, signature: &Signature, out: &mut dyn alloy_rlp::BufMut) {
        match self {
            Self::Legacy(tx) => tx.network_encode(signature, out),
            Self::Eip2930(tx) => tx.network_encode(signature, out),
            Self::Eip1559(tx) => tx.network_encode(signature, out),
            Self::Eip7702(tx) => tx.network_encode(signature, out),
            Self::Cip64(tx) => tx.network_encode(signature, out),
            Self::Deposit(tx) => tx.network_encode(out),
        }
    }

    fn tx_hash_with_type(&self, signature: &Signature, _ty: u8) -> B256 {
        use alloy_consensus::Typed2718 as _;
        match self {
            Self::Legacy(tx) => tx.tx_hash_with_type(signature, tx.ty()),
            Self::Eip2930(tx) => tx.tx_hash_with_type(signature, tx.ty()),
            Self::Eip1559(tx) => tx.tx_hash_with_type(signature, tx.ty()),
            Self::Eip7702(tx) => tx.tx_hash_with_type(signature, tx.ty()),
            Self::Cip64(tx) => tx.tx_hash_with_type(signature, tx.ty()),
            Self::Deposit(tx) => tx.tx_hash(),
        }
    }

    fn tx_hash(&self, signature: &Signature) -> B256 {
        Self::tx_hash(self, signature)
    }
}

// =============================================================================
// SignableTransaction implementation
// =============================================================================

impl SignableTransaction<Signature> for CeloTypedTransaction {
    fn set_chain_id(&mut self, chain_id: alloy_primitives::ChainId) {
        match self {
            Self::Legacy(tx) => tx.set_chain_id(chain_id),
            Self::Eip2930(tx) => tx.set_chain_id(chain_id),
            Self::Eip1559(tx) => tx.set_chain_id(chain_id),
            Self::Eip7702(tx) => tx.set_chain_id(chain_id),
            Self::Cip64(tx) => tx.set_chain_id(chain_id),
            Self::Deposit(_) => {}
        }
    }

    fn encode_for_signing(&self, out: &mut dyn alloy_primitives::bytes::BufMut) {
        match self {
            Self::Legacy(tx) => tx.encode_for_signing(out),
            Self::Eip2930(tx) => tx.encode_for_signing(out),
            Self::Eip1559(tx) => tx.encode_for_signing(out),
            Self::Eip7702(tx) => tx.encode_for_signing(out),
            Self::Cip64(tx) => tx.encode_for_signing(out),
            Self::Deposit(_) => {}
        }
    }

    fn payload_len_for_signature(&self) -> usize {
        match self {
            Self::Legacy(tx) => tx.payload_len_for_signature(),
            Self::Eip2930(tx) => tx.payload_len_for_signature(),
            Self::Eip1559(tx) => tx.payload_len_for_signature(),
            Self::Eip7702(tx) => tx.payload_len_for_signature(),
            Self::Cip64(tx) => tx.payload_len_for_signature(),
            Self::Deposit(_) => 0,
        }
    }

    fn into_signed(self, signature: Signature) -> Signed<Self, Signature>
    where
        Self: Sized,
    {
        let hash = self.tx_hash(&signature);
        Signed::new_unchecked(self, signature, hash)
    }
}
