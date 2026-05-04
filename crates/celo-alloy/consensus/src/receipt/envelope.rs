//! Receipt envelope types for Celo.

use crate::{
    CeloTxType,
    receipt::{CeloCip64Receipt, CeloCip64ReceiptWithBloom},
};
use alloy_consensus::{Eip658Value, Receipt, ReceiptWithBloom, TxReceipt};
use alloy_eips::{
    Typed2718,
    eip2718::{Decodable2718, Eip2718Error, Eip2718Result, Encodable2718, IsTyped2718},
};
use alloy_primitives::{Bloom, Log, logs_bloom};
use alloy_rlp::{BufMut, Decodable, Encodable, length_of_length};
use op_alloy_consensus::{OpDepositReceipt, OpDepositReceiptWithBloom};
use std::vec::Vec;

/// Receipt envelope, as defined in [EIP-2718], modified for Celo.
///
/// This enum distinguishes between tagged and untagged legacy receipts, as the
/// in-protocol merkle tree may commit to EITHER 0-prefixed or raw. Therefore
/// we must ensure that encoding returns the precise byte-array that was
/// decoded, preserving the presence or absence of the `TransactionType` flag.
///
/// Transaction receipt payloads are specified in their respective EIPs.
///
/// [EIP-2718]: https://eips.ethereum.org/EIPS/eip-2718
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "type"))]
pub enum CeloReceiptEnvelope<T = Log> {
    /// Receipt envelope with no type flag.
    #[cfg_attr(feature = "serde", serde(rename = "0x0", alias = "0x00"))]
    Legacy(ReceiptWithBloom<Receipt<T>>),
    /// Receipt envelope with type flag 1, containing a [EIP-2930] receipt.
    ///
    /// [EIP-2930]: https://eips.ethereum.org/EIPS/eip-2930
    #[cfg_attr(feature = "serde", serde(rename = "0x1", alias = "0x01"))]
    Eip2930(ReceiptWithBloom<Receipt<T>>),
    /// Receipt envelope with type flag 2, containing a [EIP-1559] receipt.
    ///
    /// [EIP-1559]: https://eips.ethereum.org/EIPS/eip-1559
    #[cfg_attr(feature = "serde", serde(rename = "0x2", alias = "0x02"))]
    Eip1559(ReceiptWithBloom<Receipt<T>>),
    /// Receipt envelope with type flag 4, containing a [EIP-7702] receipt.
    ///
    /// [EIP-7702]: https://eips.ethereum.org/EIPS/eip-7702
    #[cfg_attr(feature = "serde", serde(rename = "0x4", alias = "0x04"))]
    Eip7702(ReceiptWithBloom<Receipt<T>>),
    /// Receipt envelope with type flag 123, containing a [CIP-64] receipt.
    ///
    /// [CIP-64]: https://github.com/celo-org/celo-proposals/blob/master/CIPs/cip-0064.md
    #[cfg_attr(feature = "serde", serde(rename = "0x7b", alias = "0x7B"))]
    Cip64(ReceiptWithBloom<CeloCip64Receipt<T>>),
    /// Receipt envelope with type flag 126, containing a [deposit] receipt.
    ///
    /// [deposit]: https://specs.optimism.io/protocol/deposits.html
    #[cfg_attr(feature = "serde", serde(rename = "0x7e", alias = "0x7E"))]
    Deposit(ReceiptWithBloom<OpDepositReceipt<T>>),
}

impl CeloReceiptEnvelope<Log> {
    /// Creates a new [`CeloReceiptEnvelope`] from the given parts.
    pub fn from_parts<'a>(
        status: bool,
        cumulative_gas_used: u64,
        logs: impl IntoIterator<Item = &'a Log>,
        tx_type: CeloTxType,
        deposit_nonce: Option<u64>,
        deposit_receipt_version: Option<u64>,
        base_fee: Option<u128>,
    ) -> Self {
        let logs = logs.into_iter().cloned().collect::<Vec<_>>();
        let logs_bloom = logs_bloom(&logs);
        let inner_receipt =
            Receipt { status: Eip658Value::Eip658(status), cumulative_gas_used, logs };
        match tx_type {
            CeloTxType::Legacy => {
                Self::Legacy(ReceiptWithBloom { receipt: inner_receipt, logs_bloom })
            }
            CeloTxType::Eip2930 => {
                Self::Eip2930(ReceiptWithBloom { receipt: inner_receipt, logs_bloom })
            }
            CeloTxType::Eip1559 => {
                Self::Eip1559(ReceiptWithBloom { receipt: inner_receipt, logs_bloom })
            }
            CeloTxType::Eip7702 => {
                Self::Eip7702(ReceiptWithBloom { receipt: inner_receipt, logs_bloom })
            }
            CeloTxType::Cip64 => {
                let inner = CeloCip64ReceiptWithBloom {
                    receipt: CeloCip64Receipt { inner: inner_receipt, base_fee },
                    logs_bloom,
                };
                Self::Cip64(inner)
            }
            CeloTxType::Deposit => {
                let inner = OpDepositReceiptWithBloom {
                    receipt: OpDepositReceipt {
                        inner: inner_receipt,
                        deposit_nonce,
                        deposit_receipt_version,
                    },
                    logs_bloom,
                };
                Self::Deposit(inner)
            }
        }
    }
}

impl<T> CeloReceiptEnvelope<T> {
    /// Return the [`CeloTxType`] of the inner receipt.
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

    /// Return true if the transaction was successful.
    pub const fn is_success(&self) -> bool {
        self.status()
    }

    /// Returns the success status of the receipt's transaction.
    pub const fn status(&self) -> bool {
        self.as_receipt().unwrap().status.coerce_status()
    }

    /// Returns the cumulative gas used at this receipt.
    pub const fn cumulative_gas_used(&self) -> u64 {
        self.as_receipt().unwrap().cumulative_gas_used
    }

    /// Return the receipt logs.
    #[allow(clippy::missing_const_for_fn)]
    pub fn logs(&self) -> &[T] {
        &self.as_receipt().unwrap().logs
    }

    /// Return the receipt's bloom.
    pub const fn logs_bloom(&self) -> &Bloom {
        match self {
            Self::Legacy(t) => &t.logs_bloom,
            Self::Eip2930(t) => &t.logs_bloom,
            Self::Eip1559(t) => &t.logs_bloom,
            Self::Eip7702(t) => &t.logs_bloom,
            Self::Cip64(t) => &t.logs_bloom,
            Self::Deposit(t) => &t.logs_bloom,
        }
    }

    /// Return the receipt's deposit_nonce if it is a deposit receipt.
    pub fn deposit_nonce(&self) -> Option<u64> {
        self.as_deposit_receipt().and_then(|r| r.deposit_nonce)
    }

    /// Return the receipt's deposit version if it is a deposit receipt.
    pub fn deposit_receipt_version(&self) -> Option<u64> {
        self.as_deposit_receipt().and_then(|r| r.deposit_receipt_version)
    }

    /// Return the receipt's base fee if it is a cip64 receipt.
    pub fn base_fee(&self) -> Option<u128> {
        self.as_cip64_receipt().and_then(|r| r.base_fee)
    }

    /// Returns the deposit receipt if it is a deposit receipt.
    pub const fn as_deposit_receipt_with_bloom(&self) -> Option<&OpDepositReceiptWithBloom<T>> {
        match self {
            Self::Deposit(t) => Some(t),
            _ => None,
        }
    }

    /// Returns the deposit receipt if it is a deposit receipt.
    pub const fn as_deposit_receipt(&self) -> Option<&OpDepositReceipt<T>> {
        match self {
            Self::Deposit(t) => Some(&t.receipt),
            _ => None,
        }
    }

    /// Returns the cip64 receipt if it is a cip64 receipt.
    pub const fn as_cip64_receipt_with_bloom(&self) -> Option<&CeloCip64ReceiptWithBloom<T>> {
        match self {
            Self::Cip64(t) => Some(t),
            _ => None,
        }
    }

    /// Returns the cip64 receipt if it is a cip64 receipt.
    pub const fn as_cip64_receipt(&self) -> Option<&CeloCip64Receipt<T>> {
        match self {
            Self::Cip64(t) => Some(&t.receipt),
            _ => None,
        }
    }

    /// Return the inner receipt. Currently this is infallible, however, future
    /// receipt types may be added.
    pub const fn as_receipt(&self) -> Option<&Receipt<T>> {
        match self {
            Self::Legacy(t) | Self::Eip2930(t) | Self::Eip1559(t) | Self::Eip7702(t) => {
                Some(&t.receipt)
            }
            Self::Cip64(t) => Some(&t.receipt.inner),
            Self::Deposit(t) => Some(&t.receipt.inner),
        }
    }
}

impl CeloReceiptEnvelope {
    /// Get the length of the inner receipt in the 2718 encoding.
    pub fn inner_length(&self) -> usize {
        match self {
            Self::Legacy(t) => t.length(),
            Self::Eip2930(t) => t.length(),
            Self::Eip1559(t) => t.length(),
            Self::Eip7702(t) => t.length(),
            Self::Cip64(t) => t.length(),
            Self::Deposit(t) => t.length(),
        }
    }

    /// Calculate the length of the rlp payload of the network encoded receipt.
    pub fn rlp_payload_length(&self) -> usize {
        let length = self.inner_length();
        match self {
            Self::Legacy(_) => length,
            _ => length + 1,
        }
    }
}

impl<T> TxReceipt for CeloReceiptEnvelope<T>
where
    T: Clone + core::fmt::Debug + PartialEq + Eq + Send + Sync,
{
    type Log = T;

    fn status_or_post_state(&self) -> Eip658Value {
        self.as_receipt().unwrap().status
    }

    fn status(&self) -> bool {
        self.as_receipt().unwrap().status.coerce_status()
    }

    /// Return the receipt's bloom.
    fn bloom(&self) -> Bloom {
        *self.logs_bloom()
    }

    fn bloom_cheap(&self) -> Option<Bloom> {
        Some(self.bloom())
    }

    /// Returns the cumulative gas used at this receipt.
    fn cumulative_gas_used(&self) -> u64 {
        self.as_receipt().unwrap().cumulative_gas_used
    }

    /// Return the receipt logs.
    fn logs(&self) -> &[T] {
        &self.as_receipt().unwrap().logs
    }
}

impl Encodable for CeloReceiptEnvelope {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.network_encode(out)
    }

    fn length(&self) -> usize {
        let mut payload_length = self.rlp_payload_length();
        if !self.is_legacy() {
            payload_length += length_of_length(payload_length);
        }
        payload_length
    }
}

impl Decodable for CeloReceiptEnvelope {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Self::network_decode(buf)
            .map_or_else(|_| Err(alloy_rlp::Error::Custom("Unexpected type")), Ok)
    }
}

impl Typed2718 for CeloReceiptEnvelope {
    fn ty(&self) -> u8 {
        let ty = match self {
            Self::Legacy(_) => CeloTxType::Legacy,
            Self::Eip2930(_) => CeloTxType::Eip2930,
            Self::Eip1559(_) => CeloTxType::Eip1559,
            Self::Eip7702(_) => CeloTxType::Eip7702,
            Self::Cip64(_) => CeloTxType::Cip64,
            Self::Deposit(_) => CeloTxType::Deposit,
        };
        ty.into()
    }
}

impl IsTyped2718 for CeloReceiptEnvelope {
    fn is_type(type_id: u8) -> bool {
        <CeloTxType as IsTyped2718>::is_type(type_id)
    }
}

impl Encodable2718 for CeloReceiptEnvelope {
    fn encode_2718_len(&self) -> usize {
        self.inner_length() + !self.is_legacy() as usize
    }

    fn encode_2718(&self, out: &mut dyn BufMut) {
        match self.type_flag() {
            None => {}
            Some(ty) => out.put_u8(ty),
        }
        match self {
            Self::Deposit(t) => t.encode(out),
            Self::Cip64(t) => t.encode(out),
            Self::Legacy(t) | Self::Eip2930(t) | Self::Eip1559(t) | Self::Eip7702(t) => {
                t.encode(out)
            }
        }
    }
}

impl Decodable2718 for CeloReceiptEnvelope {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        match ty.try_into().map_err(|_| Eip2718Error::UnexpectedType(ty))? {
            CeloTxType::Legacy => {
                Err(alloy_rlp::Error::Custom("type-0 eip2718 transactions are not supported")
                    .into())
            }
            CeloTxType::Eip1559 => Ok(Self::Eip1559(Decodable::decode(buf)?)),
            CeloTxType::Eip7702 => Ok(Self::Eip7702(Decodable::decode(buf)?)),
            CeloTxType::Eip2930 => Ok(Self::Eip2930(Decodable::decode(buf)?)),
            CeloTxType::Cip64 => Ok(Self::Cip64(Decodable::decode(buf)?)),
            CeloTxType::Deposit => Ok(Self::Deposit(Decodable::decode(buf)?)),
        }
    }

    fn fallback_decode(buf: &mut &[u8]) -> Eip2718Result<Self> {
        Ok(Self::Legacy(Decodable::decode(buf)?))
    }
}

#[cfg(all(test, feature = "arbitrary"))]
impl<'a, T> arbitrary::Arbitrary<'a> for CeloReceiptEnvelope<T>
where
    T: arbitrary::Arbitrary<'a>,
{
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        match u.int_in_range(0..=5)? {
            0 => Ok(Self::Legacy(ReceiptWithBloom::arbitrary(u)?)),
            1 => Ok(Self::Eip2930(ReceiptWithBloom::arbitrary(u)?)),
            2 => Ok(Self::Eip1559(ReceiptWithBloom::arbitrary(u)?)),
            4 => Ok(Self::Eip7702(ReceiptWithBloom::arbitrary(u)?)),
            5 => Ok(Self::Cip64(CeloCip64ReceiptWithBloom::arbitrary(u)?)),
            _ => Ok(Self::Deposit(OpDepositReceiptWithBloom::arbitrary(u)?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Receipt, ReceiptWithBloom};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Log, LogData, address, b256, bytes, hex};
    use alloy_rlp::Encodable;

    #[cfg(not(feature = "std"))]
    use std::vec;

    // Test vector from: https://eips.ethereum.org/EIPS/eip-2481
    #[test]
    fn encode_legacy_receipt() {
        let expected = hex!(
            "f901668001b9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000f85ff85d940000000000000000000000000000000000000011f842a0000000000000000000000000000000000000000000000000000000000000deada0000000000000000000000000000000000000000000000000000000000000beef830100ff"
        );

        let mut data = vec![];
        let receipt = CeloReceiptEnvelope::Legacy(ReceiptWithBloom {
            receipt: Receipt {
                status: false.into(),
                cumulative_gas_used: 0x1,
                logs: vec![Log {
                    address: address!("0000000000000000000000000000000000000011"),
                    data: LogData::new_unchecked(
                        vec![
                            b256!(
                                "000000000000000000000000000000000000000000000000000000000000dead"
                            ),
                            b256!(
                                "000000000000000000000000000000000000000000000000000000000000beef"
                            ),
                        ],
                        bytes!("0100ff"),
                    ),
                }],
            },
            logs_bloom: [0; 256].into(),
        });

        receipt.network_encode(&mut data);

        // check that the rlp length equals the length of the expected rlp
        assert_eq!(receipt.length(), expected.len());
        assert_eq!(data, expected);
    }

    #[test]
    fn legacy_receipt_from_parts() {
        let receipt = CeloReceiptEnvelope::from_parts(
            true,
            100,
            vec![],
            CeloTxType::Legacy,
            None,
            None,
            None,
        );
        assert!(receipt.status());
        assert_eq!(receipt.cumulative_gas_used(), 100);
        assert_eq!(receipt.logs().len(), 0);
        assert_eq!(receipt.tx_type(), CeloTxType::Legacy);
    }

    #[test]
    fn cip64_receipt_from_parts() {
        let receipt = CeloReceiptEnvelope::from_parts(
            true,
            100,
            vec![],
            CeloTxType::Cip64,
            None,
            None,
            Some(1_u128),
        );
        assert!(receipt.status());
        assert_eq!(receipt.cumulative_gas_used(), 100);
        assert_eq!(receipt.logs().len(), 0);
        assert_eq!(receipt.tx_type(), CeloTxType::Cip64);
        assert_eq!(receipt.base_fee(), Some(1));
    }

    fn make_receipt_envelope(
        tx_type: CeloTxType,
        status: bool,
        cumulative_gas_used: u64,
        deposit_nonce: Option<u64>,
        deposit_receipt_version: Option<u64>,
        base_fee: Option<u128>,
    ) -> CeloReceiptEnvelope {
        let logs = vec![Log {
            address: address!("0x000000000000000000000000000000000000aabb"),
            data: LogData::new_unchecked(
                vec![b256!("0xdeaddeaddeaddeaddeaddeaddeaddeaddeaddeaddeaddeaddeaddeaddeaddead")],
                bytes!("0x77"),
            ),
        }];
        CeloReceiptEnvelope::from_parts(
            status,
            cumulative_gas_used,
            &logs,
            tx_type,
            deposit_nonce,
            deposit_receipt_version,
            base_fee,
        )
    }

    fn all_receipt_envelopes() -> Vec<(CeloReceiptEnvelope, CeloTxType)> {
        vec![
            (
                make_receipt_envelope(CeloTxType::Legacy, true, 0xa1, None, None, None),
                CeloTxType::Legacy,
            ),
            (
                make_receipt_envelope(CeloTxType::Eip2930, true, 0xa2, None, None, None),
                CeloTxType::Eip2930,
            ),
            (
                make_receipt_envelope(CeloTxType::Eip1559, true, 0xa3, None, None, None),
                CeloTxType::Eip1559,
            ),
            (
                make_receipt_envelope(CeloTxType::Eip7702, true, 0xa4, None, None, None),
                CeloTxType::Eip7702,
            ),
            (
                make_receipt_envelope(CeloTxType::Cip64, true, 0xa5, None, None, Some(0xabcd)),
                CeloTxType::Cip64,
            ),
            (
                make_receipt_envelope(CeloTxType::Deposit, true, 0xa6, Some(7), Some(2), None),
                CeloTxType::Deposit,
            ),
        ]
    }

    /// Pins `tx_type` per arm AND `Typed2718::ty -> 0` against the envelope.
    #[test]
    fn receipt_envelope_tx_type_per_variant() {
        for (env, ty) in all_receipt_envelopes() {
            assert_eq!(env.tx_type(), ty);
            assert_eq!(env.ty(), ty as u8);
        }
    }

    /// Pins `is_success -> true|false` and `status -> true|false` against
    /// both successful and failed receipts.
    #[test]
    fn receipt_envelope_is_success_and_status() {
        for ty in [
            CeloTxType::Legacy,
            CeloTxType::Eip2930,
            CeloTxType::Eip1559,
            CeloTxType::Eip7702,
            CeloTxType::Cip64,
            CeloTxType::Deposit,
        ] {
            let dep_nonce = matches!(ty, CeloTxType::Deposit).then_some(1);
            let succ = make_receipt_envelope(ty, true, 1, dep_nonce, None, None);
            assert!(succ.is_success(), "{ty:?} success");
            assert!(succ.status(), "{ty:?} status");
            assert!(<CeloReceiptEnvelope as TxReceipt>::status(&succ));

            let fail = make_receipt_envelope(ty, false, 1, dep_nonce, None, None);
            assert!(!fail.is_success(), "{ty:?} fail success");
            assert!(!fail.status(), "{ty:?} fail status");
            assert!(!<CeloReceiptEnvelope as TxReceipt>::status(&fail));
        }
    }

    /// Pins `logs -> empty` for the inherent `logs()` AND TxReceipt forwarder
    /// per variant.
    #[test]
    fn receipt_envelope_logs_per_variant() {
        for (env, ty) in all_receipt_envelopes() {
            assert_eq!(env.logs().len(), 1, "{ty:?} inherent");
            assert_eq!(<CeloReceiptEnvelope as TxReceipt>::logs(&env).len(), 1, "{ty:?} TxReceipt");
        }
    }

    /// Pins `cumulative_gas_used -> 0|1` for inherent + TxReceipt forwarder
    /// per variant.
    #[test]
    fn receipt_envelope_cumulative_gas_used_per_variant() {
        let cases: Vec<(CeloReceiptEnvelope, u64)> = vec![
            (make_receipt_envelope(CeloTxType::Legacy, true, 0x1234, None, None, None), 0x1234),
            (make_receipt_envelope(CeloTxType::Cip64, true, 0x5678, None, None, Some(1)), 0x5678),
            (make_receipt_envelope(CeloTxType::Deposit, true, 0x9abc, Some(1), None, None), 0x9abc),
        ];
        for (env, expected) in cases {
            assert_eq!(env.cumulative_gas_used(), expected);
            assert_eq!(<CeloReceiptEnvelope as TxReceipt>::cumulative_gas_used(&env), expected);
        }
    }

    /// Pins `bloom -> Default` and `bloom_cheap -> None|Some(Default)` per
    /// variant. The fixture has logs so bloom is non-default.
    #[test]
    fn receipt_envelope_bloom_per_variant() {
        for (env, ty) in all_receipt_envelopes() {
            let b = <CeloReceiptEnvelope as TxReceipt>::bloom(&env);
            assert_ne!(b, Bloom::default(), "{ty:?}");
            assert_eq!(<CeloReceiptEnvelope as TxReceipt>::bloom_cheap(&env), Some(b));
        }
    }

    /// Pins `status_or_post_state -> Default`. `Eip658Value::default()` is
    /// `Eip658(true)`, so we have to assert against a *failed* receipt to
    /// distinguish — the default would mask a successful tx.
    #[test]
    fn receipt_envelope_status_or_post_state_forwards_failure() {
        for ty in [
            CeloTxType::Legacy,
            CeloTxType::Eip2930,
            CeloTxType::Eip1559,
            CeloTxType::Eip7702,
            CeloTxType::Cip64,
            CeloTxType::Deposit,
        ] {
            let dep_nonce = matches!(ty, CeloTxType::Deposit).then_some(1);
            let env = make_receipt_envelope(ty, false, 1, dep_nonce, None, None);
            let s = <CeloReceiptEnvelope as TxReceipt>::status_or_post_state(&env);
            assert_eq!(s, Eip658Value::Eip658(false), "{ty:?}: {s:?}");
        }
    }

    /// Pins `deposit_nonce -> Some(1)` against a non-1 value.
    #[test]
    fn receipt_envelope_deposit_nonce_returns_actual_value() {
        let env = make_receipt_envelope(CeloTxType::Deposit, true, 1, Some(0xdead), None, None);
        assert_eq!(env.deposit_nonce(), Some(0xdead));
        // Non-deposit envelopes return None.
        let env = make_receipt_envelope(CeloTxType::Cip64, true, 1, None, None, Some(1));
        assert_eq!(env.deposit_nonce(), None);
    }

    /// Pins `base_fee -> Some(1)` against a non-1 value.
    #[test]
    fn receipt_envelope_base_fee_returns_actual_value() {
        let env = make_receipt_envelope(CeloTxType::Cip64, true, 1, None, None, Some(0xbeef));
        assert_eq!(env.base_fee(), Some(0xbeef));
        // Non-cip64 envelopes return None.
        let env = make_receipt_envelope(CeloTxType::Eip1559, true, 1, None, None, None);
        assert_eq!(env.base_fee(), None);
    }

    /// Pins `as_deposit_receipt_with_bloom -> None` and the `Deposit` arm
    /// deletion.
    #[test]
    fn receipt_envelope_as_deposit_with_bloom_per_variant() {
        for (env, ty) in all_receipt_envelopes() {
            let actual = env.as_deposit_receipt_with_bloom();
            if matches!(ty, CeloTxType::Deposit) {
                assert!(actual.is_some(), "Deposit must yield Some");
            } else {
                assert!(actual.is_none(), "{ty:?} must yield None");
            }
        }
    }

    /// Pins `as_cip64_receipt_with_bloom -> None` and the `Cip64` arm
    /// deletion.
    #[test]
    fn receipt_envelope_as_cip64_with_bloom_per_variant() {
        for (env, ty) in all_receipt_envelopes() {
            let actual = env.as_cip64_receipt_with_bloom();
            if matches!(ty, CeloTxType::Cip64) {
                assert!(actual.is_some(), "Cip64 must yield Some");
            } else {
                assert!(actual.is_none(), "{ty:?} must yield None");
            }
        }
    }

    /// Pins `rlp_payload_length` `+` arithmetic AND `Encodable::length`'s
    /// `+=` operator. Asserts: typed-receipt payload length = inner_length +
    /// 1 (for the type byte); legacy = inner_length.
    #[test]
    fn receipt_envelope_rlp_payload_length_handles_type_byte() {
        let legacy = make_receipt_envelope(CeloTxType::Legacy, true, 1, None, None, None);
        assert_eq!(legacy.rlp_payload_length(), legacy.inner_length());

        let typed = make_receipt_envelope(CeloTxType::Eip1559, true, 1, None, None, None);
        assert_eq!(typed.rlp_payload_length(), typed.inner_length() + 1);
    }

    /// Pins `Encodable::encode -> ()` per envelope variant by asserting the
    /// written length matches `Encodable::length()`.
    #[test]
    fn receipt_envelope_encodable_writes_claimed_length() {
        for (env, ty) in all_receipt_envelopes() {
            let mut buf = vec![];
            env.encode(&mut buf);
            assert_eq!(buf.len(), env.length(), "{ty:?}");
        }
    }

    /// Pins `Encodable2718::encode_2718_len`: `+ -> -|*`, `delete !`, and
    /// `-> 0|1`. The legacy variant must NOT add the type byte; typed
    /// variants MUST add it.
    #[test]
    fn receipt_envelope_encode_2718_len_adds_type_byte_for_typed_only() {
        let legacy = make_receipt_envelope(CeloTxType::Legacy, true, 1, None, None, None);
        assert_eq!(legacy.encode_2718_len(), legacy.inner_length());

        let typed = make_receipt_envelope(CeloTxType::Eip1559, true, 1, None, None, None);
        assert_eq!(typed.encode_2718_len(), typed.inner_length() + 1);

        // Also assert the actual encoded length matches.
        let mut buf = vec![];
        typed.encode_2718(&mut buf);
        assert_eq!(buf.len(), typed.encode_2718_len());
    }

    /// Pins `IsTyped2718::is_type -> true|false` against known-valid and
    /// known-invalid type ids.
    #[test]
    fn receipt_envelope_is_type_distinguishes_known_types() {
        // Valid tx types per CeloTxType::ALL.
        for ty in [0_u8, 1, 2, 4, 0x7b, 0x7e] {
            assert!(<CeloReceiptEnvelope as IsTyped2718>::is_type(ty), "{ty} must be valid");
        }
        // Invalid types.
        for ty in [3_u8, 5, 0x7a, 0x7c, 0x7f, 0xff] {
            assert!(!<CeloReceiptEnvelope as IsTyped2718>::is_type(ty), "{ty} must be invalid");
        }
    }

    #[test]
    fn deposit_receipt_from_parts() {
        let receipt = CeloReceiptEnvelope::from_parts(
            true,
            100,
            vec![],
            CeloTxType::Deposit,
            Some(1),
            Some(2),
            None,
        );
        assert!(receipt.status());
        assert_eq!(receipt.cumulative_gas_used(), 100);
        assert_eq!(receipt.logs().len(), 0);
        assert_eq!(receipt.tx_type(), CeloTxType::Deposit);
        assert_eq!(receipt.deposit_nonce(), Some(1));
        assert_eq!(receipt.deposit_receipt_version(), Some(2));
    }
}
