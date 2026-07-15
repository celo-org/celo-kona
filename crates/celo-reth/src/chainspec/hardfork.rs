use alloy_celo_evm::block::Upgrade18Overrides;
use alloy_hardforks::Hardfork;
use alloy_primitives::{Address, U256};
use reth_chainspec::{EthChainSpec, Hardforks};

/// Celo-specific hardfork variants not part of the OP Stack hardfork set.
///
/// All variants shift the ForkID hash op-geth peers expect:
/// * `Gingerbread` — pre-bedrock Celo L1 EVM upgrade at block `21_616_000` on mainnet.
/// * `Cel2` — L2 cutover at the timestamp captured in the genesis (mainnet `1_742_957_258`).
/// * `Upgrade18` — CGT v2 migration (provisional name, see below).
///
/// They contribute to ForkID via [`reth_chainspec::ChainHardforks`]. `Gingerbread` and
/// `Cel2` have no runtime EVM activation logic; `Upgrade18`'s timestamp additionally
/// gates the CGT v2 predeploy irregular state transition — it is extracted with
/// [`upgrade18_time`] and handed to `alloy-celo-evm`'s `CeloBlockExecutorFactory`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum CeloHardfork {
    /// Pre-bedrock Celo L1 EVM upgrade.
    Gingerbread,
    /// Celo L2 cutover.
    Cel2,
    /// OP Upgrade 18: the CGT v1 → v2 migration (predeploy install + reserve seed via
    /// irregular state transition). The name is provisional — the final activation
    /// trigger is an open decision (celo-blockchain-planning#1407) — but must stay keyed
    /// identically to `CeloRollupConfig::upgrade18_time` on the celo-kona side.
    Upgrade18,
}

impl Hardfork for CeloHardfork {
    fn name(&self) -> &'static str {
        match self {
            Self::Gingerbread => "Gingerbread",
            Self::Cel2 => "Cel2",
            Self::Upgrade18 => "Upgrade18",
        }
    }
}

/// Extracts the provisional Upgrade 18 (CGT v2) activation timestamp from a chain spec's
/// hardfork list, or `None` when the fork is not scheduled.
pub fn upgrade18_time(spec: &impl Hardforks) -> Option<u64> {
    spec.fork(CeloHardfork::Upgrade18).as_timestamp()
}

/// Reads Upgrade 18 activation-artifact param overrides from the genesis `config`
/// extra fields:
///
/// - `upgrade18LiquidityControllerOwner`, `upgrade18CeloTokenL1`, `upgrade18CeloGasBridgeL1` —
///   `0x…` addresses;
/// - `upgrade18NativeAssetLiquidityAmount` — a number, `0x…` quantity, or decimal string (wei).
///
/// Overrides beat the artifact's per-network constants; on chains the artifact doesn't
/// know (dev/e2e) all four are required once the fork is scheduled. Absent fields stay
/// `None`.
///
/// # Panics
///
/// If a key is present but unparseable. Treating it as absent instead would silently
/// fall back to the artifact's per-network constant (or to an `Upgrade18ParamMissing`
/// halt blaming the wrong cause) — an operator-supplied value must either apply or
/// stop the node at startup.
pub fn upgrade18_overrides(spec: &impl EthChainSpec) -> Upgrade18Overrides {
    let fields = &spec.genesis().config.extra_fields;
    let address = |key: &str| -> Option<Address> {
        fields.get_deserialized::<Address>(key).map(|parsed| {
            parsed.unwrap_or_else(|e| {
                panic!("genesis config `{key}` is not a valid `0x…` address: {e}")
            })
        })
    };
    let amount = |key: &str| -> Option<U256> {
        let value = fields.get(key)?;
        let parsed =
            value.as_u64().map(U256::from).or_else(|| value.as_str()?.parse::<U256>().ok());
        Some(parsed.unwrap_or_else(|| {
            panic!(
                "genesis config `{key}` must be a u64 number, `0x…` quantity, or decimal \
                 string (wei), got: {value}"
            )
        }))
    };
    Upgrade18Overrides {
        liquidity_controller_owner: address("upgrade18LiquidityControllerOwner"),
        celo_token_l1: address("upgrade18CeloTokenL1"),
        celo_gas_bridge_l1: address("upgrade18CeloGasBridgeL1"),
        native_asset_liquidity_amount: amount("upgrade18NativeAssetLiquidityAmount"),
    }
}
