//! Receipt-triggered CGTv2 upgrade deposits selected from frozen network artifacts.

use crate::decode_2718_canonical;
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
    bundle: &'static [u8],
    // Register new hashes before emitting the first retry request for them on L1.
    retry_bundles: &'static [CgtV2RetryBundle],
}

/// A retry artifact and its pinned hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CgtV2RetryBundle {
    // The hash is pinned independently of the embedded artifact.
    hash: B256,
    bundle: &'static [u8],
}

impl CgtV2NetworkParams {
    /// Finds the registered retry bundle with this hash.
    pub fn retry_bundle(&self, hash: B256) -> Option<&'static CgtV2RetryBundle> {
        if hash.is_zero() {
            return None;
        }
        self.retry_bundles.iter().find(|retry| retry.hash == hash)
    }

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
            retry_bundles: &[],
            total_supply: CGT_V2_TOTAL_SUPPLY,
            bundle: include_bytes!("../upgrades/cgtv2/mainnet.rlp"),
        },
    },
    CgtV2NetworkEntry {
        chain_id: CELO_SEPOLIA_CHAIN_ID,
        params: CgtV2NetworkParams {
            celo_gas_bridge_l1: TEMPORARY_L1_BRIDGE,
            retry_bundles: &[],
            total_supply: CGT_V2_TOTAL_SUPPLY,
            bundle: include_bytes!("../upgrades/cgtv2/sepolia.rlp"),
        },
    },
    CgtV2NetworkEntry {
        chain_id: CELO_CHAOS_CHAIN_ID,
        params: CgtV2NetworkParams {
            celo_gas_bridge_l1: TEMPORARY_L1_BRIDGE,
            retry_bundles: &[],
            total_supply: CGT_V2_TOTAL_SUPPLY,
            bundle: include_bytes!("../upgrades/cgtv2/chaos.rlp"),
        },
    },
];

// Fixture inputs must never become production defaults or select production bundles.
const FIXTURE_PARAMS: CgtV2NetworkParams = CgtV2NetworkParams {
    celo_gas_bridge_l1: Address::with_last_byte(0xcc),
    total_supply: uint!(1000_U256),
    bundle: include_bytes!("../upgrades/cgtv2/fixture.rlp"),
    retry_bundles: &[CgtV2RetryBundle {
        hash: b256!("437fabe614e74fd20231c33accdacbad38631aaefc16abdb3c38c3bc5be10f14"),
        bundle: include_bytes!("../upgrades/cgtv2/fixture.rlp"),
    }],
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
pub const CGT_V2_STATIC_STEPS: usize = 15;
/// Number of required transactions in the complete group.
pub const CGT_V2_STEPS: usize = CGT_V2_STATIC_STEPS + 1;
/// Gas limit for the dynamic reserve-funding deposit.
pub const CGT_V2_FUNDING_GAS: u64 = 100_000;
/// Synthetic fixture allowance. Runtime gas is derived from the selected bundle.
pub const CGT_V2_GROUP_GAS: u64 = 40_600_000;
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
    "Cgtv2 0: L1BlockCGT deployment",
    "Cgtv2 1: L2ToL1MessagePasserCGT deployment",
    "Cgtv2 2: NativeAssetLiquidity deployment",
    "Cgtv2 3: LiquidityController deployment",
    "Cgtv2 4: CeloGasBridgeL2 deployment",
    "Cgtv2 5: CeloSequencerFeeVault deployment",
    "Cgtv2 6: L1BlockCGT upgrade",
    "Cgtv2 7: L2ToL1MessagePasserCGT upgrade",
    "Cgtv2 8: NativeAssetLiquidity upgrade",
    "Cgtv2 9: LiquidityController upgrade and initialize",
    "Cgtv2 10: CeloGasBridgeL2 upgrade and initialize",
    "Cgtv2 11: CeloSequencerFeeVault upgrade and initialize",
    "Cgtv2 12: CeloGasBridgeL2 minter authorization",
    "Cgtv2 13: LiquidityController ownership transfer",
    "Cgtv2 14: CGTv2 flag enable",
    "Celo: CGTv2 Reserve Funding",
];

/// Invalid required transaction fields or unresolved network parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgtV2GroupError {
    /// The network or its deployment-specific bundle has not been configured.
    Config(CgtV2ConfigError),
    /// The migrated amount or required transaction is invalid.
    Invalid,
    /// The embedded artifact differs from the authorized bundle hash.
    RetryHashMismatch(B256),
}
impl core::fmt::Display for CgtV2GroupError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Config(e) => e.fmt(f),
            Self::Invalid => f.write_str("invalid CGTv2 upgrade deposit"),
            Self::RetryHashMismatch(hash) => {
                write!(f, "local CGTv2 retry artifact does not match protocol hash {hash}")
            }
        }
    }
}
impl core::error::Error for CgtV2GroupError {}

/// Shared group construction, recognition and canonical transaction checks.
#[derive(Debug)]
pub struct CgtV2UpgradeGroup;

impl CgtV2UpgradeGroup {
    /// Hash of the canonical RLP list of static upgrade deposits.
    pub fn bundle_hash(chain_id: u64) -> Result<B256, CgtV2GroupError> {
        let params = cgt_v2_params(chain_id).map_err(CgtV2GroupError::Config)?;
        Self::decode_bundle(chain_id, params.bundle)?;
        Ok(keccak256(params.bundle))
    }

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

    fn static_deposits(chain_id: u64) -> Result<Vec<TxDeposit>, CgtV2GroupError> {
        let params = cgt_v2_params(chain_id).map_err(CgtV2GroupError::Config)?;
        Self::decode_bundle(chain_id, params.bundle)
    }

    fn decode_bundle(chain_id: u64, bytes: &[u8]) -> Result<Vec<TxDeposit>, CgtV2GroupError> {
        if bytes.is_empty() {
            return Err(CgtV2GroupError::Config(CgtV2ConfigError { chain_id }));
        }
        let raw =
            alloy_rlp::decode_exact::<Vec<Bytes>>(bytes).map_err(|_| CgtV2GroupError::Invalid)?;
        if raw.is_empty() {
            return Err(CgtV2GroupError::Config(CgtV2ConfigError { chain_id }));
        }
        if raw.len() != CGT_V2_STATIC_STEPS {
            return Err(CgtV2GroupError::Invalid);
        }
        raw.iter()
            .enumerate()
            .map(|(step, bytes)| {
                let envelope = decode_2718_canonical::<OpTxEnvelope>(bytes)
                    .map_err(|_| CgtV2GroupError::Invalid)?;
                let tx = envelope.as_deposit().ok_or(CgtV2GroupError::Invalid)?.inner().clone();
                if tx.source_hash != Self::source_hash(step)
                    || tx.mint != 0
                    || tx.value != U256::ZERO
                    || tx.is_system_transaction
                    || tx.gas_limit == 0
                {
                    return Err(CgtV2GroupError::Invalid);
                }
                Ok(tx)
            })
            .collect()
    }

    /// Checked allowance for the original upgrade group.
    pub fn gas_limit(chain_id: u64) -> Result<u64, CgtV2GroupError> {
        Self::group_gas(&Self::static_deposits(chain_id)?)
    }

    fn group_gas(txs: &[TxDeposit]) -> Result<u64, CgtV2GroupError> {
        txs.iter().try_fold(CGT_V2_FUNDING_GAS, |gas, tx| {
            gas.checked_add(tx.gas_limit).ok_or(CgtV2GroupError::Invalid)
        })
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

    /// Constructs the original upgrade deposits and dynamic funding.
    pub fn deposits(
        chain_id: u64,
        migrated_amount: U256,
    ) -> Result<Vec<TxDeposit>, CgtV2GroupError> {
        let params = cgt_v2_params(chain_id).map_err(CgtV2GroupError::Config)?;
        params.upgrade_transactions(chain_id, migrated_amount, None).map(|(txs, _)| txs)
    }

    /// Matches any CGTv2 suffix in a decoded block and returns its one-time gas allowance.
    pub fn block_upgrade_gas<'a, T: OpTransaction + 'a>(
        chain_id: u64,
        mut transactions: impl Iterator<Item = &'a T>,
    ) -> Result<Option<u64>, CgtV2GroupError> {
        let mut has_prefix = false;
        let mut only_deposits = true;
        while let Some(tx) = transactions.next() {
            if Self::is_activation_transaction(tx) {
                if !has_prefix || !only_deposits {
                    return Err(CgtV2GroupError::Invalid);
                }
                let group = core::iter::once(tx).chain(transactions);
                return Self::validate_group(
                    chain_id,
                    group.map(|tx| tx.as_deposit().map(|tx| tx.inner())),
                )
                .map(Some);
            }
            has_prefix = true;
            only_deposits &= tx.as_deposit().is_some();
        }
        Ok(None)
    }

    /// Validates a complete group against one pinned artifact and returns its allowance.
    pub fn validate_group<'a>(
        chain_id: u64,
        deposits: impl IntoIterator<Item = Option<&'a TxDeposit>>,
    ) -> Result<u64, CgtV2GroupError> {
        CgtV2GroupMatcher::new(chain_id)?.validate_group(deposits)
    }
}

impl CgtV2RetryBundle {
    fn static_deposits(&self, chain_id: u64) -> Result<Vec<TxDeposit>, CgtV2GroupError> {
        let txs = CgtV2UpgradeGroup::decode_bundle(chain_id, self.bundle)?;
        if keccak256(self.bundle) != self.hash {
            return Err(CgtV2GroupError::RetryHashMismatch(self.hash));
        }
        Ok(txs)
    }
}

impl CgtV2NetworkParams {
    /// Returns the selected complete group and its checked one-time gas allowance.
    pub fn upgrade_transactions(
        &self,
        chain_id: u64,
        migrated_amount: U256,
        retry: Option<&CgtV2RetryBundle>,
    ) -> Result<(Vec<TxDeposit>, u64), CgtV2GroupError> {
        let mut txs = match retry {
            Some(retry) => retry.static_deposits(chain_id)?,
            None => CgtV2UpgradeGroup::decode_bundle(chain_id, self.bundle)?,
        };
        let gas = CgtV2UpgradeGroup::group_gas(&txs)?;
        let reserve = self.reserve_seed(migrated_amount).ok_or(CgtV2GroupError::Invalid)?;
        txs.push(CgtV2UpgradeGroup::funding(reserve)?);
        Ok((txs, gas))
    }
}

#[derive(Debug)]
struct CgtV2StaticBundle {
    deposits: Vec<TxDeposit>,
    gas: u64,
}

impl CgtV2StaticBundle {
    fn new(deposits: Vec<TxDeposit>) -> Result<Self, CgtV2GroupError> {
        let gas = CgtV2UpgradeGroup::group_gas(&deposits)?;
        Ok(Self { deposits, gas })
    }
}

/// One checked step, recorded only when the caller commits its execution result.
#[derive(Debug)]
pub struct CgtV2MatchedStep(TxDeposit);

/// Matches a complete group while retaining only artifacts that match its committed prefix.
#[derive(Debug)]
pub struct CgtV2GroupMatcher {
    params: CgtV2NetworkParams,
    candidates: Vec<CgtV2StaticBundle>,
    next_step: usize,
}

impl CgtV2GroupMatcher {
    /// Loads the known artifacts for the chain when an upgrade group is encountered.
    pub fn new(chain_id: u64) -> Result<Self, CgtV2GroupError> {
        let params = cgt_v2_params(chain_id).map_err(CgtV2GroupError::Config)?;
        Self::for_network(chain_id, params)
    }

    /// Loads known artifacts for the given network parameters.
    pub fn for_network(chain_id: u64, params: CgtV2NetworkParams) -> Result<Self, CgtV2GroupError> {
        let initial = CgtV2UpgradeGroup::decode_bundle(chain_id, params.bundle)
            .and_then(CgtV2StaticBundle::new);
        // A broken local artifact for an unused registration must not invalidate prior groups.
        // Derivation reports its error on an authenticated request for that registration.
        let mut candidates: Vec<_> = params
            .retry_bundles
            .iter()
            .filter_map(|retry| {
                retry.static_deposits(chain_id).and_then(CgtV2StaticBundle::new).ok()
            })
            .collect();
        match initial {
            Ok(initial) => candidates.push(initial),
            Err(error) if candidates.is_empty() => return Err(error),
            Err(_) => {}
        }
        Ok(Self { params, candidates, next_step: 0 })
    }

    /// Returns the number of committed required deposits.
    pub const fn next_step(&self) -> usize {
        self.next_step
    }

    /// Checks the next step against the same artifacts as all previous steps.
    pub fn check(&self, tx: &TxDeposit) -> Result<CgtV2MatchedStep, CgtV2GroupError> {
        let valid = match self.next_step {
            CGT_V2_STATIC_STEPS => {
                self.params.reserve_seed(tx.value).is_some()
                    && *tx == CgtV2UpgradeGroup::funding(tx.value)?
            }
            step if step < CGT_V2_STATIC_STEPS => {
                self.candidates.iter().any(|bundle| bundle.deposits[step] == *tx)
            }
            _ => false,
        };
        if !valid {
            return Err(CgtV2GroupError::Invalid);
        }
        Ok(CgtV2MatchedStep(tx.clone()))
    }

    /// Advances a step checked by this matcher when its execution result is committed.
    pub fn commit(&mut self, step: CgtV2MatchedStep) {
        if self.next_step < CGT_V2_STATIC_STEPS {
            self.candidates.retain(|bundle| bundle.deposits[self.next_step] == step.0);
        }
        self.next_step += 1;
    }

    /// Completes validation and returns the allowance of the matching artifact.
    pub fn validate_group<'a>(
        mut self,
        deposits: impl IntoIterator<Item = Option<&'a TxDeposit>>,
    ) -> Result<u64, CgtV2GroupError> {
        for deposit in deposits {
            let step = self.check(deposit.ok_or(CgtV2GroupError::Invalid)?)?;
            self.commit(step);
        }
        if self.next_step != CGT_V2_STEPS {
            return Err(CgtV2GroupError::Invalid);
        }
        self.candidates.first().map(|bundle| bundle.gas).ok_or(CgtV2GroupError::Invalid)
    }
}

#[cfg(any(test, feature = "test-utils"))]
/// Test-only variants of the synthetic profile.
pub mod test_utils {
    use super::*;

    /// Keccak of the Go/Rust fixture with 111 and 222 extra gas on steps 3 and 9.
    pub const APPENDED_RETRY_HASH: B256 =
        b256!("90a1acc747b73e211612cc656db22ff1cc65ed6c9c8dc2bc22da40160fc7abc0");

    /// Uses the synthetic profile without any registered retry hashes.
    pub const fn fixture_without_retries() -> CgtV2NetworkParams {
        CgtV2NetworkParams { retry_bundles: &[], ..FIXTURE_PARAMS }
    }

    /// Adds the distinct retry artifact while preserving the initial and previous retry artifacts.
    pub const fn with_appended_retry() -> CgtV2NetworkParams {
        const RETRIES: [CgtV2RetryBundle; 2] = [
            FIXTURE_PARAMS.retry_bundles[0],
            CgtV2RetryBundle {
                hash: APPENDED_RETRY_HASH,
                bundle: include_bytes!("../testdata/cgt_v2/cgtv2-fixture-retry.rlp"),
            },
        ];
        CgtV2NetworkParams { retry_bundles: &RETRIES, ..FIXTURE_PARAMS }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_eips::Encodable2718;

    #[test]
    fn cgt_v2_zero_pin_is_unset() {
        const RETRIES: [CgtV2RetryBundle; 1] =
            [CgtV2RetryBundle { hash: B256::ZERO, ..FIXTURE_PARAMS.retry_bundles[0] }];
        let params = CgtV2NetworkParams { retry_bundles: &RETRIES, ..FIXTURE_PARAMS };
        assert!(params.retry_bundle(B256::ZERO).is_none());
    }

    #[test]
    fn separate_retry_transaction_matches_registered_bundle() {
        use test_utils::{APPENDED_RETRY_HASH, with_appended_retry};

        let params = with_appended_retry();
        let matcher = || CgtV2GroupMatcher::for_network(1337, params).unwrap();
        let amount = U256::from(987);
        let first = CgtV2UpgradeGroup::deposits(1337, amount).unwrap();
        let old_retry = params.retry_bundle(CgtV2UpgradeGroup::bundle_hash(1337).unwrap());
        assert!(old_retry.is_some());
        assert_eq!(
            params.upgrade_transactions(1337, amount, old_retry).unwrap(),
            (first.clone(), 40_600_000)
        );
        let new_retry = params.retry_bundle(APPENDED_RETRY_HASH);
        assert!(new_retry.is_some());
        let (mut retry, gas) = params.upgrade_transactions(1337, amount, new_retry).unwrap();
        assert_eq!(gas, 40_600_333);
        assert_eq!(retry[3].gas_limit, 6_000_111);
        assert_eq!(retry[9].gas_limit, 500_222);
        assert_eq!(retry[..3], first[..3]);
        assert_eq!(retry.last().unwrap().mint, 13);
        assert_eq!(matcher().validate_group(first.iter().map(Some)).unwrap(), 40_600_000);
        assert_eq!(matcher().validate_group(retry.iter().map(Some)).unwrap(), 40_600_333);
        retry[9] = first[9].clone();
        assert!(matcher().validate_group(retry.iter().map(Some)).is_err());
    }

    #[test]
    fn bad_retry_artifact_is_scoped_to_its_request() {
        const RETRIES: [CgtV2RetryBundle; 4] = [
            FIXTURE_PARAMS.retry_bundles[0],
            CgtV2RetryBundle { hash: B256::with_last_byte(2), bundle: &[] },
            CgtV2RetryBundle { hash: B256::with_last_byte(3), bundle: &[0xff] },
            CgtV2RetryBundle { hash: B256::with_last_byte(4), bundle: FIXTURE_PARAMS.bundle },
        ];
        let params = CgtV2NetworkParams { retry_bundles: &RETRIES, ..FIXTURE_PARAMS };
        for retry in &RETRIES[1..] {
            let attempt = params.retry_bundle(retry.hash).unwrap();
            assert!(params.upgrade_transactions(1337, U256::from(600), Some(attempt)).is_err());
        }
        let group = CgtV2UpgradeGroup::deposits(1337, U256::from(600)).unwrap();
        let allowance = CgtV2GroupMatcher::for_network(1337, params)
            .unwrap()
            .validate_group(group.iter().map(Some))
            .unwrap();
        assert_eq!(allowance, 40_600_000);
    }

    #[test]
    fn cgt_v2_bundle_hash_matches_go() {
        assert_eq!(
            CgtV2UpgradeGroup::bundle_hash(CGT_V2_FIXTURE_CHAIN_ID).unwrap(),
            b256!("437fabe614e74fd20231c33accdacbad38631aaefc16abdb3c38c3bc5be10f14")
        );
    }

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
            assert_eq!(CgtV2UpgradeGroup::step(tx), Some(i));
            let mut changed = txs.clone();
            changed[i].gas_limit += 1;
            assert!(CgtV2UpgradeGroup::validate_group(1337, changed.iter().map(Some)).is_err());
        }
        assert_eq!(txs.iter().map(|tx| tx.gas_limit).sum::<u64>(), CGT_V2_GROUP_GAS);
        assert!(CgtV2UpgradeGroup::deposits(1337, U256::from(1001)).is_err());
        for id in [42220, 11142220, 11162320] {
            assert!(CgtV2UpgradeGroup::deposits(id, U256::ZERO).is_err());
            assert!(CgtV2UpgradeGroup::validate_group(id, txs.iter().map(Some)).is_err());
        }
        assert_eq!(
            CgtV2UpgradeGroup::deposits(1337, U256::from(1000)).unwrap().last().unwrap().mint,
            0
        );
    }
    #[test]
    fn cgt_v2_network_artifacts_and_mock_gas() {
        for id in [CELO_MAINNET_CHAIN_ID, CELO_SEPOLIA_CHAIN_ID, CELO_CHAOS_CHAIN_ID] {
            // Exercise the production profile with a mock artifact; checked-in placeholders stay
            // disabled.
            let txs =
                CgtV2NetworkParams { bundle: FIXTURE_PARAMS.bundle, ..cgt_v2_params(id).unwrap() }
                    .upgrade_transactions(id, U256::from(600), None)
                    .map(|(txs, _)| txs)
                    .unwrap();
            assert_eq!(txs.last().unwrap().value, CGT_V2_TOTAL_SUPPLY - U256::from(600));
            assert!(CgtV2UpgradeGroup::gas_limit(id).is_err());
        }
        let mut raw = alloy_rlp::decode_exact::<Vec<Bytes>>(FIXTURE_PARAMS.bundle).unwrap();
        let mut tx = decode_2718_canonical::<OpTxEnvelope>(&raw[0])
            .unwrap()
            .as_deposit()
            .unwrap()
            .inner()
            .clone();
        tx.gas_limit += 111;
        raw[0] = tx.encoded_2718().into();
        let encoded = alloy_rlp::encode(&raw);
        let decoded = CgtV2UpgradeGroup::decode_bundle(1337, &encoded).unwrap();
        assert_eq!(CgtV2UpgradeGroup::group_gas(&decoded).unwrap(), 40_600_111);
        assert_eq!(CgtV2UpgradeGroup::gas_limit(1337).unwrap(), 40_600_000);
        tx.gas_limit = u64::MAX;
        raw[0] = tx.encoded_2718().into();
        assert!(
            CgtV2UpgradeGroup::decode_bundle(1337, &alloy_rlp::encode(&raw))
                .and_then(|txs| CgtV2UpgradeGroup::group_gas(&txs))
                .is_err()
        );
        tx.gas_limit = 6_000_000;
        tx.source_hash = B256::ZERO;
        raw[0] = tx.encoded_2718().into();
        assert!(CgtV2UpgradeGroup::decode_bundle(1337, &alloy_rlp::encode(&raw)).is_err());
        assert!(
            CgtV2UpgradeGroup::decode_bundle(1337, &alloy_rlp::encode(raw[..14].to_vec())).is_err()
        );
        assert!(CgtV2UpgradeGroup::decode_bundle(1337, &[0xc0, 0x00]).is_err());
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
