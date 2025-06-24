//! Contains the hardfork configuration for the chain.

use alloc::string::{String, ToString};
use core::fmt::Display;
use kona_genesis::HardForkConfig;

/// Hardfork configuration.
///
/// See: <https://github.com/ethereum-optimism/superchain-registry/blob/8ff62ada16e14dd59d0fb94ffb47761c7fa96e01/ops/internal/config/chain.go#L102-L110>
#[derive(Debug, Copy, Clone, Default, Hash, Eq, PartialEq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct CeloHardForkConfig {
    /// The OP [HardForkConfig] for the chain.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub op_hardfork_config: HardForkConfig,
    /// `cel2_time` sets the activation time for the Celo L2 transition.
    /// Active if `cel2_time` != None && L2 block timestamp >= Some(cel2_time), inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub cel2_time: Option<u64>,
}

impl Display for CeloHardForkConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        #[inline(always)]
        fn fmt_time(t: Option<u64>) -> String {
            t.map(|t| t.to_string())
                .unwrap_or_else(|| "Not scheduled".to_string())
        }

        writeln!(f, "🍴 Scheduled Hardforks:")?;
        writeln!(
            f,
            "-> Regolith Activation Time: {}",
            fmt_time(self.op_hardfork_config.ecotone_time)
        )?;
        writeln!(
            f,
            "-> Canyon Activation Time: {}",
            fmt_time(self.op_hardfork_config.canyon_time)
        )?;
        writeln!(
            f,
            "-> Delta Activation Time: {}",
            fmt_time(self.op_hardfork_config.delta_time)
        )?;
        writeln!(
            f,
            "-> Ecotone Activation Time: {}",
            fmt_time(self.op_hardfork_config.ecotone_time)
        )?;
        writeln!(
            f,
            "-> Fjord Activation Time: {}",
            fmt_time(self.op_hardfork_config.fjord_time)
        )?;
        writeln!(
            f,
            "-> Granite Activation Time: {}",
            fmt_time(self.op_hardfork_config.granite_time)
        )?;
        writeln!(
            f,
            "-> Holocene Activation Time: {}",
            fmt_time(self.op_hardfork_config.holocene_time)
        )?;
        writeln!(
            f,
            "-> Pectra Blob Schedule Activation Time (Sepolia Superchain Only): {}",
            fmt_time(self.op_hardfork_config.pectra_blob_schedule_time)
        )?;
        writeln!(
            f,
            "-> Isthmus Activation Time: {}",
            fmt_time(self.op_hardfork_config.isthmus_time)
        )?;
        writeln!(
            f,
            "-> Interop Activation Time: {}",
            fmt_time(self.op_hardfork_config.interop_time)
        )
    }
}

#[cfg(test)]
#[cfg(feature = "serde")]
mod tests {
    use super::*;

    #[test]
    fn test_hardforks_deserialize_json() {
        let raw: &str = r#"
        {
            "canyon_time": 1699981200,
            "delta_time": 1703203200,
            "ecotone_time": 1708534800,
            "fjord_time": 1716998400,
            "granite_time": 1723478400,
            "holocene_time":1732633200,
            "cel2_time":1732633200
        }
        "#;

        let hardforks = CeloHardForkConfig {
            op_hardfork_config: HardForkConfig {
                regolith_time: None,
                canyon_time: Some(1699981200),
                delta_time: Some(1703203200),
                ecotone_time: Some(1708534800),
                fjord_time: Some(1716998400),
                granite_time: Some(1723478400),
                holocene_time: Some(1732633200),
                pectra_blob_schedule_time: None,
                isthmus_time: None,
                interop_time: None,
            },
            cel2_time: Some(1732633200),
        };

        let deserialized: CeloHardForkConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(hardforks, deserialized);
    }

    #[test]
    fn test_hardforks_deserialize_new_field_fail_json() {
        let raw: &str = r#"
        {
            "canyon_time": 1704992401,
            "delta_time": 1708560000,
            "ecotone_time": 1710374401,
            "fjord_time": 1720627201,
            "granite_time": 1726070401,
            "holocene_time": 1736445601,
            "new_field": 0
        }
        "#;

        let err = serde_json::from_str::<CeloHardForkConfig>(raw).unwrap_err();
        assert_eq!(err.classify(), serde_json::error::Category::Data);
    }

    #[test]
    fn test_hardforks_deserialize_toml() {
        let raw: &str = r#"
        canyon_time =  1699981200 # Tue 14 Nov 2023 17:00:00 UTC
        delta_time =   1703203200 # Fri 22 Dec 2023 00:00:00 UTC
        ecotone_time = 1708534800 # Wed 21 Feb 2024 17:00:00 UTC
        fjord_time =   1716998400 # Wed 29 May 2024 16:00:00 UTC
        granite_time = 1723478400 # Mon Aug 12 16:00:00 UTC 2024
        holocene_time = 1732633200 # Tue Nov 26 15:00:00 UTC 2024
        cel2_time = 1732633200 # Tue Nov 26 15:00:00 UTC 2024
        "#;

        let hardforks = CeloHardForkConfig {
            op_hardfork_config: HardForkConfig {
                regolith_time: None,
                canyon_time: Some(1699981200),
                delta_time: Some(1703203200),
                ecotone_time: Some(1708534800),
                fjord_time: Some(1716998400),
                granite_time: Some(1723478400),
                holocene_time: Some(1732633200),
                pectra_blob_schedule_time: None,
                isthmus_time: None,
                interop_time: None,
            },
            cel2_time: Some(1732633200),
        };

        let deserialized: CeloHardForkConfig = toml::from_str(raw).unwrap();
        assert_eq!(hardforks, deserialized);
    }

    #[test]
    fn test_hardforks_deserialize_new_field_fail_toml() {
        let raw: &str = r#"
        canyon_time =  1699981200 # Tue 14 Nov 2023 17:00:00 UTC
        delta_time =   1703203200 # Fri 22 Dec 2023 00:00:00 UTC
        ecotone_time = 1708534800 # Wed 21 Feb 2024 17:00:00 UTC
        fjord_time =   1716998400 # Wed 29 May 2024 16:00:00 UTC
        granite_time = 1723478400 # Mon Aug 12 16:00:00 UTC 2024
        holocene_time = 1732633200 # Tue Nov 26 15:00:00 UTC 2024
        new_field_time = 1732633200 # Tue Nov 26 15:00:00 UTC 2024
        "#;
        toml::from_str::<CeloHardForkConfig>(raw).unwrap_err();
    }
}
