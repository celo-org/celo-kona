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

#[cfg(test)]
mod tests {
    use super::*;
    use std::format;

    /// Pins `Display::fmt -> Ok(Default)` (which would not write any bytes).
    /// Each variant must produce its specific lowercase name.
    #[test]
    fn display_writes_variant_specific_string() {
        assert_eq!(format!("{}", CeloTxType::Legacy), "legacy");
        assert_eq!(format!("{}", CeloTxType::Eip2930), "eip2930");
        assert_eq!(format!("{}", CeloTxType::Eip1559), "eip1559");
        assert_eq!(format!("{}", CeloTxType::Eip7702), "eip7702");
        assert_eq!(format!("{}", CeloTxType::Cip64), "cip64");
        assert_eq!(format!("{}", CeloTxType::Deposit), "deposit");
    }
}
