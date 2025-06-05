//! Receipt envelope types for Celo.

use alloy_consensus::{Eip658Value, Receipt, ReceiptWithBloom, TxReceipt};
use alloy_eips::{
    Typed2718,
    eip2718::{Decodable2718, Eip2718Error, Eip2718Result, Encodable2718, IsTyped2718},
};
use alloy_primitives::{Bloom, Log, logs_bloom};
use alloy_rlp::{BufMut, Decodable, Encodable, length_of_length};
use op_alloy_consensus::{OpDepositReceipt, OpDepositReceiptWithBloom, OpReceiptEnvelope};
use std::vec::Vec;
use crate::CeloTxTypeAlt;

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
/// 

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "type"))]
pub enum CeloReceiptEnvelopeAlt<T = Log> {
    /// OP envelope
    OP(OpReceiptEnvelope<T>),
    /// Receipt envelope with type flag 123, containing a [CIP-64] receipt.
    ///
    /// [CIP-64]: https://github.com/celo-org/celo-proposals/blob/master/CIPs/cip-0064.md
    #[cfg_attr(feature = "serde", serde(rename = "0x7b", alias = "0x7B"))]
    Cip64(ReceiptWithBloom<Receipt<T>>), // TODO: replace with CeloCip64Receipt which includes baseFee
}

impl CeloReceiptEnvelopeAlt<Log> {
    /// Creates a new [`CeloReceiptEnvelopeAlt`] from the given parts.
    pub fn from_parts<'a>(
        status: bool,
        cumulative_gas_used: u64,
        logs: impl IntoIterator<Item = &'a Log>,
        tx_type: CeloTxTypeAlt,
        deposit_nonce: Option<u64>,
        deposit_receipt_version: Option<u64>,
    ) -> Self {
        match tx_type {
            CeloTxTypeAlt::Cip64 => {
                let logs = logs.into_iter().cloned().collect::<Vec<_>>();
                let logs_bloom = logs_bloom(&logs);
                let inner_receipt = Receipt {
                    status: Eip658Value::Eip658(status),
                    cumulative_gas_used,
                    logs,
                };
                
                Self::Cip64(ReceiptWithBloom {
                    receipt: inner_receipt,
                    logs_bloom,
                })
            },
            CeloTxTypeAlt::OP(op_tx_type) => {
                Self::OP(OpReceiptEnvelope::from_parts(status, cumulative_gas_used, logs, op_tx_type, deposit_nonce, deposit_receipt_version))
            }
        }
    }
}



impl<T> CeloReceiptEnvelopeAlt<T> {
    /// Return the [`CeloTxTypeAlt`] of the inner receipt.
    pub const fn tx_type(&self) -> CeloTxTypeAlt {
        match self {
            Self::Cip64(_) => CeloTxTypeAlt::Cip64,
            Self::OP(t) => CeloTxTypeAlt::OP(t.tx_type()),
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
    pub fn logs(&self) -> &[T] {
        &self.as_receipt().unwrap().logs
    }

    /// Return the receipt's bloom.
    pub const fn logs_bloom(&self) -> &Bloom {
        match self {
            Self::Cip64(t) => &t.logs_bloom,
            Self::OP(t) => t.logs_bloom(),
        }
    }

    /// Return the receipt's deposit_nonce if it is a deposit receipt.
    pub fn deposit_nonce(&self) -> Option<u64> {
        self.as_deposit_receipt().and_then(|r| r.deposit_nonce)
    }

    /// Return the receipt's deposit version if it is a deposit receipt.
    pub fn deposit_receipt_version(&self) -> Option<u64> {
        self.as_deposit_receipt()
            .and_then(|r| r.deposit_receipt_version)
    }

    /// Returns the deposit receipt if it is a deposit receipt.
    pub const fn as_deposit_receipt_with_bloom(&self) -> Option<&OpDepositReceiptWithBloom<T>> {
        match self {
            Self::OP(OpReceiptEnvelope::Deposit(t)) => Some(t),
            _ => None,
        }
    }

    /// Returns the deposit receipt if it is a deposit receipt.
    pub const fn as_deposit_receipt(&self) -> Option<&OpDepositReceipt<T>> {
        match self {
            Self::OP(OpReceiptEnvelope::Deposit(t)) => Some(&t.receipt),
            _ => None,
        }
    }

    /// Return the inner receipt. Currently this is infallible, however, future
    /// receipt types may be added.
    pub const fn as_receipt(&self) -> Option<&Receipt<T>> {
        match self {
            Self::Cip64(t) => Some(&t.receipt),
            Self::OP(t) => t.as_receipt(),
        }
    }
}


impl CeloReceiptEnvelopeAlt {
    /// Get the length of the inner receipt in the 2718 encoding.
    pub fn inner_length(&self) -> usize {
        match self {
            Self::Cip64(t) => t.length(),
            Self::OP(t) => t.length(),
        }
    }

    /// Calculate the length of the rlp payload of the network encoded receipt.
    pub fn rlp_payload_length(&self) -> usize {
        let length = self.inner_length();
        match self {
            Self::OP(OpReceiptEnvelope::Legacy(_)) => length,
            _ => length + 1,
        }
    }
}

impl<T> TxReceipt for CeloReceiptEnvelopeAlt<T>
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

impl Encodable for CeloReceiptEnvelopeAlt {
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

impl Decodable for CeloReceiptEnvelopeAlt {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Self::network_decode(buf)
            .map_or_else(|_| Err(alloy_rlp::Error::Custom("Unexpected type")), Ok)
    }
}

impl Typed2718 for CeloReceiptEnvelopeAlt {
    fn ty(&self) -> u8 {
        match self {
            Self::Cip64(_) => CeloTxTypeAlt::Cip64.into(),
            Self::OP(t) => t.ty(),
        }
    }
}

impl IsTyped2718 for CeloReceiptEnvelopeAlt {
    fn is_type(type_id: u8) -> bool {
        <CeloTxTypeAlt as IsTyped2718>::is_type(type_id)
    }
}

impl Encodable2718 for CeloReceiptEnvelopeAlt {
    fn encode_2718_len(&self) -> usize {
        self.inner_length() + !self.is_legacy() as usize
    }

    fn encode_2718(&self, out: &mut dyn BufMut) {
        match self.type_flag() {
            None => {}
            Some(ty) => out.put_u8(ty),
        }
        match self {
            Self::Cip64(t) => t.encode(out),
            Self::OP(t) => t.encode(out),
        }
    }
}

impl Decodable2718 for CeloReceiptEnvelopeAlt {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        match ty
            .try_into()
            .map_err(|_| Eip2718Error::UnexpectedType(ty))?
        {
            CeloTxTypeAlt::Cip64  => Ok(Self::Cip64(Decodable::decode(buf)?)),
            CeloTxTypeAlt::OP(_) => Ok(Self::OP(OpReceiptEnvelope::typed_decode(ty, buf)?)),
        }
    }

    fn fallback_decode(buf: &mut &[u8]) -> Eip2718Result<Self> {
        Ok(Self::OP(OpReceiptEnvelope::Legacy(Decodable::decode(buf)?)))
    }
}

#[cfg(all(test, feature = "arbitrary"))]
impl<'a, T> arbitrary::Arbitrary<'a> for CeloReceiptEnvelopeAlt<T>
where
    T: arbitrary::Arbitrary<'a>,
{
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        match u.int_in_range(0..=4)? {
            0 => Ok(Self::Legacy(ReceiptWithBloom::arbitrary(u)?)),
            1 => Ok(Self::Eip2930(ReceiptWithBloom::arbitrary(u)?)),
            2 => Ok(Self::Eip1559(ReceiptWithBloom::arbitrary(u)?)),
            4 => Ok(Self::Eip7702(ReceiptWithBloom::arbitrary(u)?)),
            _ => Ok(Self::Deposit(OpDepositReceiptWithBloom::arbitrary(u)?)),
        }
    }
}

/*
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
        let receipt = CeloReceiptEnvelopeAlt::Legacy(ReceiptWithBloom {
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
        let receipt =
            CeloReceiptEnvelopeAlt::from_parts(true, 100, vec![], CeloTxTypeAlt::Legacy, None, None);
        assert!(receipt.status());
        assert_eq!(receipt.cumulative_gas_used(), 100);
        assert_eq!(receipt.logs().len(), 0);
        assert_eq!(receipt.tx_type(), CeloTxTypeAlt::Legacy);
    }

    #[test]
    fn cip64_receipt_from_parts() {
        let receipt =
            CeloReceiptEnvelopeAlt::from_parts(true, 100, vec![], CeloTxTypeAlt::Cip64, None, None);
        assert!(receipt.status());
        assert_eq!(receipt.cumulative_gas_used(), 100);
        assert_eq!(receipt.logs().len(), 0);
        assert_eq!(receipt.tx_type(), CeloTxTypeAlt::Cip64);
    }

    #[test]
    fn deposit_receipt_from_parts() {
        let receipt = CeloReceiptEnvelopeAlt::from_parts(
            true,
            100,
            vec![],
            CeloTxTypeAlt::Deposit,
            Some(1),
            Some(2),
        );
        assert!(receipt.status());
        assert_eq!(receipt.cumulative_gas_used(), 100);
        assert_eq!(receipt.logs().len(), 0);
        assert_eq!(receipt.tx_type(), CeloTxTypeAlt::Deposit);
        assert_eq!(receipt.deposit_nonce(), Some(1));
        assert_eq!(receipt.deposit_receipt_version(), Some(2));
    }
} */