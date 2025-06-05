//! Contains the transaction type identifier for Celo.

use alloy_consensus::{Typed2718};
use op_alloy_consensus::transaction::OpTxType;
use alloy_eips::eip2718::{Eip2718Error, IsTyped2718};
use alloy_primitives::{U8, U64};
use alloy_rlp::{BufMut, Decodable, Encodable};
use derive_more::Display;

/// Celo TransactionType flags as specified in EIPs 2718, 1559, 2930, and CIP 64 as well as the deposit transaction spec
#[repr(u8)]
#[derive(Debug, Copy, Clone, Eq, Default, PartialEq, PartialOrd, Ord, Hash, Display)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(into = "U8", try_from = "U64"))]
pub enum CeloTxTypeAlt {
    /// Original OP types
    OP(OpTxType),
    /// CIP-64 transaction type.
    #[default]
    #[display("cip64")]
    Cip64 = 123,
}


impl CeloTxTypeAlt {
    /// List of all variants.
    pub const ALL: [Self; 7] = [
        Self::OP(OpTxType::Legacy),
        Self::OP(OpTxType::Eip2930),
        Self::OP(OpTxType::Eip1559),
        Self::OP(OpTxType::Eip2930),
        Self::OP(OpTxType::Eip7702),
        Self::OP(OpTxType::Deposit),
        Self::Cip64,
    ];

    /// Returns `true` if the type is [`CeloTxTypeAlt::Deposit`].
    pub const fn is_deposit(&self) -> bool {
        matches!(self, Self::OP(OpTxType::Deposit))
    }
}

#[cfg(feature = "arbitrary")]
impl arbitrary::Arbitrary<'_> for CeloTxTypeAlt {
    fn arbitrary(u: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<Self> {
        let i = u.choose_index(Self::ALL.len())?;
        Ok(Self::ALL[i])
    }
}

impl From<CeloTxTypeAlt> for U8 {
    fn from(tx_type: CeloTxTypeAlt) -> Self {
        Self::from(u8::from(tx_type))
    }
}

impl From<CeloTxTypeAlt> for u8 {
    fn from(v: CeloTxTypeAlt) -> Self {
        match v {
            CeloTxTypeAlt::Cip64 => 123,
            CeloTxTypeAlt::OP(tx_type) => tx_type as Self,
        }
    }
}

impl TryFrom<u8> for CeloTxTypeAlt {
    type Error = Eip2718Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            123 => Self::Cip64,
            a => Self::OP(OpTxType::try_from(a)?),
        })
    }
}


impl TryFrom<u64> for CeloTxTypeAlt {
    type Error = &'static str;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        let err = || "invalid tx type";
        let value: u8 = value.try_into().map_err(|_| err())?;
        Self::try_from(value).map_err(|_| err())
    }
}

impl TryFrom<U64> for CeloTxTypeAlt {
    type Error = &'static str;

    fn try_from(value: U64) -> Result<Self, Self::Error> {
        value.to::<u64>().try_into()
    }
}


impl PartialEq<u8> for CeloTxTypeAlt {
    fn eq(&self, other: &u8) -> bool {
        match self {
            CeloTxTypeAlt::Cip64 => *other == 123,
            CeloTxTypeAlt::OP(tx_type) => (*tx_type as u8) == *other,
        }
    }
}

impl PartialEq<CeloTxTypeAlt> for u8 {
    fn eq(&self, other: &CeloTxTypeAlt) -> bool {
        match other {
            CeloTxTypeAlt::Cip64 => *self as u8 == 123,
            CeloTxTypeAlt::OP(tx_type) => (*tx_type as Self) == *self,
        }
    }
}

impl Encodable for CeloTxTypeAlt {
    fn encode(&self, out: &mut dyn BufMut) {
        match self {
            CeloTxTypeAlt::OP(s) => (*s as u8).encode(out),
            CeloTxTypeAlt::Cip64 => (123 as u8).encode(out),
        }
        
    }

    fn length(&self) -> usize {
        1
    }
}

impl Decodable for CeloTxTypeAlt {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let ty = u8::decode(buf)?;

        Self::try_from(ty).map_err(|_| alloy_rlp::Error::Custom("invalid transaction type"))
    }
}

impl Typed2718 for CeloTxTypeAlt {
    fn ty(&self) -> u8 {
        (*self).into()
    }
}

impl IsTyped2718 for CeloTxTypeAlt {
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
        assert_eq!(CeloTxTypeAlt::ALL.len(), 7);
        let all = vec![
            CeloTxTypeAlt::OP(OpTxType::Legacy),
            CeloTxTypeAlt::OP(OpTxType::Eip2930),
            CeloTxTypeAlt::OP(OpTxType::Eip1559),
            CeloTxTypeAlt::OP(OpTxType::Eip2930),
            CeloTxTypeAlt::OP(OpTxType::Eip7702),
            CeloTxTypeAlt::OP(OpTxType::Deposit),
            CeloTxTypeAlt::Cip64,
        ];
        assert_eq!(CeloTxTypeAlt::ALL.to_vec(), all);
    }

    #[test]
    fn tx_type_roundtrip() {
        for &tx_type in &CeloTxTypeAlt::ALL {
            let mut buf = Vec::new();
            tx_type.encode(&mut buf);
            let decoded = CeloTxTypeAlt::decode(&mut &buf[..]).unwrap();
            assert_eq!(tx_type, decoded);
        }
    }
}