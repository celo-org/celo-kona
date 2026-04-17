//! The Celo consensus transaction type used as `NodePrimitives::SignedTx`.
//!
//! [`CeloConsensusTx`] is a thin wrapper around [`CeloTxEnvelope`] that carries
//! **ephemeral, non-consensus** cached fee values computed during pool validation.
//! Its on-chain representation (hash, encoding, equality) is identical to
//! `CeloTxEnvelope`; the cached fields exist solely to let the payload builder
//! compute a correct native-denominated tip for CIP-64 transactions after
//! [`PoolTransaction::into_consensus`](reth_transaction_pool::PoolTransaction::into_consensus).
//!
//! # Why the wrapper exists
//!
//! For CIP-64 transactions, `max_fee_per_gas` and `max_priority_fee_per_gas` on
//! the on-chain tx are denominated in an ERC20 fee currency, not native CELO wei.
//! The pool validator converts them to native equivalents using the on-chain
//! exchange rate and caches the result on
//! [`CeloPoolTx`](crate::pool::CeloPoolTx). But op-reth's payload builder calls
//! `tx.into_consensus()` before computing the miner tip, which drops the pool
//! wrapper — the raw consensus tx then returns FC-denominated fees, and
//! `effective_tip_per_gas(base_fee_in_wei)` returns `None` due to the unit
//! mismatch.
//!
//! Carrying the already-computed native fees on the consensus wrapper lets
//! [`Transaction::effective_tip_per_gas`] return the correct value without
//! requiring any change to upstream op-reth.
//!
//! # Invariants
//!
//! The cached fields are **invisible** to:
//! - Transaction hash (the on-chain hash must match `CeloTxEnvelope`'s).
//! - RLP / EIP-2718 encoding and decoding (wire format is `CeloTxEnvelope`).
//! - Compact / database encoding.
//! - `PartialEq`, `Eq`, `Hash` (two wrappers of the same envelope are equal regardless of cache
//!   state).
//! - `serde::Serialize` / `Deserialize` (JSON shape is `CeloTxEnvelope`).
//!
//! They are only read by [`Transaction::effective_tip_per_gas`]. When they are
//! `None` (e.g. a wrapper decoded from the wire, loaded from storage, or
//! constructed outside the pool path), the impl falls back to the inner
//! envelope's default behavior.

use alloc::vec::Vec;
use alloy_consensus::{
    Sealed, Signed, Transaction, TransactionEnvelope, TxEip1559, TxEip2930, TxEip7702, TxLegacy,
    crypto::RecoveryError,
    transaction::{SignerRecoverable, TxHashRef},
};
use alloy_eips::{
    Decodable2718, Encodable2718, Typed2718,
    eip2718::{Eip2718Result, IsTyped2718},
    eip2930::AccessList,
    eip7702::SignedAuthorization,
};
use alloy_evm::{FromRecoveredTx, FromTxWithEncoded};
use alloy_primitives::{Address, B256, Bytes, ChainId, Signature, TxKind, U256};
use alloy_rlp::{BufMut, Decodable, Encodable};
use celo_alloy_consensus::{CeloPooledTransaction, CeloTxEnvelope, CeloTxType, TxCip64};
use celo_revm::CeloTransaction;
use core::hash::{Hash, Hasher};
use op_alloy_consensus::{OpTransaction, TxDeposit};
use op_revm::OpTransactionError;
use reth_codecs::{
    Compact,
    alloy::transaction::{CompactEnvelope, Envelope, FromTxCompact, ToTxCompact},
};
use reth_primitives_traits::{InMemorySize, SignedTransaction, serde_bincode_compat::RlpBincode};
use revm::context::TxEnv;

/// Celo consensus transaction type used as `NodePrimitives::SignedTx`.
///
/// See the [module-level documentation](self) for why this wrapper exists and
/// what invariants the cached fields must uphold.
#[derive(Debug, Clone)]
pub struct CeloConsensusTx {
    /// The canonical on-chain transaction.
    inner: CeloTxEnvelope,
    /// Native-equivalent `max_fee_per_gas`, populated by the pool validator for CIP-64 txs.
    ///
    /// `None` for non-CIP-64 transactions and for CIP-64 transactions constructed outside
    /// the pool (wire/storage/test construction). Must not influence encoding or hashing.
    cached_native_max_fee: Option<u128>,
    /// Native-equivalent `max_priority_fee_per_gas`, populated by the pool validator.
    ///
    /// Same semantics as [`cached_native_max_fee`](Self::cached_native_max_fee).
    cached_native_max_priority_fee: Option<u128>,
}

impl CeloConsensusTx {
    /// Wraps a [`CeloTxEnvelope`] without cached native fees.
    ///
    /// Use this when constructing the wrapper outside the pool path (tests, wire decoding,
    /// storage loads). For pool-derived wrappers, see
    /// [`CeloConsensusTx::with_native_fees`].
    pub const fn new(inner: CeloTxEnvelope) -> Self {
        Self { inner, cached_native_max_fee: None, cached_native_max_priority_fee: None }
    }

    /// Wraps a [`CeloTxEnvelope`] and attaches native-equivalent fee values.
    ///
    /// Used by `CeloPoolTx::into_consensus` so that a subsequent
    /// [`Transaction::effective_tip_per_gas`] call during payload building returns
    /// the correct native-denominated tip for CIP-64 transactions.
    pub const fn with_native_fees(
        inner: CeloTxEnvelope,
        native_max_fee: u128,
        native_max_priority_fee: u128,
    ) -> Self {
        Self {
            inner,
            cached_native_max_fee: Some(native_max_fee),
            cached_native_max_priority_fee: Some(native_max_priority_fee),
        }
    }

    /// Returns a reference to the inner [`CeloTxEnvelope`].
    pub const fn envelope(&self) -> &CeloTxEnvelope {
        &self.inner
    }

    /// Unwraps into the inner [`CeloTxEnvelope`], discarding cached fee values.
    pub fn into_envelope(self) -> CeloTxEnvelope {
        self.inner
    }

    /// Returns `true` if this wrapper has native-equivalent fee values cached.
    pub const fn has_cached_native_fees(&self) -> bool {
        self.cached_native_max_fee.is_some()
    }
}

impl From<CeloTxEnvelope> for CeloConsensusTx {
    fn from(inner: CeloTxEnvelope) -> Self {
        Self::new(inner)
    }
}

impl From<CeloConsensusTx> for CeloTxEnvelope {
    fn from(tx: CeloConsensusTx) -> Self {
        tx.inner
    }
}

impl AsRef<CeloTxEnvelope> for CeloConsensusTx {
    fn as_ref(&self) -> &CeloTxEnvelope {
        &self.inner
    }
}

// ---------------------------------------------------------------------------
// Equality / hashing: only the inner envelope matters.
// ---------------------------------------------------------------------------

impl PartialEq for CeloConsensusTx {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl Eq for CeloConsensusTx {}

impl Hash for CeloConsensusTx {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // The inherent `CeloTxEnvelope::hash(&self) -> B256` method shadows
        // `core::hash::Hash::hash`, so we must call the trait method explicitly.
        Hash::hash(&self.inner, state);
    }
}

// ---------------------------------------------------------------------------
// serde: transparent over the inner envelope so JSON shape is unchanged.
// ---------------------------------------------------------------------------

impl serde::Serialize for CeloConsensusTx {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.inner.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for CeloConsensusTx {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        CeloTxEnvelope::deserialize(deserializer).map(Self::new)
    }
}

// ---------------------------------------------------------------------------
// alloy_consensus::Transaction — delegates to inner, except `effective_tip_per_gas`.
// ---------------------------------------------------------------------------

impl Transaction for CeloConsensusTx {
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

    /// Returns the effective tip per gas, in **native** units.
    ///
    /// For CIP-64 transactions carried through the pool, the inner envelope's
    /// `max_fee_per_gas` / `max_priority_fee_per_gas` are fee-currency-denominated
    /// and cannot be compared directly against `base_fee` (native wei). If the
    /// pool validator attached native-equivalent cached fees via
    /// [`CeloConsensusTx::with_native_fees`], they are used here. Otherwise this
    /// falls through to the inner envelope's behavior (which returns `None` for
    /// CIP-64 whenever the FC-denominated fee is numerically less than the native
    /// base fee — a known limitation for wrappers not constructed via the pool).
    fn effective_tip_per_gas(&self, base_fee: u64) -> Option<u128> {
        if matches!(self.inner, CeloTxEnvelope::Cip64(_)) &&
            let (Some(native_max_fee), Some(native_max_prio)) =
                (self.cached_native_max_fee, self.cached_native_max_priority_fee)
        {
            let base_fee = base_fee as u128;
            if native_max_fee < base_fee {
                return None;
            }
            let fee = native_max_fee - base_fee;
            return Some(fee.min(native_max_prio));
        }
        self.inner.effective_tip_per_gas(base_fee)
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

// ---------------------------------------------------------------------------
// Typed2718 / IsTyped2718
// ---------------------------------------------------------------------------

impl Typed2718 for CeloConsensusTx {
    fn ty(&self) -> u8 {
        self.inner.ty()
    }
}

impl IsTyped2718 for CeloConsensusTx {
    fn is_type(type_id: u8) -> bool {
        <CeloTxEnvelope as IsTyped2718>::is_type(type_id)
    }
}

// ---------------------------------------------------------------------------
// RLP encoding: transparent over the inner envelope.
// ---------------------------------------------------------------------------

impl Encodable for CeloConsensusTx {
    fn encode(&self, out: &mut dyn BufMut) {
        self.inner.encode(out);
    }

    fn length(&self) -> usize {
        self.inner.length()
    }
}

impl Decodable for CeloConsensusTx {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        CeloTxEnvelope::decode(buf).map(Self::new)
    }
}

// ---------------------------------------------------------------------------
// EIP-2718 typed envelope encoding
// ---------------------------------------------------------------------------

impl Encodable2718 for CeloConsensusTx {
    fn type_flag(&self) -> Option<u8> {
        self.inner.type_flag()
    }

    fn encode_2718_len(&self) -> usize {
        self.inner.encode_2718_len()
    }

    fn encode_2718(&self, out: &mut dyn BufMut) {
        self.inner.encode_2718(out);
    }

    fn trie_hash(&self) -> B256 {
        self.inner.trie_hash()
    }
}

impl Decodable2718 for CeloConsensusTx {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        CeloTxEnvelope::typed_decode(ty, buf).map(Self::new)
    }

    fn fallback_decode(buf: &mut &[u8]) -> Eip2718Result<Self> {
        CeloTxEnvelope::fallback_decode(buf).map(Self::new)
    }
}

// ---------------------------------------------------------------------------
// Reth signed-transaction traits
// ---------------------------------------------------------------------------

impl InMemorySize for CeloConsensusTx {
    fn size(&self) -> usize {
        self.inner.size() + core::mem::size_of::<Option<u128>>() * 2
    }
}

impl OpTransaction for CeloConsensusTx {
    fn is_deposit(&self) -> bool {
        self.inner.is_deposit()
    }

    fn as_deposit(&self) -> Option<&Sealed<TxDeposit>> {
        self.inner.as_deposit()
    }
}

impl TxHashRef for CeloConsensusTx {
    fn tx_hash(&self) -> &B256 {
        self.inner.tx_hash()
    }
}

impl SignerRecoverable for CeloConsensusTx {
    fn recover_signer(&self) -> Result<Address, RecoveryError> {
        // CeloTxEnvelope has an inherent `recover_signer` that returns a
        // different error type; dispatch through the trait explicitly.
        <CeloTxEnvelope as SignerRecoverable>::recover_signer(&self.inner)
    }

    fn recover_signer_unchecked(&self) -> Result<Address, RecoveryError> {
        <CeloTxEnvelope as SignerRecoverable>::recover_signer_unchecked(&self.inner)
    }
}

impl SignedTransaction for CeloConsensusTx {}

impl TransactionEnvelope for CeloConsensusTx {
    type TxType = CeloTxType;

    fn tx_type(&self) -> Self::TxType {
        <CeloTxEnvelope as TransactionEnvelope>::tx_type(&self.inner)
    }
}

// ---------------------------------------------------------------------------
// Conversions to/from CeloPooledTransaction (required by `PoolTransaction`).
// ---------------------------------------------------------------------------

impl From<CeloPooledTransaction> for CeloConsensusTx {
    fn from(tx: CeloPooledTransaction) -> Self {
        Self::new(CeloTxEnvelope::from(tx))
    }
}

impl TryFrom<CeloConsensusTx> for CeloPooledTransaction {
    type Error = <Self as TryFrom<CeloTxEnvelope>>::Error;

    fn try_from(tx: CeloConsensusTx) -> Result<Self, Self::Error> {
        Self::try_from(tx.inner)
    }
}

// ---------------------------------------------------------------------------
// EVM transaction construction (`alloy_evm::FromRecoveredTx` / `FromTxWithEncoded`).
//
// These are needed so that `CeloEvmFactory`'s `Tx = CeloTransaction<TxEnv>` can
// be constructed from a `CeloConsensusTx` — the bound is expressed on
// `EvmFactory::Tx` via `FromRecoveredTx<R::Transaction>` and
// `FromTxWithEncoded<R::Transaction>` in `CeloEvmConfig`'s `ConfigureEvm` impl.
//
// Orphan rules: `CeloTransaction` and the traits are foreign, but
// `CeloConsensusTx` is local, so the impls are admissible here.
// ---------------------------------------------------------------------------

impl FromRecoveredTx<CeloConsensusTx> for CeloTransaction<TxEnv> {
    fn from_recovered_tx(tx: &CeloConsensusTx, sender: Address) -> Self {
        Self::from_recovered_tx(&tx.inner, sender)
    }
}

impl FromTxWithEncoded<CeloConsensusTx> for CeloTransaction<TxEnv> {
    fn from_encoded_tx(tx: &CeloConsensusTx, caller: Address, encoded: Bytes) -> Self {
        Self::from_encoded_tx(&tx.inner, caller, encoded)
    }
}

// Silence unused-import warnings for the error alias pulled in above.
#[allow(dead_code)]
type _CeloTxErr = OpTransactionError;

// ---------------------------------------------------------------------------
// Reth Compact / bincode / database traits — all transparent over inner.
// ---------------------------------------------------------------------------

impl ToTxCompact for CeloConsensusTx {
    fn to_tx_compact(&self, buf: &mut (impl BufMut + AsMut<[u8]>)) {
        self.inner.to_tx_compact(buf);
    }
}

impl FromTxCompact for CeloConsensusTx {
    type TxType = CeloTxType;

    fn from_tx_compact(buf: &[u8], tx_type: CeloTxType, signature: Signature) -> (Self, &[u8]) {
        let (inner, rest) = CeloTxEnvelope::from_tx_compact(buf, tx_type, signature);
        (Self::new(inner), rest)
    }
}

impl Envelope for CeloConsensusTx {
    fn signature(&self) -> &Signature {
        self.inner.signature()
    }

    fn tx_type(&self) -> Self::TxType {
        <CeloTxEnvelope as Envelope>::tx_type(&self.inner)
    }
}

impl Compact for CeloConsensusTx {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        CompactEnvelope::to_compact(self, buf)
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        CompactEnvelope::from_compact(buf, len)
    }
}

impl RlpBincode for CeloConsensusTx {}

// ---------------------------------------------------------------------------
// Database compression: mirrors the CeloTxEnvelope impls in celo-alloy-consensus.
// ---------------------------------------------------------------------------

use reth_db_api::table::{Compress, Decompress};

impl Compress for CeloConsensusTx {
    type Compressed = Vec<u8>;

    fn compress_to_buf<B: bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        let _ = Compact::to_compact(self, buf);
    }
}

impl Decompress for CeloConsensusTx {
    fn decompress(value: &[u8]) -> Result<Self, reth_db_api::DatabaseError> {
        let (obj, _) = Compact::from_compact(value, value.len());
        Ok(obj)
    }
}

// ---------------------------------------------------------------------------
// CIP-64 transaction accessor used by the pool validator and RPC layer.
// ---------------------------------------------------------------------------

impl CeloConsensusTx {
    /// Returns the inner CIP-64 signed transaction if this is a CIP-64 envelope.
    pub const fn as_cip64(&self) -> Option<&Signed<TxCip64>> {
        match &self.inner {
            CeloTxEnvelope::Cip64(signed) => Some(signed),
            _ => None,
        }
    }

    /// Returns the inner legacy signed transaction if this is a legacy envelope.
    pub const fn as_legacy(&self) -> Option<&Signed<TxLegacy>> {
        match &self.inner {
            CeloTxEnvelope::Legacy(signed) => Some(signed),
            _ => None,
        }
    }

    /// Returns the inner EIP-1559 signed transaction if this is an EIP-1559 envelope.
    pub const fn as_eip1559(&self) -> Option<&Signed<TxEip1559>> {
        match &self.inner {
            CeloTxEnvelope::Eip1559(signed) => Some(signed),
            _ => None,
        }
    }

    /// Returns the inner EIP-2930 signed transaction if this is an EIP-2930 envelope.
    pub const fn as_eip2930(&self) -> Option<&Signed<TxEip2930>> {
        match &self.inner {
            CeloTxEnvelope::Eip2930(signed) => Some(signed),
            _ => None,
        }
    }

    /// Returns the inner EIP-7702 signed transaction if this is an EIP-7702 envelope.
    pub const fn as_eip7702(&self) -> Option<&Signed<TxEip7702>> {
        match &self.inner {
            CeloTxEnvelope::Eip7702(signed) => Some(signed),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn _assert_bounds() {
        fn needs_signed_tx<T: SignedTransaction>() {}
        needs_signed_tx::<CeloConsensusTx>();
    }
}
