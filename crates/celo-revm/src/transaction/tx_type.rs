//! Contains the transaction type identifier for Celo.

use alloy_consensus::Typed2718;
use alloy_eips::eip2718::{Eip2718Error, IsTyped2718};
use alloy_primitives::{U8, U64};
use alloy_rlp::{BufMut, Decodable, Encodable};
use derive_more::Display;

/// Identifier for an Celo CIP-64 transaction
pub const CIP64_TRANSACTION_TYPE: u8 = 0x7b;

/// Celo TransactionType flags as specified in EIPs 2718, 1559, 2930, and CIP 64 as well as the deposit transaction spec
#[repr(u8)]
#[derive(Debug, Copy, Clone, Eq, Default, PartialEq, PartialOrd, Ord, Hash, Display)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(into = "U8", try_from = "U64"))]
pub enum CeloTxType {
    /// Legacy transaction type.
    #[default]
    #[display("legacy")]
    Legacy = 0,
    /// EIP-2930 transaction type.
    #[display("eip2930")]
    Eip2930 = 1,
    /// EIP-1559 transaction type.
    #[display("eip1559")]
    Eip1559 = 2,
    /// EIP-7702 transaction type.
    #[display("eip7702")]
    Eip7702 = 4,
    /// CIP-64 transaction type.
    #[display("cip64")]
    Cip64 = 123,
    /// Optimism Deposit transaction type.
    #[display("deposit")]
    Deposit = 126,
}

impl CeloTxType {
    /// List of all variants.
    pub const ALL: [Self; 6] = [
        Self::Legacy,
        Self::Eip2930,
        Self::Eip1559,
        Self::Eip7702,
        Self::Cip64,
        Self::Deposit,
    ];

    /// Returns `true` if the type is [`CeloTxType::Deposit`].
    pub const fn is_deposit(&self) -> bool {
        matches!(self, Self::Deposit)
    }
}

#[cfg(feature = "arbitrary")]
impl arbitrary::Arbitrary<'_> for CeloTxType {
    fn arbitrary(u: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<Self> {
        let i = u.choose_index(Self::ALL.len())?;
        Ok(Self::ALL[i])
    }
}

impl From<CeloTxType> for U8 {
    fn from(tx_type: CeloTxType) -> Self {
        Self::from(u8::from(tx_type))
    }
}

impl From<CeloTxType> for u8 {
    fn from(v: CeloTxType) -> Self {
        v as Self
    }
}

impl TryFrom<u8> for CeloTxType {
    type Error = Eip2718Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::Legacy,
            1 => Self::Eip2930,
            2 => Self::Eip1559,
            4 => Self::Eip7702,
            123 => Self::Cip64,
            126 => Self::Deposit,
            _ => return Err(Eip2718Error::UnexpectedType(value)),
        })
    }
}

impl TryFrom<u64> for CeloTxType {
    type Error = &'static str;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        let err = || "invalid tx type";
        let value: u8 = value.try_into().map_err(|_| err())?;
        Self::try_from(value).map_err(|_| err())
    }
}

impl TryFrom<U64> for CeloTxType {
    type Error = &'static str;

    fn try_from(value: U64) -> Result<Self, Self::Error> {
        value.to::<u64>().try_into()
    }
}

impl PartialEq<u8> for CeloTxType {
    fn eq(&self, other: &u8) -> bool {
        (*self as u8) == *other
    }
}

impl PartialEq<CeloTxType> for u8 {
    fn eq(&self, other: &CeloTxType) -> bool {
        *self == *other as Self
    }
}

impl Encodable for CeloTxType {
    fn encode(&self, out: &mut dyn BufMut) {
        (*self as u8).encode(out);
    }

    fn length(&self) -> usize {
        1
    }
}

impl Decodable for CeloTxType {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let ty = u8::decode(buf)?;

        Self::try_from(ty).map_err(|_| alloy_rlp::Error::Custom("invalid transaction type"))
    }
}

impl Typed2718 for CeloTxType {
    fn ty(&self) -> u8 {
        (*self).into()
    }
}

impl IsTyped2718 for CeloTxType {
    fn is_type(type_id: u8) -> bool {
        // legacy | eip2930 | eip1559 | eip7702 | cip64 | deposit
        matches!(type_id, 0 | 1 | 2 | 4 | 123 | 126)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{vec, vec::Vec};

    #[test]
    fn test_all_tx_types() {
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
    fn tx_type_roundtrip() {
        for &tx_type in &CeloTxType::ALL {
            let mut buf = Vec::new();
            tx_type.encode(&mut buf);
            let decoded = CeloTxType::decode(&mut &buf[..]).unwrap();
            assert_eq!(tx_type, decoded);
        }
    }
}
