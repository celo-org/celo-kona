use alloy_hardforks::Hardfork;

/// Historical Celo ForkID identifiers. CGTv2 has no timestamp entry.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum CeloHardfork {
    /// Pre-Bedrock Celo L1 EVM upgrade.
    Gingerbread,
    /// Celo L2 cutover.
    Cel2,
}
impl Hardfork for CeloHardfork {
    fn name(&self) -> &'static str {
        match self {
            Self::Gingerbread => "Gingerbread",
            Self::Cel2 => "Cel2",
        }
    }
}
