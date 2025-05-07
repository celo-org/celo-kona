use alloy_consensus::TxType;

#[repr(u8)]
pub enum CeloTxType {
    NonCeloTx(TxType),
    Cip64 = 123,
}

impl From<CeloTxType> for u8 {
    fn from(tx_type: CeloTxType) -> Self {
        match tx_type {
            CeloTxType::NonCeloTx(tx_type) => tx_type.into(),
            CeloTxType::Cip64 => 123,
        }
    }
}
