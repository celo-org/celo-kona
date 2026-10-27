//! Receipt-triggered CGTv2 upgrade deposits. Only the synthetic bundle is configured.

use alloy_eips::Decodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address, b256, keccak256, uint};
use op_alloy_consensus::{OpTransaction, OpTxEnvelope, TxDeposit, UpgradeDepositSource};
use std::vec::Vec;

/// Celo mainnet execution chain ID.
pub const CELO_MAINNET_CHAIN_ID: u64 = 42220;
/// Celo Sepolia execution chain ID.
pub const CELO_SEPOLIA_CHAIN_ID: u64 = 11142220;
/// Celo Chaos execution chain ID.
pub const CELO_CHAOS_CHAIN_ID: u64 = 11162320;
/// Synthetic profile matching the frozen contract/NUT fixtures.
pub const CGT_V2_FIXTURE_CHAIN_ID: u64 = 1337;

/// CeloTokenL1.TOTAL_MARKET_CAP: 1 billion CELO, with 18 decimals.
/// Source: celo-contracts/v6.0.0 at 0296412d673a666af6236efa1f615032b1f1c424.
pub const CGT_V2_TOTAL_SUPPLY: U256 = uint!(1_000_000_000_000_000_000_000_000_000_U256);

// TODO(CGTv2): replace for each network after its L1 bridge deployment.
// This temporary address is not a deployed production bridge.
const TEMPORARY_L1_BRIDGE: Address = address!("00000000000000000000000000000000c6700001");

/// Fixed migration parameters resolved only from the execution chain ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CgtV2NetworkParams {
    /// Fixed L1 bridge proxy that emits the authenticated migration event.
    pub celo_gas_bridge_l1: Address,
    total_supply: U256,
}

impl CgtV2NetworkParams {
    /// Checked reserve increment. The supply cannot be supplied by the signal or an operator.
    pub const fn reserve_seed(&self, migrated_amount: U256) -> Option<U256> {
        self.total_supply.checked_sub(migrated_amount)
    }
}

struct CgtV2NetworkEntry {
    chain_id: u64,
    params: CgtV2NetworkParams,
}

const NETWORKS: [CgtV2NetworkEntry; 3] = [
    CgtV2NetworkEntry {
        chain_id: CELO_MAINNET_CHAIN_ID,
        params: CgtV2NetworkParams {
            celo_gas_bridge_l1: TEMPORARY_L1_BRIDGE,
            total_supply: CGT_V2_TOTAL_SUPPLY,
        },
    },
    CgtV2NetworkEntry {
        chain_id: CELO_SEPOLIA_CHAIN_ID,
        params: CgtV2NetworkParams {
            celo_gas_bridge_l1: TEMPORARY_L1_BRIDGE,
            total_supply: CGT_V2_TOTAL_SUPPLY,
        },
    },
    CgtV2NetworkEntry {
        chain_id: CELO_CHAOS_CHAIN_ID,
        params: CgtV2NetworkParams {
            celo_gas_bridge_l1: TEMPORARY_L1_BRIDGE,
            total_supply: CGT_V2_TOTAL_SUPPLY,
        },
    },
];

// Fixture inputs must never become production defaults or select production bundles.
const FIXTURE_PARAMS: CgtV2NetworkParams = CgtV2NetworkParams {
    celo_gas_bridge_l1: Address::with_last_byte(0xcc),
    total_supply: uint!(1000_U256),
};

/// The network or its deployment-specific upgrade bundle has not been configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CgtV2ConfigError {
    /// The execution chain whose configuration is unavailable.
    pub chain_id: u64,
}
impl core::fmt::Display for CgtV2ConfigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "CGTv2 network or upgrade bundle is not configured for chain {}", self.chain_id)
    }
}
impl core::error::Error for CgtV2ConfigError {}

/// Resolve the fixed network table, with chain 1337 as a separate synthetic profile.
pub fn cgt_v2_params(chain_id: u64) -> Result<CgtV2NetworkParams, CgtV2ConfigError> {
    if chain_id == CGT_V2_FIXTURE_CHAIN_ID {
        return Ok(FIXTURE_PARAMS);
    }
    NETWORKS
        .iter()
        .find_map(|network| (network.chain_id == chain_id).then_some(network.params))
        .ok_or(CgtV2ConfigError { chain_id })
}

/// Number of static NUT transactions, followed by one dynamic funding deposit.
pub const CGT_V2_STATIC_STEPS: usize = 19;
/// Number of required transactions in the complete group.
pub const CGT_V2_STEPS: usize = CGT_V2_STATIC_STEPS + 1;
/// Gas limit for the dynamic reserve-funding deposit.
pub const CGT_V2_FUNDING_GAS: u64 = 100_000;
/// Reserved gas for the static group and the dynamic funding deposit.
pub const CGT_V2_GROUP_GAS: u64 = 48_100_000;
/// Fixed depositor authorized by the proxy admin and CGT setter.
pub const CGT_V2_DEPOSITOR: Address = address!("deaddeaddeaddeaddeaddeaddeaddeaddead0001");
/// L1Block proxy.
pub const CGT_V2_L1_BLOCK: Address = address!("4200000000000000000000000000000000000015");
/// Liquidity controller mints the reserve through an ordinary deposit.
pub const CGT_V2_CONTROLLER: Address = address!("420000000000000000000000000000000000002a");
/// Reserve predeploy.
pub const CGT_V2_RESERVE: Address = address!("4200000000000000000000000000000000000029");
/// Migration-specific flag, unused and zero in the legacy layout.
pub const CGT_V2_FLAG_SLOT: B256 =
    b256!("d2ff82c9b477ff6a09f530b1c627ffb4b0b81e2ae2ba427f824162e8dad020aa");

const INTENTS: [&str; CGT_V2_STEPS] = [
    "Cgtv2 0: StorageSetter deployment",
    "Cgtv2 1: L1BlockCGT deployment",
    "Cgtv2 2: L2ToL1MessagePasserCGT deployment",
    "Cgtv2 3: NativeAssetLiquidity deployment",
    "Cgtv2 4: LiquidityController deployment",
    "Cgtv2 5: CeloGasBridgeL2 deployment",
    "Cgtv2 6: CeloSequencerFeeVault deployment",
    "Cgtv2 7: L1BlockCGT upgrade",
    "Cgtv2 8: L2ToL1MessagePasserCGT upgrade",
    "Cgtv2 9: NativeAssetLiquidity upgrade",
    "Cgtv2 10: LiquidityController initializer reset",
    "Cgtv2 11: LiquidityController upgrade and initialize",
    "Cgtv2 12: CeloGasBridgeL2 initializer reset",
    "Cgtv2 13: CeloGasBridgeL2 upgrade and initialize",
    "Cgtv2 14: CeloSequencerFeeVault initializer reset",
    "Cgtv2 15: CeloSequencerFeeVault upgrade and initialize",
    "Cgtv2 16: CeloGasBridgeL2 minter authorization",
    "Cgtv2 17: LiquidityController ownership transfer",
    "Cgtv2 18: CGTv2 flag enable",
    "Celo: CGTv2 Reserve Funding",
];
const STATIC: [&[u8]; CGT_V2_STATIC_STEPS] = [
    include_bytes!("../testdata/cgt_v2/cgtv2-00.rlp"),
    include_bytes!("../testdata/cgt_v2/cgtv2-01.rlp"),
    include_bytes!("../testdata/cgt_v2/cgtv2-02.rlp"),
    include_bytes!("../testdata/cgt_v2/cgtv2-03.rlp"),
    include_bytes!("../testdata/cgt_v2/cgtv2-04.rlp"),
    include_bytes!("../testdata/cgt_v2/cgtv2-05.rlp"),
    include_bytes!("../testdata/cgt_v2/cgtv2-06.rlp"),
    include_bytes!("../testdata/cgt_v2/cgtv2-07.rlp"),
    include_bytes!("../testdata/cgt_v2/cgtv2-08.rlp"),
    include_bytes!("../testdata/cgt_v2/cgtv2-09.rlp"),
    include_bytes!("../testdata/cgt_v2/cgtv2-10.rlp"),
    include_bytes!("../testdata/cgt_v2/cgtv2-11.rlp"),
    include_bytes!("../testdata/cgt_v2/cgtv2-12.rlp"),
    include_bytes!("../testdata/cgt_v2/cgtv2-13.rlp"),
    include_bytes!("../testdata/cgt_v2/cgtv2-14.rlp"),
    include_bytes!("../testdata/cgt_v2/cgtv2-15.rlp"),
    include_bytes!("../testdata/cgt_v2/cgtv2-16.rlp"),
    include_bytes!("../testdata/cgt_v2/cgtv2-17.rlp"),
    include_bytes!("../testdata/cgt_v2/cgtv2-18.rlp"),
];

/// Invalid required transaction fields or unresolved network parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgtV2GroupError {
    /// The network or its deployment-specific bundle has not been configured.
    Config(CgtV2ConfigError),
    /// The migrated amount or required transaction is invalid.
    Invalid,
}
impl core::fmt::Display for CgtV2GroupError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Config(e) => e.fmt(f),
            Self::Invalid => f.write_str("invalid CGTv2 upgrade deposit"),
        }
    }
}
impl core::error::Error for CgtV2GroupError {}

/// Shared group construction, recognition and canonical transaction checks.
#[derive(Debug)]
pub struct CgtV2UpgradeGroup;
impl CgtV2UpgradeGroup {
    /// Uses the existing upgrade domain and NUT intent qualification.
    fn source_hash(step: usize) -> B256 {
        UpgradeDepositSource { intent: INTENTS[step].into() }.source_hash()
    }

    /// Identifies a reserved CGTv2 source, including noncanonical transaction fields.
    pub fn step(tx: &TxDeposit) -> Option<usize> {
        (0..CGT_V2_STEPS).find(|&i| tx.source_hash == Self::source_hash(i))
    }

    /// Recognizes any required group transaction for payload/import no-user checks.
    pub fn is_activation_transaction(tx: &impl OpTransaction) -> bool {
        tx.as_deposit().is_some_and(|deposit| Self::step(deposit).is_some())
    }

    /// Recognizes group members in encoded Engine API payloads.
    pub fn is_activation_encoded(encoded: &[u8]) -> bool {
        encoded.first() == Some(&op_alloy_consensus::DEPOSIT_TX_TYPE_ID)
            && OpTxEnvelope::decode_2718_exact(encoded)
                .is_ok_and(|tx| Self::is_activation_transaction(&tx))
    }

    fn static_deposit(step: usize) -> TxDeposit {
        // These frozen bytes are decoded and checked against NUT/source-hash vectors in tests.
        OpTxEnvelope::decode_2718_exact(STATIC[step])
            .expect("valid frozen CGTv2 NUT deposit")
            .as_deposit()
            .expect("deposit")
            .clone()
            .into_inner()
    }

    fn funding(reserve: U256) -> Result<TxDeposit, CgtV2GroupError> {
        Ok(TxDeposit {
            source_hash: Self::source_hash(CGT_V2_STATIC_STEPS),
            from: CGT_V2_CONTROLLER,
            to: TxKind::Call(CGT_V2_RESERVE),
            mint: reserve.try_into().map_err(|_| CgtV2GroupError::Invalid)?,
            value: reserve,
            gas_limit: CGT_V2_FUNDING_GAS,
            is_system_transaction: false,
            input: Bytes::copy_from_slice(&keccak256(b"deposit()")[..4]),
        })
    }

    fn bundle_params(chain_id: u64) -> Result<CgtV2NetworkParams, CgtV2GroupError> {
        let params = cgt_v2_params(chain_id).map_err(CgtV2GroupError::Config)?;
        // TODO(CGTv2): replace this fixture-only guard with per-network bundle selection
        // once the production contracts and network configuration are finalized.
        // A network profile does not authorize running fixture contracts on that network.
        if chain_id != CGT_V2_FIXTURE_CHAIN_ID {
            return Err(CgtV2GroupError::Config(CgtV2ConfigError { chain_id }));
        }
        Ok(params)
    }

    /// Constructs all required transactions from the authenticated event amount.
    pub fn deposits(
        chain_id: u64,
        migrated_amount: U256,
    ) -> Result<Vec<TxDeposit>, CgtV2GroupError> {
        let params = Self::bundle_params(chain_id)?;
        let reserve = params.reserve_seed(migrated_amount).ok_or(CgtV2GroupError::Invalid)?;
        let mut txs: Vec<_> = (0..CGT_V2_STATIC_STEPS).map(Self::static_deposit).collect();
        txs.push(Self::funding(reserve)?);
        Ok(txs)
    }

    /// A reserved source must match every field of the corresponding required deposit.
    /// Execution checks the funding shape/bounds; derivation authenticates its exact amount.
    pub fn validate(chain_id: u64, tx: &TxDeposit) -> Result<Option<usize>, CgtV2GroupError> {
        let Some(step) = Self::step(tx) else { return Ok(None) };
        let params = Self::bundle_params(chain_id)?;
        let expected = if step < CGT_V2_STATIC_STEPS {
            Self::static_deposit(step)
        } else {
            params.reserve_seed(tx.value).ok_or(CgtV2GroupError::Invalid)?;
            Self::funding(tx.value)?
        };
        if *tx != expected {
            return Err(CgtV2GroupError::Invalid);
        }
        Ok(Some(step))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_eips::Encodable2718;
    #[test]
    fn cgt_group_matches_shared_vectors() {
        let txs = CgtV2UpgradeGroup::deposits(1337, U256::from(600)).unwrap();
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/cgt_v2/cgtv2-vectors.json")).unwrap();
        for (i, tx) in txs.iter().enumerate() {
            assert_eq!(
                tx.source_hash,
                vectors[i]["sourceHash"].as_str().unwrap().parse::<B256>().unwrap()
            );
            assert_eq!(
                keccak256(tx.encoded_2718()),
                vectors[i]["transactionHash"].as_str().unwrap().parse::<B256>().unwrap()
            );
            assert_eq!(CgtV2UpgradeGroup::validate(1337, tx).unwrap(), Some(i));
            let mut changed = tx.clone();
            changed.gas_limit += 1;
            assert!(CgtV2UpgradeGroup::validate(1337, &changed).is_err());
        }
        assert_eq!(txs.iter().map(|tx| tx.gas_limit).sum::<u64>(), CGT_V2_GROUP_GAS);
        assert!(CgtV2UpgradeGroup::deposits(1337, U256::from(1001)).is_err());
        for id in [42220, 11142220, 11162320] {
            assert!(CgtV2UpgradeGroup::deposits(id, U256::ZERO).is_err());
            for tx in &txs {
                assert!(CgtV2UpgradeGroup::validate(id, tx).is_err());
            }
        }
        assert_eq!(
            CgtV2UpgradeGroup::deposits(1337, U256::from(1000)).unwrap().last().unwrap().mint,
            0
        );
    }
    #[test]
    fn cgt_v2_reserve_checks_amount() {
        let p = cgt_v2_params(1337).unwrap();
        assert_eq!(p.reserve_seed(U256::from(600)), Some(U256::from(400)));
        assert_eq!(p.reserve_seed(U256::ZERO), Some(U256::from(1000)));
        assert_eq!(p.reserve_seed(U256::from(1000)), Some(U256::ZERO));
        assert_eq!(p.reserve_seed(U256::from(1001)), None);
    }
    #[test]
    fn cgt_v2_active_networks_use_fixed_supply() {
        let supply = U256::from(1_000_000_000_000_000_000_000_000_000_u128);
        for chain_id in [42220, 11142220, 11162320] {
            let params = cgt_v2_params(chain_id).unwrap();
            assert_ne!(params.celo_gas_bridge_l1, Address::ZERO);
            assert_ne!(params.celo_gas_bridge_l1, Address::with_last_byte(0xcc));
            assert_eq!(params.reserve_seed(U256::ZERO), Some(supply));
            assert_eq!(params.reserve_seed(supply), Some(U256::ZERO));
            assert_eq!(params.reserve_seed(supply + U256::from(1)), None);
        }
        for chain_id in [44787, 62320, 1] {
            assert_eq!(cgt_v2_params(chain_id), Err(CgtV2ConfigError { chain_id }));
        }
    }
}
