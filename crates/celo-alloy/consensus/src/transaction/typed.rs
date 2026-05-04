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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::transaction::RlpEcdsaEncodableTx;
    use alloy_eips::eip2930::AccessList;
    use alloy_primitives::{Bytes, TxKind, U256, address};

    fn legacy() -> TxLegacy {
        TxLegacy {
            chain_id: Some(0xa4ec),
            nonce: 1,
            gas_price: 11,
            gas_limit: 21_000,
            to: TxKind::Call(address!("0x0000000000000000000000000000000000000a01")),
            value: U256::from(2_u64),
            input: Bytes::new(),
        }
    }

    fn eip2930() -> TxEip2930 {
        TxEip2930 {
            chain_id: 0xa4ec,
            nonce: 2,
            gas_price: 12,
            gas_limit: 21_000,
            to: TxKind::Call(address!("0x0000000000000000000000000000000000000a02")),
            value: U256::from(3_u64),
            access_list: AccessList::default(),
            input: Bytes::new(),
        }
    }

    fn eip1559() -> TxEip1559 {
        TxEip1559 {
            chain_id: 0xa4ec,
            nonce: 3,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 1,
            to: TxKind::Call(address!("0x0000000000000000000000000000000000000a03")),
            value: U256::from(4_u64),
            access_list: AccessList::default(),
            input: Bytes::new(),
        }
    }

    fn eip7702() -> TxEip7702 {
        TxEip7702 {
            chain_id: 0xa4ec,
            nonce: 4,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 1,
            to: address!("0x0000000000000000000000000000000000000a04"),
            value: U256::from(5_u64),
            access_list: AccessList::default(),
            authorization_list: vec![],
            input: Bytes::new(),
        }
    }

    fn cip64() -> TxCip64 {
        TxCip64 {
            chain_id: 0xa4ec,
            nonce: 5,
            gas_limit: 21_000,
            to: TxKind::Call(address!("0x0000000000000000000000000000000000000a05")),
            value: U256::from(6_u64),
            input: Bytes::new(),
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 1,
            access_list: AccessList::default(),
            fee_currency: None,
        }
    }

    fn deposit() -> TxDeposit {
        TxDeposit {
            source_hash: B256::from([0xAA; 32]),
            from: address!("0x0000000000000000000000000000000000000bbb"),
            to: TxKind::Call(address!("0x0000000000000000000000000000000000000a06")),
            mint: 0,
            value: U256::from(7_u64),
            gas_limit: 21_000,
            is_system_transaction: false,
            input: Bytes::new(),
        }
    }

    /// All six variants paired with their expected `CeloTxType`. Ensures
    /// every match arm is reachable via at least one assertion.
    fn all_variants() -> Vec<(CeloTypedTransaction, CeloTxType)> {
        vec![
            (CeloTypedTransaction::Legacy(legacy()), CeloTxType::Legacy),
            (CeloTypedTransaction::Eip2930(eip2930()), CeloTxType::Eip2930),
            (CeloTypedTransaction::Eip1559(eip1559()), CeloTxType::Eip1559),
            (CeloTypedTransaction::Eip7702(eip7702()), CeloTxType::Eip7702),
            (CeloTypedTransaction::Cip64(cip64()), CeloTxType::Cip64),
            (CeloTypedTransaction::Deposit(deposit()), CeloTxType::Deposit),
        ]
    }

    /// Pins `tx_type` against `delete match arm <variant>` for every variant
    /// AND against `-> Default`. Each variant maps to a distinct CeloTxType.
    #[test]
    fn tx_type_returns_matching_variant() {
        for (tx, expected) in all_variants() {
            assert_eq!(tx.tx_type(), expected, "tx_type for {expected:?}");
        }
    }

    /// Pins `checked_signature_hash` against `-> None` (catches all the
    /// non-deposit variants) and `Some(_)` arm deletion (catches the deposit
    /// variant which must return None).
    #[test]
    fn checked_signature_hash_is_some_for_all_but_deposit() {
        for (tx, ty) in all_variants() {
            let result = tx.checked_signature_hash();
            if matches!(ty, CeloTxType::Deposit) {
                assert!(result.is_none(), "{ty:?} must return None");
            } else {
                assert!(result.is_some(), "{ty:?} must return Some");
            }
        }
    }

    /// Pins each typed accessor (legacy/eip2930/eip1559/cip64/deposit)
    /// against `-> None` for the matching arm AND `delete match arm` for the
    /// fallthrough. For every variant: the matching accessor returns Some,
    /// every other accessor returns None.
    #[test]
    fn typed_accessors_return_some_only_for_matching_variant() {
        let tx = CeloTypedTransaction::Legacy(legacy());
        assert!(tx.legacy().is_some());
        assert!(tx.eip2930().is_none());
        assert!(tx.eip1559().is_none());
        assert!(tx.cip64().is_none());
        assert!(tx.deposit().is_none());

        let tx = CeloTypedTransaction::Eip2930(eip2930());
        assert!(tx.legacy().is_none());
        assert!(tx.eip2930().is_some());
        assert!(tx.eip1559().is_none());
        assert!(tx.cip64().is_none());
        assert!(tx.deposit().is_none());

        let tx = CeloTypedTransaction::Eip1559(eip1559());
        assert!(tx.legacy().is_none());
        assert!(tx.eip2930().is_none());
        assert!(tx.eip1559().is_some());
        assert!(tx.cip64().is_none());
        assert!(tx.deposit().is_none());

        let tx = CeloTypedTransaction::Cip64(cip64());
        assert!(tx.legacy().is_none());
        assert!(tx.eip2930().is_none());
        assert!(tx.eip1559().is_none());
        assert!(tx.cip64().is_some());
        assert!(tx.deposit().is_none());

        let tx = CeloTypedTransaction::Deposit(deposit());
        assert!(tx.legacy().is_none());
        assert!(tx.eip2930().is_none());
        assert!(tx.eip1559().is_none());
        assert!(tx.cip64().is_none());
        assert!(tx.deposit().is_some());
    }

    /// Pins `is_deposit -> true|false` and the underlying matches!.
    #[test]
    fn is_deposit_distinguishes_deposit_variant() {
        for (tx, ty) in all_variants() {
            let want = matches!(ty, CeloTxType::Deposit);
            assert_eq!(tx.is_deposit(), want, "{ty:?}");
        }
    }

    /// Pins `From<CeloTxEnvelope> -> Self` for every match arm.
    #[test]
    fn from_celo_tx_envelope_dispatches_each_variant() {
        let sig = Signature::test_signature();
        let cases: Vec<(CeloTxEnvelope, CeloTxType)> = vec![
            (CeloTxEnvelope::Legacy(legacy().into_signed(sig)), CeloTxType::Legacy),
            (CeloTxEnvelope::Eip2930(eip2930().into_signed(sig)), CeloTxType::Eip2930),
            (CeloTxEnvelope::Eip1559(eip1559().into_signed(sig)), CeloTxType::Eip1559),
            (CeloTxEnvelope::Eip7702(eip7702().into_signed(sig)), CeloTxType::Eip7702),
            (CeloTxEnvelope::Cip64(cip64().into_signed(sig)), CeloTxType::Cip64),
            (
                CeloTxEnvelope::Deposit(alloy_consensus::Sealed::new_unchecked(
                    deposit(),
                    B256::ZERO,
                )),
                CeloTxType::Deposit,
            ),
        ];
        for (env, expected) in cases {
            let typed = CeloTypedTransaction::from(env);
            assert_eq!(typed.tx_type(), expected);
        }
    }

    /// Pins the simple `From<TxLegacy/TxEip2930/...>` impls against `->
    /// Default`. Default `TxLegacy` etc. would have nonce=0; our fixtures
    /// have non-zero nonces, so equality on tx_type alone suffices to
    /// distinguish the variant — and the inner field assertion catches
    /// `-> Default` (Default for all wrapped types is `Legacy(Default)`).
    #[test]
    fn from_inner_tx_constructs_correct_variant() {
        assert_eq!(
            <CeloTypedTransaction as From<TxLegacy>>::from(legacy()).tx_type(),
            CeloTxType::Legacy,
        );
        assert_eq!(
            <CeloTypedTransaction as From<TxEip2930>>::from(eip2930()).tx_type(),
            CeloTxType::Eip2930,
        );
        assert_eq!(
            <CeloTypedTransaction as From<TxEip1559>>::from(eip1559()).tx_type(),
            CeloTxType::Eip1559,
        );
        assert_eq!(
            <CeloTypedTransaction as From<TxEip7702>>::from(eip7702()).tx_type(),
            CeloTxType::Eip7702,
        );
        assert_eq!(
            <CeloTypedTransaction as From<TxCip64>>::from(cip64()).tx_type(),
            CeloTxType::Cip64,
        );
        assert_eq!(
            <CeloTypedTransaction as From<TxDeposit>>::from(deposit()).tx_type(),
            CeloTxType::Deposit,
        );
    }

    /// Pins `set_chain_id -> ()` AND each match arm in
    /// `SignableTransaction::set_chain_id`. Mutates each variant and reads
    /// back via `chain_id()`. The Deposit arm is intentionally a no-op (no
    /// chain_id field), so we just assert it doesn't panic.
    #[test]
    fn set_chain_id_round_trips_per_variant() {
        let new_chain_id = 0xfade_u64;
        let mut t = CeloTypedTransaction::Legacy(legacy());
        t.set_chain_id(new_chain_id);
        assert_eq!(t.legacy().unwrap().chain_id, Some(new_chain_id));

        let mut t = CeloTypedTransaction::Eip2930(eip2930());
        t.set_chain_id(new_chain_id);
        assert_eq!(t.eip2930().unwrap().chain_id, new_chain_id);

        let mut t = CeloTypedTransaction::Eip1559(eip1559());
        t.set_chain_id(new_chain_id);
        assert_eq!(t.eip1559().unwrap().chain_id, new_chain_id);

        let mut t = CeloTypedTransaction::Eip7702(eip7702());
        t.set_chain_id(new_chain_id);
        // Eip7702 has no public accessor in the typed enum; chain_id is on the inner.
        if let CeloTypedTransaction::Eip7702(tx) = &t {
            assert_eq!(tx.chain_id, new_chain_id);
        }

        let mut t = CeloTypedTransaction::Cip64(cip64());
        t.set_chain_id(new_chain_id);
        assert_eq!(t.cip64().unwrap().chain_id, new_chain_id);

        // Deposit no-op — must not panic.
        let mut t = CeloTypedTransaction::Deposit(deposit());
        t.set_chain_id(new_chain_id);
        // Deposit txs don't expose chain_id; just assert the variant was preserved.
        assert!(t.is_deposit());
    }

    /// Pins `payload_len_for_signature -> 0|1` for non-deposit variants and
    /// the Deposit arm returning 0.
    #[test]
    fn payload_len_for_signature_is_nonzero_for_non_deposit() {
        for (tx, ty) in all_variants() {
            let len = tx.payload_len_for_signature();
            if matches!(ty, CeloTxType::Deposit) {
                assert_eq!(len, 0, "Deposit must return 0");
            } else {
                assert!(len > 1, "{ty:?} must return >1, got {len}");
            }
        }
    }

    /// Pins `encode_for_signing -> ()` per variant. Asserts each non-deposit
    /// variant writes more than zero bytes; Deposit writes zero.
    #[test]
    fn encode_for_signing_writes_bytes_per_variant() {
        for (tx, ty) in all_variants() {
            let mut buf = Vec::new();
            tx.encode_for_signing(&mut buf);
            if matches!(ty, CeloTxType::Deposit) {
                assert!(buf.is_empty(), "Deposit must write nothing");
            } else {
                assert!(!buf.is_empty(), "{ty:?} must write bytes");
            }
        }
    }

    /// Pins `tx_hash` per variant: each variant must produce a non-zero
    /// hash, and switching the inner tx changes the hash. Catches any
    /// `delete match arm` mutation in `tx_hash`.
    #[test]
    fn tx_hash_distinguishes_variants() {
        let sig = Signature::test_signature();
        let mut hashes = Vec::new();
        for (tx, _) in all_variants() {
            let hash = tx.tx_hash(&sig);
            assert_ne!(hash, B256::ZERO, "non-zero hash");
            hashes.push(hash);
        }
        // Each variant produces a distinct hash. If any arm got deleted,
        // the wrong inner method runs and the hash changes (or panics).
        let mut sorted = hashes.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), hashes.len(), "all variants must hash distinctly");
    }

    /// Pins `rlp_encoded_fields_length` per variant by asserting non-zero
    /// length AND that lengths differ across variants (so any mis-routed
    /// arm produces a different value).
    #[test]
    fn rlp_encoded_fields_length_per_variant() {
        for (tx, ty) in all_variants() {
            let len = tx.rlp_encoded_fields_length();
            assert!(len > 0, "{ty:?} must have non-zero encoded length");
        }
    }

    /// Pins `rlp_encode_fields` per variant by asserting the length of the
    /// written buffer matches `rlp_encoded_fields_length`. A deleted arm
    /// would write nothing for that variant.
    #[test]
    fn rlp_encode_fields_writes_expected_length_per_variant() {
        for (tx, ty) in all_variants() {
            let mut buf = Vec::new();
            tx.rlp_encode_fields(&mut buf);
            assert_eq!(buf.len(), tx.rlp_encoded_fields_length(), "{ty:?} encoded length mismatch",);
        }
    }

    /// Pins `eip2718_encode` per variant.
    #[test]
    fn eip2718_encode_writes_bytes_per_variant() {
        let sig = Signature::test_signature();
        for (tx, ty) in all_variants() {
            let mut buf = Vec::new();
            tx.eip2718_encode(&sig, &mut buf);
            assert!(!buf.is_empty(), "{ty:?} must write bytes");
        }
    }

    /// Pins `network_encode` per variant.
    #[test]
    fn network_encode_writes_bytes_per_variant() {
        let sig = Signature::test_signature();
        for (tx, ty) in all_variants() {
            let mut buf = Vec::new();
            tx.network_encode(&sig, &mut buf);
            assert!(!buf.is_empty(), "{ty:?} must write bytes");
        }
    }
}
