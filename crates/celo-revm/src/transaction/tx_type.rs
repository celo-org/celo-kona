use alloy_eips::eip2718::{Eip2718Error, IsTyped2718};
use op_alloy_consensus::OpTxType;

#[repr(u8)]
pub enum CeloTxType {
    NonCeloTx(OpTxType),
    Cip64 = 123,
}

impl From<CeloTxType> for u8 {
    fn from(tx_type: CeloTxType) -> Self {
        match tx_type {
            CeloTxType::NonCeloTx(op_tx_type) => op_tx_type.into(),
            CeloTxType::Cip64 => tx_type.into(),
        }
    }
}

impl TryFrom<u8> for CeloTxType {
    type Error = Eip2718Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            0 | 1 | 2 | 4 | 126 => Self::NonCeloTx(OpTxType::try_from(value)?),
            123 => Self::Cip64,
            _ => return Err(Eip2718Error::UnexpectedType(value)),
        })
    }
}

impl IsTyped2718 for CeloTxType {
    fn is_type(type_id: u8) -> bool {
        // legacy | eip2930 | eip1559 | eip7702 | cip64 | deposit
        matches!(type_id, 0 | 1 | 2 | 4 | 123 | 126)
    }
}
