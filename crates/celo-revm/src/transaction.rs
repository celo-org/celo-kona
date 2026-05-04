//! Celo transaction types and trait implementations.

use alloy_eips::{Encodable2718, Typed2718};
use alloy_evm::{FromRecoveredTx, FromTxWithEncoded, IntoTxEnv};
use alloy_primitives::{Address, Bytes};
use auto_impl::auto_impl;
use celo_alloy_consensus::{CeloTxEnvelope, CeloTxType, TxCip64};
use op_alloy_consensus::TxDeposit;
use op_revm::{
    OpTransaction,
    transaction::{OpTxTr, deposit::DepositTransactionParts},
};
use revm::{
    context::TxEnv,
    context_interface::transaction::Transaction,
    handler::SystemCallTx,
    primitives::{B256, Log, TxKind, U256},
};
use std::vec::Vec;

#[auto_impl(&, &mut, Box, Arc)]
pub trait CeloTxTr: OpTxTr {
    fn fee_currency(&self) -> Option<Address>;

    /// Returns `true` if transaction is of type CIP-64.
    fn is_cip64(&self) -> bool {
        self.tx_type() == CeloTxType::Cip64 as u8
    }

    /// Returns `true` if this transaction pays fees in native CELO.
    ///
    /// A CIP-64 tx that sets `feeCurrency` to the zero address is treated as native,
    /// because the zero address cannot host an ERC20 fee currency contract. This
    /// matches op-geth's `feeCurrency == nil` check (Go's nil `common.Address` is
    /// the zero value).
    fn is_fee_in_celo(&self) -> bool {
        match self.fee_currency() {
            None => true,
            Some(addr) => addr == Address::ZERO,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
/// Logs and gas tracking for CIP-64 debit and credit EVM calls.
pub struct Cip64Info {
    /// Gas used by the debit call (net, after refunds)
    pub debit_gas_used: u64,
    /// Gas refunded by the debit call
    pub debit_gas_refunded: u64,
    /// Gas used by the credit call (net, after refunds)
    pub credit_gas_used: u64,
    /// Gas refunded by the credit call
    pub credit_gas_refunded: u64,
    /// Logs from system calls (debit/credit) that need to be merged into the final receipt
    pub logs_pre: Vec<Log>,
    pub logs_post: Vec<Log>,
    /// Base fee converted to the ERC20 fee currency (set during handler execution)
    pub base_fee_in_erc20: Option<u128>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CeloTransaction<T: Transaction> {
    pub op_tx: OpTransaction<T>,
    pub fee_currency: Option<Address>,
    pub cip64_tx_info: Option<Cip64Info>,
    /// Pre-computed effective gas price for CIP-64 transactions.
    /// This is calculated using the base fee converted to the fee currency.
    /// When set, this value is returned by `effective_gas_price()` instead of
    /// computing it from the native base fee.
    pub effective_gas_price: Option<u128>,
}

impl<T: Transaction> CeloTransaction<T> {
    pub fn new(op_tx: OpTransaction<T>) -> Self {
        Self {
            op_tx,
            fee_currency: None,
            cip64_tx_info: None,
            effective_gas_price: None,
        }
    }
}

impl Default for CeloTransaction<TxEnv> {
    fn default() -> Self {
        Self {
            op_tx: OpTransaction::default(),
            fee_currency: None,
            cip64_tx_info: None,
            effective_gas_price: None,
        }
    }
}

impl SystemCallTx for CeloTransaction<TxEnv> {
    fn new_system_tx_with_caller(
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Self {
        Self::new_system_tx_with_gas_limit(caller, system_contract_address, data, 30_000_000)
    }
}

impl CeloTransaction<TxEnv> {
    /// Creates new transaction for system call with custom gas limit.
    pub fn new_system_tx_with_gas_limit(
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
        gas_limit: u64,
    ) -> Self {
        CeloTransaction::new(OpTransaction::new(TxEnv {
            caller,
            data,
            kind: TxKind::Call(system_contract_address),
            gas_limit,
            ..Default::default()
        }))
    }
}

#[cfg(feature = "reth")]
impl<T: reth_evm::TransactionEnv> reth_evm::TransactionEnv for CeloTransaction<T> {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        reth_evm::TransactionEnv::set_gas_limit(&mut self.op_tx, gas_limit);
    }

    fn nonce(&self) -> u64 {
        reth_evm::TransactionEnv::nonce(&self.op_tx)
    }

    fn set_nonce(&mut self, nonce: u64) {
        reth_evm::TransactionEnv::set_nonce(&mut self.op_tx, nonce);
    }

    fn set_access_list(&mut self, access_list: alloy_eips::eip2930::AccessList) {
        reth_evm::TransactionEnv::set_access_list(&mut self.op_tx, access_list);
    }
}

impl<T: Transaction> Transaction for CeloTransaction<T> {
    type AccessListItem<'a>
        = T::AccessListItem<'a>
    where
        T: 'a;
    type Authorization<'a>
        = T::Authorization<'a>
    where
        T: 'a;

    fn tx_type(&self) -> u8 {
        self.op_tx.tx_type()
    }

    fn caller(&self) -> Address {
        self.op_tx.caller()
    }

    fn gas_limit(&self) -> u64 {
        self.op_tx.gas_limit()
    }

    fn value(&self) -> U256 {
        self.op_tx.value()
    }

    fn input(&self) -> &Bytes {
        self.op_tx.input()
    }

    fn nonce(&self) -> u64 {
        self.op_tx.nonce()
    }

    fn kind(&self) -> TxKind {
        self.op_tx.kind()
    }

    fn chain_id(&self) -> Option<u64> {
        self.op_tx.chain_id()
    }

    fn access_list(&self) -> Option<impl Iterator<Item = Self::AccessListItem<'_>>> {
        self.op_tx.access_list()
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.op_tx.max_priority_fee_per_gas()
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.op_tx.max_fee_per_gas()
    }

    fn gas_price(&self) -> u128 {
        self.op_tx.gas_price()
    }

    fn blob_versioned_hashes(&self) -> &[B256] {
        self.op_tx.blob_versioned_hashes()
    }

    fn max_fee_per_blob_gas(&self) -> u128 {
        self.op_tx.max_fee_per_blob_gas()
    }

    fn effective_gas_price(&self, base_fee: u128) -> u128 {
        // For CIP-64 transactions, return the pre-computed effective gas price
        // that was calculated using the base fee converted to the fee currency.
        // This ensures the GASPRICE opcode returns the correct value.
        if let Some(egp) = self.effective_gas_price {
            return egp;
        }
        self.op_tx.effective_gas_price(base_fee)
    }

    fn authorization_list_len(&self) -> usize {
        self.op_tx.authorization_list_len()
    }

    fn authorization_list(&self) -> impl Iterator<Item = Self::Authorization<'_>> {
        self.op_tx.authorization_list()
    }
}

impl<T: Transaction> OpTxTr for CeloTransaction<T> {
    fn enveloped_tx(&self) -> Option<&Bytes> {
        self.op_tx.enveloped_tx()
    }

    fn source_hash(&self) -> Option<B256> {
        self.op_tx.source_hash()
    }

    fn mint(&self) -> Option<u128> {
        self.op_tx.mint()
    }

    fn is_system_transaction(&self) -> bool {
        self.op_tx.is_system_transaction()
    }
}

impl<T: Transaction> CeloTxTr for CeloTransaction<T> {
    fn fee_currency(&self) -> Option<Address> {
        self.fee_currency
    }
}

#[cfg(feature = "alloy-op-evm")]
impl alloy_op_evm::block::OpTxEnv for CeloTransaction<TxEnv> {
    fn encoded_bytes(&self) -> Option<&Bytes> {
        self.enveloped_tx()
    }
}

// Trait implementations for transaction conversion (from alloy-evm)

impl<T: Transaction> IntoTxEnv<Self> for CeloTransaction<T> {
    fn into_tx_env(self) -> Self {
        self
    }
}

impl FromTxWithEncoded<CeloTxEnvelope> for CeloTransaction<TxEnv> {
    fn from_encoded_tx(tx: &CeloTxEnvelope, caller: Address, encoded: Bytes) -> Self {
        let base = match tx {
            CeloTxEnvelope::Legacy(tx) => TxEnv::from_recovered_tx(tx.tx(), caller),
            CeloTxEnvelope::Eip1559(tx) => TxEnv::from_recovered_tx(tx.tx(), caller),
            CeloTxEnvelope::Eip2930(tx) => TxEnv::from_recovered_tx(tx.tx(), caller),
            CeloTxEnvelope::Eip7702(tx) => TxEnv::from_recovered_tx(tx.tx(), caller),
            CeloTxEnvelope::Cip64(tx) => {
                let TxCip64 {
                    chain_id,
                    nonce,
                    gas_limit,
                    to,
                    value,
                    input,
                    max_fee_per_gas,
                    max_priority_fee_per_gas,
                    access_list,
                    fee_currency: _,
                } = tx.tx();
                TxEnv {
                    tx_type: tx.ty(),
                    caller,
                    gas_limit: *gas_limit,
                    gas_price: *max_fee_per_gas,
                    kind: *to,
                    value: *value,
                    data: input.clone(),
                    nonce: *nonce,
                    chain_id: Some(*chain_id),
                    gas_priority_fee: Some(*max_priority_fee_per_gas),
                    access_list: access_list.clone(),
                    ..Default::default()
                }
            }
            CeloTxEnvelope::Deposit(tx) => {
                let TxDeposit {
                    to,
                    value,
                    gas_limit,
                    input,
                    source_hash: _,
                    from: _,
                    mint: _,
                    is_system_transaction: _,
                } = tx.inner();
                TxEnv {
                    tx_type: tx.ty(),
                    caller,
                    gas_limit: *gas_limit,
                    kind: *to,
                    value: *value,
                    data: input.clone(),
                    ..Default::default()
                }
            }
        };

        let deposit = if let CeloTxEnvelope::Deposit(tx) = tx {
            DepositTransactionParts {
                source_hash: tx.source_hash,
                mint: Some(tx.mint),
                is_system_transaction: tx.is_system_transaction,
            }
        } else {
            Default::default()
        };

        let fee_currency: Option<Address> = match tx {
            CeloTxEnvelope::Cip64(tx) => tx.tx().fee_currency,
            _ => None,
        };

        Self {
            op_tx: OpTransaction {
                base,
                enveloped_tx: Some(encoded),
                deposit,
            },
            fee_currency,
            cip64_tx_info: None,
            effective_gas_price: None,
        }
    }
}

impl FromRecoveredTx<CeloTxEnvelope> for CeloTransaction<TxEnv> {
    fn from_recovered_tx(tx: &CeloTxEnvelope, sender: Address) -> Self {
        let encoded = tx.encoded_2718();
        Self::from_encoded_tx(tx, sender, encoded.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Sealed;
    use alloy_consensus::SignableTransaction;
    use alloy_consensus::transaction::Either;
    use alloy_eips::eip2930::{AccessList, AccessListItem};
    use alloy_eips::eip7702::{Authorization, SignedAuthorization};
    use alloy_primitives::{Address, Signature, U256};
    use revm::primitives::TxKind;

    fn addr(b: u8) -> Address {
        Address::with_last_byte(b)
    }

    fn b256_with_byte(b: u8) -> B256 {
        let mut bytes = [0u8; 32];
        bytes[31] = b;
        B256::from(bytes)
    }

    /// Construct a CIP-64-shaped CeloTransaction with deliberately distinct
    /// field values so every forwarding accessor can be observed.
    /// Note: `source_hash` is left zero; op-revm's `OpTransaction::tx_type` flips
    /// to DEPOSIT whenever source_hash is non-zero.
    fn populated_tx() -> CeloTransaction<TxEnv> {
        CeloTransaction {
            op_tx: OpTransaction {
                base: TxEnv {
                    tx_type: CeloTxType::Cip64 as u8,
                    caller: addr(0xAA),
                    gas_limit: 21_000,
                    gas_price: 99,
                    kind: TxKind::Call(addr(0xBB)),
                    value: U256::from(7u64),
                    data: Bytes::from_static(&[0xDE, 0xAD]),
                    nonce: 42,
                    chain_id: Some(0xa4ec),
                    access_list: AccessList(vec![AccessListItem {
                        address: addr(0xDD),
                        storage_keys: vec![b256_with_byte(0xEE)],
                    }]),
                    gas_priority_fee: Some(11),
                    blob_hashes: vec![b256_with_byte(0xCC)],
                    max_fee_per_blob_gas: 13,
                    authorization_list: vec![Either::Left(SignedAuthorization::new_unchecked(
                        Authorization {
                            chain_id: U256::from(0xa4ec_u64),
                            address: addr(0xFF),
                            nonce: 99,
                        },
                        0,
                        U256::from(1u64),
                        U256::from(2u64),
                    ))],
                },
                enveloped_tx: Some(Bytes::from_static(&[0x01, 0x02, 0x03])),
                deposit: DepositTransactionParts::default(),
            },
            fee_currency: Some(addr(0x77)),
            cip64_tx_info: None,
            effective_gas_price: None,
        }
    }

    fn populated_deposit_tx() -> CeloTransaction<TxEnv> {
        let mut tx = populated_tx();
        tx.op_tx.deposit = DepositTransactionParts {
            source_hash: b256_with_byte(0xEE),
            mint: Some(17),
            is_system_transaction: true,
        };
        tx
    }

    #[test]
    fn transaction_trait_forwards_every_field() {
        let tx = populated_tx();
        assert_eq!(tx.tx_type(), CeloTxType::Cip64 as u8);
        assert_eq!(tx.caller(), addr(0xAA));
        assert_eq!(tx.gas_limit(), 21_000);
        assert_eq!(tx.value(), U256::from(7u64));
        assert_eq!(tx.input(), &Bytes::from_static(&[0xDE, 0xAD]));
        assert_eq!(<CeloTransaction<TxEnv> as Transaction>::nonce(&tx), 42);
        assert_eq!(tx.kind(), TxKind::Call(addr(0xBB)));
        assert_eq!(tx.chain_id(), Some(0xa4ec));
        assert_eq!(tx.max_priority_fee_per_gas(), Some(11));
        assert_eq!(tx.max_fee_per_gas(), 99);
        assert_eq!(tx.gas_price(), 99);
        assert_eq!(tx.blob_versioned_hashes(), &[b256_with_byte(0xCC)][..]);
        assert_eq!(tx.max_fee_per_blob_gas(), 13);

        // Access list must round-trip through the trait. The
        // `Some(empty())` mutation on `access_list()` would surface as a zero
        // count here.
        let access_list_items: Vec<_> = tx.access_list().expect("Some").collect();
        assert_eq!(access_list_items.len(), 1);

        // Authorization list must report >0 length and yield the entry.
        assert_eq!(tx.authorization_list_len(), 1);
        assert_eq!(tx.authorization_list().count(), 1);
    }

    #[test]
    fn op_tx_tr_forwards_deposit_fields() {
        let tx = populated_deposit_tx();
        assert_eq!(tx.source_hash(), Some(b256_with_byte(0xEE)));
        assert_eq!(tx.mint(), Some(17));
        assert!(tx.is_system_transaction());
    }

    #[test]
    fn celo_tx_tr_returns_fee_currency() {
        let tx = populated_tx();
        assert_eq!(tx.fee_currency(), Some(addr(0x77)));
    }

    #[test]
    fn is_cip64_distinguishes_tx_types() {
        let mut tx = populated_tx();
        assert!(tx.is_cip64());
        // tx_type other than Cip64 → not cip64.
        tx.op_tx.base.tx_type = CeloTxType::Eip1559 as u8;
        assert!(!tx.is_cip64());
    }

    #[test]
    fn is_fee_in_celo_treats_zero_addr_as_native() {
        let mut tx = populated_tx();
        // Non-zero fee currency → ERC20.
        assert!(!tx.is_fee_in_celo());
        // None → native.
        tx.fee_currency = None;
        assert!(tx.is_fee_in_celo());
        // Some(ZERO) → still native (matches op-geth).
        tx.fee_currency = Some(Address::ZERO);
        assert!(tx.is_fee_in_celo());
    }

    #[test]
    fn effective_gas_price_overrides_when_set() {
        let mut tx = populated_tx();
        tx.effective_gas_price = Some(123_456);
        assert_eq!(tx.effective_gas_price(99), 123_456);
        // Without override, falls back to op-tx behavior.
        tx.effective_gas_price = None;
        // (max_fee_per_gas - base_fee).min(max_priority) + base_fee = (99-90).min(11) + 90 = 99
        assert_eq!(tx.effective_gas_price(90), 99);
    }

    #[test]
    fn op_tx_env_encoded_bytes_forwards_enveloped_tx() {
        let tx = populated_tx();
        let bytes = <CeloTransaction<TxEnv> as alloy_op_evm::block::OpTxEnv>::encoded_bytes(&tx);
        assert_eq!(bytes, Some(&Bytes::from_static(&[0x01, 0x02, 0x03])));
    }

    #[test]
    fn new_system_tx_sets_caller_kind_data_and_gas() {
        let caller = addr(0x11);
        let target = addr(0x22);
        let data = Bytes::from_static(&[0x55, 0x66]);
        let tx = CeloTransaction::<TxEnv>::new_system_tx_with_gas_limit(
            caller,
            target,
            data.clone(),
            12_345,
        );
        assert_eq!(tx.caller(), caller);
        assert_eq!(tx.gas_limit(), 12_345);
        assert_eq!(tx.kind(), TxKind::Call(target));
        assert_eq!(tx.input(), &data);
    }

    #[test]
    fn new_system_tx_with_caller_uses_30m_gas_limit() {
        let tx = CeloTransaction::<TxEnv>::new_system_tx_with_caller(
            addr(0x11),
            addr(0x22),
            Bytes::new(),
        );
        // Default L1-style system tx limit per `new_system_tx_with_caller`.
        assert_eq!(tx.gas_limit(), 30_000_000);
    }

    fn cip64_envelope() -> (TxCip64, CeloTxEnvelope) {
        let tx = TxCip64 {
            chain_id: 0xa4ec,
            nonce: 0x705,
            gas_limit: 0x3644c,
            to: TxKind::Call(addr(0x99)),
            value: U256::from(123u64),
            input: Bytes::from_static(&[0xCA, 0xFE]),
            max_fee_per_gas: 0x26442dbed,
            max_priority_fee_per_gas: 0x4d7ee,
            access_list: AccessList(vec![AccessListItem {
                address: addr(0x44),
                storage_keys: vec![b256_with_byte(0x33)],
            }]),
            fee_currency: Some(addr(0x55)),
        };
        let signed = tx.clone().into_signed(Signature::test_signature());
        (tx, CeloTxEnvelope::Cip64(signed))
    }

    #[test]
    fn from_encoded_tx_cip64_populates_all_fields() {
        let (tx, envelope) = cip64_envelope();
        let caller = addr(0xAB);
        let encoded = Bytes::from_static(&[0xFE, 0xED]);
        let out = CeloTransaction::<TxEnv>::from_encoded_tx(&envelope, caller, encoded.clone());

        assert_eq!(out.tx_type(), CeloTxType::Cip64 as u8);
        assert_eq!(out.caller(), caller);
        assert_eq!(out.gas_limit(), tx.gas_limit);
        assert_eq!(out.gas_price(), tx.max_fee_per_gas);
        assert_eq!(out.kind(), tx.to);
        assert_eq!(out.value(), tx.value);
        assert_eq!(out.input(), &tx.input);
        assert_eq!(
            <CeloTransaction<TxEnv> as Transaction>::nonce(&out),
            tx.nonce
        );
        assert_eq!(out.chain_id(), Some(tx.chain_id));
        assert_eq!(
            out.max_priority_fee_per_gas(),
            Some(tx.max_priority_fee_per_gas)
        );
        assert_eq!(out.fee_currency(), tx.fee_currency);
        assert_eq!(out.op_tx.enveloped_tx, Some(encoded));
        // access_list field assignment must round-trip — the
        // `delete field access_list` mutation defaults this to empty.
        assert_eq!(out.op_tx.base.access_list.0.len(), 1);
    }

    #[test]
    fn from_encoded_tx_deposit_populates_all_fields() {
        let dep = TxDeposit {
            source_hash: b256_with_byte(0x77),
            from: addr(0xAB),
            to: TxKind::Call(addr(0x88)),
            mint: 555,
            value: U256::from(42u64),
            gas_limit: 21_000,
            is_system_transaction: true,
            input: Bytes::from_static(&[0xCA, 0xFE]),
        };
        let envelope =
            CeloTxEnvelope::Deposit(Sealed::new_unchecked(dep.clone(), b256_with_byte(0)));
        let caller = addr(0xCD);
        let out =
            CeloTransaction::<TxEnv>::from_encoded_tx(&envelope, caller, Bytes::from_static(&[1]));

        // Deposit branch sets only a subset of TxEnv fields explicitly; the rest
        // come from `..Default::default()`. The mutants targeting these specific
        // fields must observe the deposit's values, not zeros.
        assert_eq!(out.caller(), caller);
        assert_eq!(out.gas_limit(), dep.gas_limit);
        assert_eq!(out.kind(), dep.to);
        assert_eq!(out.value(), dep.value);
        assert_eq!(out.input(), &dep.input);
        assert_eq!(out.source_hash(), Some(dep.source_hash));
        assert_eq!(out.mint(), Some(dep.mint));
        assert!(out.is_system_transaction());
        assert_eq!(out.fee_currency(), None);
        // Assert on the raw base.tx_type, not Transaction::tx_type — the
        // latter routes through OpTransaction's deposit-detection logic, which
        // would mask a "delete field tx_type" mutation in the deposit branch.
        assert_eq!(
            out.op_tx.base.tx_type,
            CeloTxEnvelope::Deposit(Sealed::new_unchecked(dep, b256_with_byte(0))).ty()
        );
    }

    #[test]
    fn from_recovered_tx_round_trips_encoded_bytes() {
        let (_, envelope) = cip64_envelope();
        let caller = addr(0xAB);
        let out = CeloTransaction::<TxEnv>::from_recovered_tx(&envelope, caller);
        // The `from_recovered_tx -> Default::default()` mutation surfaces here:
        // a default CeloTransaction has tx_type=0 (Legacy) and no fee_currency.
        assert_eq!(out.tx_type(), CeloTxType::Cip64 as u8);
        assert!(out.fee_currency().is_some());
        // And the encoded payload must be present (it's what `from_recovered_tx`
        // computes via `encoded_2718`).
        assert!(out.op_tx.enveloped_tx.is_some());
    }

    #[test]
    fn test_cip64_transaction_fields() {
        let cip64_tx = CeloTransaction {
            op_tx: OpTransaction {
                base: TxEnv {
                    tx_type: CeloTxType::Cip64 as u8,
                    gas_limit: 10,
                    gas_price: 100,
                    gas_priority_fee: Some(5),
                    ..Default::default()
                },
                enveloped_tx: None,
                deposit: DepositTransactionParts::default(),
            },
            fee_currency: Some(Address::with_last_byte(1)),
            cip64_tx_info: None,
            effective_gas_price: None,
        };
        // Verify transaction type
        assert_eq!(cip64_tx.tx_type(), CeloTxType::Cip64 as u8);
        // Verify common fields access
        assert_eq!(cip64_tx.gas_limit(), 10);
        assert_eq!(cip64_tx.kind(), TxKind::Call(Address::ZERO));
        // Verify gas related calculations
        assert_eq!(cip64_tx.effective_gas_price(90), 95);
        assert_eq!(cip64_tx.max_fee_per_gas(), 100);
        // Verify CIP-64 fields
        assert_eq!(cip64_tx.fee_currency(), Some(Address::with_last_byte(1)));
    }
}
