//! Celo transaction type additional implementations.

use crate::transaction::envelope::CeloTxType;

impl CeloTxType {
    /// List of all variants.
    pub const ALL: [Self; 6] =
        [Self::Legacy, Self::Eip2930, Self::Eip1559, Self::Eip7702, Self::Cip64, Self::Deposit];

    /// Returns `true` if the type is [`CeloTxType::Deposit`].
    pub const fn is_deposit(&self) -> bool {
        matches!(self, Self::Deposit)
    }
}

impl core::fmt::Display for CeloTxType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Legacy => write!(f, "legacy"),
            Self::Eip2930 => write!(f, "eip2930"),
            Self::Eip1559 => write!(f, "eip1559"),
            Self::Eip7702 => write!(f, "eip7702"),
            Self::Cip64 => write!(f, "cip64"),
            Self::Deposit => write!(f, "deposit"),
        }
    }
}

impl Default for CeloTxType {
    fn default() -> Self {
        Self::Legacy
    }
}
