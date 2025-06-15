//! A Hint definition for Celo

use alloc::{str::FromStr, string::String, vec::Vec};
use alloy_primitives::hex;
use core::fmt::Display;
use kona_proof::{HintType, errors::HintParsingError};

/// The [CeloHintType] extends kona's [HintType] enum with additional variants.
/// for EigenDA integration
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CeloHintType {
    /// Original hint type from kona's [HintType] enum.
    Original(HintType),
    /// EigenDA certificate hint type.
    EigenDACert,
}

impl CeloHintType {
    /// The string identifier for EigenDA certificate hint type.
    pub const EIGENDA_CERT: &'static str = "eigenda-certificate";

    /// Encodes the hint type with the provided data into a hex-encoded string.
    pub fn encode_with(&self, data: &[&[u8]]) -> String {
        let concatenated = hex::encode(data.iter().copied().flatten().copied().collect::<Vec<_>>());
        alloc::format!("{} {}", self, concatenated)
    }
}

impl FromStr for CeloHintType {
    type Err = HintParsingError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            Self::EIGENDA_CERT => Ok(Self::EigenDACert),
            _ => Ok(Self::Original(HintType::from_str(value)?)),
        }
    }
}

impl From<CeloHintType> for &str {
    fn from(value: CeloHintType) -> Self {
        match value {
            CeloHintType::EigenDACert => CeloHintType::EIGENDA_CERT,
            CeloHintType::Original(ht) => ht.into(),
        }
    }
}

impl Display for CeloHintType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s: &str = (*self).into();
        write!(f, "{}", s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kona_proof::HintType;

    #[test]
    fn test_celo_hint_type_encoding() {
        let test_cases = alloc::vec![
            (
                CeloHintType::EigenDACert,
                &[b"test_data" as &[u8]],
                "eigenda-certificate 746573745f64617461"
            ),
            (
                CeloHintType::Original(HintType::L1BlockHeader),
                &[b"test_data" as &[u8]],
                "l1-block-header 746573745f64617461"
            ),
        ];

        for (hint_type, data, expected) in test_cases {
            let encoded = hint_type.encode_with(data);
            assert_eq!(encoded, expected, "Failed to encode {:?}", hint_type);
        }
    }

    #[test]
    fn test_celo_hint_type_decoding_valid_data() {
        let test_cases = alloc::vec![
            ("eigenda-certificate", CeloHintType::EigenDACert),
            (
                "l1-block-header",
                CeloHintType::Original(HintType::L1BlockHeader)
            ),
        ];

        for (input, expected) in test_cases {
            let result = CeloHintType::from_str(input).expect("Failed to decode valid hint type");
            assert_eq!(result, expected, "Failed to decode '{}'", input);
        }
    }

    #[test]
    fn test_celo_hint_type_decoding_invalid_data() {
        let invalid_result = CeloHintType::from_str("invalid-hint-type");
        assert!(
            invalid_result.is_err(),
            "Expected error for invalid hint type"
        );
    }

    #[test]
    fn test_celo_hint_type_conversion() {
        let test_cases = alloc::vec![
            (CeloHintType::EigenDACert, "eigenda-certificate"),
            (
                CeloHintType::Original(HintType::L1BlockHeader),
                "l1-block-header"
            ),
        ];

        for (hint_type, expected) in test_cases {
            let str_value: &str = hint_type.into();
            assert_eq!(str_value, expected, "Failed to convert {:?}", hint_type);
        }
    }
}
