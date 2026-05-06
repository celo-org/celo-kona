use alloy_hardforks::Hardfork;

/// Celo-specific hardfork variants not part of the OP Stack hardfork set.
///
/// Both forks shift the ForkID hash op-geth peers expect:
/// * `Gingerbread` — pre-bedrock Celo L1 EVM upgrade at block `21_616_000` on mainnet.
/// * `Cel2` — L2 cutover at the timestamp captured in the genesis (mainnet `1_742_957_258`).
///
/// They contribute to ForkID via [`reth_chainspec::ChainHardforks`]; runtime EVM
/// activation logic does not branch on them yet.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum CeloHardfork {
    /// Pre-bedrock Celo L1 EVM upgrade.
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
