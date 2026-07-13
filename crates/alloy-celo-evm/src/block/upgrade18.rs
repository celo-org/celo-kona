//! CGT v2 (OP Upgrade 18) irregular state transition.
//!
//! At the Upgrade 18 activation block, Celo migrates from Custom Gas Token v1 to v2 by
//! installing / upgrading the CGT v2 predeploys **via direct state writes**, before any
//! transaction of the block executes — the same mechanism as OP Canyon's create2deployer
//! force-deploy ([`alloy_op_evm`]'s `ensure_create2_deployer`). This is the "all-in-reth"
//! install path: no network-upgrade deposit transactions are involved, and because this
//! module runs inside the shared block executor it covers the node (celo-reth) and the
//! fault-proof client (celo-kona) with a single implementation.
//!
//! # The activation artifact
//!
//! The injected state comes from a pinned artifact, `res/predeploys.json`, generated in
//! celo-org/optimism at the frozen `celo-contracts/v6.0.0` tag by simulating the real
//! `L2Genesis.s.sol` init calls — so the post-fork state of the touched accounts is
//! byte-identical to a fresh v6.0.0 genesis. It is compiled into the generated (no-std)
//! `upgrade18_data` module covering six predeploys:
//! `L1BlockCGT` (`0x42…0015`), `L2ToL1MessagePasserCGT` (`0x42…0016`),
//! `CeloSequencerFeeVault` (`0x42…0011`), `NativeAssetLiquidity` (`0x42…0029`, carries
//! the CELO reserve seed), `LiquidityController` (`0x42…002A`), and `CeloGasBridgeL2`
//! (`0x42…1023`).
//!
//! The artifact carries no account nonces, and the transition leaves every touched
//! account's nonce at its live pre-fork value (zero for fresh installs). The
//! byte-identical claim above therefore assumes the reference genesis assigns these
//! accounts nonce zero — true for its `vm.etch`-style predeploy allocs today, and
//! something the artifact generator must keep true (or the artifact must grow a nonce
//! field) if that ever changes.
//!
//! # Parameters
//!
//! Four artifact values are `param:` placeholders ([`Upgrade18Param`]). Two are known
//! per network and ship in the artifact (`LiquidityControllerOwner` = Celo governance,
//! `CeloTokenL1`); two are only known at migration time and must arrive via
//! [`Upgrade18Overrides`] or a regenerated artifact (`CeloGasBridgeL1` from the L1
//! deploy, `NativeAssetLiquidityAmount` — the reserve seed, research issue
//! celo-blockchain-planning#1405). Resolution order: override, then per-network
//! constant. **A scheduled fork with an unresolvable parameter fails block execution**
//! at the boundary — the chain halts loudly instead of silently skipping the migration
//! or seeding a zero reserve. An explicitly zero value is rejected the same way: no
//! placeholder has a legitimate zero, so a zeroed override (operator typo) must not
//! reach state.
//!
//! The reserve seed is the migration's single deliberate "mint native CELO" step; it
//! backs the CELO held by the legacy `OptimismPortal` escrow on L1. Balances are applied
//! as *increments*, so dust force-sent to a predeploy before the fork is preserved
//! rather than burned.

use super::upgrade18_data::{
    PROXY_SHELL_BYTECODE, PROXY_SHELL_CODE_HASH, RAW_PREDEPLOYS, SlotValue,
};
use alloc::vec::Vec;
use alloy_evm::{Database, block::BlockExecutionError};
use alloy_primitives::{B256, Bytes, U256, address};
use revm::{
    primitives::Address,
    state::{Account, Bytecode, EvmState, EvmStorageSlot},
};

/// The `CeloGasBridgeL2` predeploy proxy. Serves as the transition's completion marker:
/// it is the one touched address guaranteed to have **no code** before the fork (verified
/// in the live-chain audit, research issue celo-blockchain-planning#1404 — the address is
/// not CREATE/CREATE2-reachable, so nobody can deploy to it) and code after.
pub const CELO_GAS_BRIDGE_L2: Address = address!("0x4200000000000000000000000000000000001023");

/// A `param:` placeholder in the activation artifact — a value that is not identical on
/// every network. See the module docs for resolution semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upgrade18Param {
    /// `LiquidityController.initialize` owner (Celo governance). Known per network.
    LiquidityControllerOwner,
    /// The CELO ERC-20 on L1 (`CeloGasBridgeL2`'s remote token). Known per network.
    CeloTokenL1,
    /// The `CeloGasBridgeL1` proxy on L1 (`CeloGasBridgeL2.otherBridge`). Only known
    /// once the v2 L1 contracts are deployed.
    CeloGasBridgeL1,
    /// The native CELO seeded into `NativeAssetLiquidity` — the reserve backing all
    /// bridged CELO. Decided at migration time (celo-blockchain-planning#1405).
    NativeAssetLiquidityAmount,
}

impl Upgrade18Param {
    /// Human-readable artifact key.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::LiquidityControllerOwner => "liquidityControllerOwner",
            Self::CeloTokenL1 => "celoTokenL1",
            Self::CeloGasBridgeL1 => "celoGasBridgeL1",
            Self::NativeAssetLiquidityAmount => "nativeAssetLiquidityAmount",
        }
    }
}

/// Caller-supplied values for the artifact's `param:` placeholders. An override always
/// wins over the artifact's per-network constant; migration-time parameters
/// ([`Upgrade18Param::CeloGasBridgeL1`], [`Upgrade18Param::NativeAssetLiquidityAmount`])
/// have no constant and *must* be supplied here until a finalized artifact ships them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Upgrade18Overrides {
    /// Overrides [`Upgrade18Param::LiquidityControllerOwner`].
    pub liquidity_controller_owner: Option<Address>,
    /// Overrides [`Upgrade18Param::CeloTokenL1`].
    pub celo_token_l1: Option<Address>,
    /// Overrides [`Upgrade18Param::CeloGasBridgeL1`].
    pub celo_gas_bridge_l1: Option<Address>,
    /// Overrides [`Upgrade18Param::NativeAssetLiquidityAmount`].
    pub native_asset_liquidity_amount: Option<U256>,
}

impl Upgrade18Overrides {
    fn get(&self, param: Upgrade18Param) -> Option<U256> {
        let as_word = |addr: &Option<Address>| addr.map(|a| a.into_word().into());
        match param {
            Upgrade18Param::LiquidityControllerOwner => as_word(&self.liquidity_controller_owner),
            Upgrade18Param::CeloTokenL1 => as_word(&self.celo_token_l1),
            Upgrade18Param::CeloGasBridgeL1 => as_word(&self.celo_gas_bridge_l1),
            Upgrade18Param::NativeAssetLiquidityAmount => self.native_asset_liquidity_amount,
        }
    }
}

/// The Upgrade 18 transition cannot run: a scheduled activation reached its boundary
/// block with an unresolvable artifact parameter. Fails block execution (chain halt)
/// rather than skipping the migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Upgrade18ParamMissing {
    /// The unresolvable placeholder (absent, or resolved to the illegitimate value zero).
    pub param: Upgrade18Param,
    /// The chain the resolution ran for.
    pub chain_id: u64,
}

impl core::fmt::Display for Upgrade18ParamMissing {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "Upgrade 18 (CGT v2) is scheduled but artifact param `{}` is unresolved (absent \
             or zero) for chain {} — supply it via Upgrade18Overrides or a regenerated \
             predeploys artifact before the activation timestamp",
            self.param.name(),
            self.chain_id
        )
    }
}

impl core::error::Error for Upgrade18ParamMissing {}

/// One account's fully-resolved state diff, ready to commit.
#[derive(Debug, Clone)]
struct ResolvedAccountDiff {
    address: Address,
    /// Runtime bytecode + its keccak hash (from the artifact); `None` keeps existing
    /// code.
    code: Option<(&'static [u8], B256)>,
    storage: Vec<(U256, U256)>,
    /// Native CELO *added* to the balance (the reserve seed).
    balance_increment: U256,
}

/// Resolves the activation artifact against `chain_id` + `overrides` into concrete
/// account diffs: for each predeploy, the implementation code planted at its
/// deterministic address plus the proxy account's shell code, storage, and balance.
fn resolve_state_diff(
    chain_id: u64,
    overrides: &Upgrade18Overrides,
) -> Result<Vec<ResolvedAccountDiff>, Upgrade18ParamMissing> {
    let resolve = |param: Upgrade18Param| {
        let value = overrides
            .get(param)
            .or_else(|| super::upgrade18_data::known_param(chain_id, param))
            .ok_or(Upgrade18ParamMissing { param, chain_id })?;
        // No placeholder has a legitimate zero value: a zero reserve seed leaves every
        // CELO escrowed on L1 unbacked, and a zero address bricks the receiving contract
        // (ownerless LiquidityController, bridge pointing at 0x0). Treat an explicit
        // zero — most plausibly an operator typo — exactly like an absent value.
        if value.is_zero() {
            return Err(Upgrade18ParamMissing { param, chain_id });
        }
        Ok(value)
    };

    let mut diffs = Vec::with_capacity(RAW_PREDEPLOYS.len() * 2);
    for predeploy in RAW_PREDEPLOYS {
        diffs.push(ResolvedAccountDiff {
            address: predeploy.impl_address,
            code: Some((predeploy.impl_code, predeploy.impl_code_hash)),
            storage: Vec::new(),
            balance_increment: U256::ZERO,
        });

        let mut storage = Vec::with_capacity(predeploy.storage.len());
        for (slot, value) in predeploy.storage {
            let value = match value {
                SlotValue::Literal(word) => *word,
                SlotValue::Param(param) => resolve(*param)?,
            };
            storage.push((*slot, value));
        }
        diffs.push(ResolvedAccountDiff {
            address: predeploy.proxy,
            code: Some((PROXY_SHELL_BYTECODE, PROXY_SHELL_CODE_HASH)),
            storage,
            balance_increment: match predeploy.balance {
                Some(param) => resolve(param)?,
                None => U256::ZERO,
            },
        });
    }
    Ok(diffs)
}

/// Computes the CGT v2 predeploy state changes iff the executed block is the first
/// Upgrade-18-active block whose pre-state does not already contain the migration;
/// returns `None` on every other block.
///
/// The caller (the Celo block executor) is responsible for reporting the returned
/// changes to the block's `OnStateHook` **and** committing them to the database —
/// exactly like `alloy_evm`'s `SystemCaller` does for pre-block system calls. Reporting
/// matters: reth's live engine derives the block's state root from hook-streamed
/// updates, so a silent `db.commit` (the upstream Canyon `ensure_create2_deployer`
/// pattern) produces a sealed root that *excludes* the transition.
///
/// # Why not the upstream Canyon first-block heuristic
///
/// Upstream `ensure_create2_deployer` detects the activation block as
/// `active(t) && !active(t - 2)`, assuming a 2-second block time. Celo runs **1-second
/// blocks**, where that window matches *two* consecutive blocks — and because this
/// transition mints the `NativeAssetLiquidity` reserve seed, running twice would double
/// the mint. An exact `t == activation` match is exactly-once but *skips the transition
/// entirely* if no block lands on the activation second (e.g. a sequencer outage across
/// the boundary).
///
/// Instead the gate is: fork active **and** the [`CELO_GAS_BRIDGE_L2`] marker account has
/// no code in the pre-state. This is exactly-once and gap-proof by construction, and it
/// is a pure function of the block's (config, timestamp, pre-state), so replaying any
/// block — node import, payload building, or the kona proof — reaches the same decision.
/// The cost is one account read per post-activation block. Because the marker check runs
/// *before* parameter resolution, already-migrated chains never need the parameters.
pub(crate) fn cgt_v2_state_changes<DB>(
    upgrade18_time: Option<u64>,
    overrides: &Upgrade18Overrides,
    chain_id: u64,
    timestamp: u64,
    db: &mut DB,
) -> Result<Option<EvmState>, BlockExecutionError>
where
    DB: Database,
{
    let active = upgrade18_time.is_some_and(|activation| timestamp >= activation);
    if !active {
        return Ok(None);
    }
    let already_applied = db
        .basic(CELO_GAS_BRIDGE_L2)
        .map_err(BlockExecutionError::other)?
        .is_some_and(|account| !account.is_empty_code_hash());
    if already_applied {
        return Ok(None);
    }

    let diffs = resolve_state_diff(chain_id, overrides).map_err(BlockExecutionError::other)?;
    tracing::info!(
        target: "celo::upgrade18",
        chain_id,
        timestamp,
        accounts = diffs.len(),
        "applying the Upgrade 18 (CGT v2) predeploy state transition"
    );
    build_state_changes(db, &diffs).map(Some).map_err(BlockExecutionError::other)
}

/// Materializes the resolved account diffs against the live pre-state.
fn build_state_changes<DB>(
    db: &mut DB,
    diffs: &[ResolvedAccountDiff],
) -> Result<EvmState, DB::Error>
where
    DB: Database,
{
    let mut changes = EvmState::default();
    for diff in diffs {
        // Start from the live account: proxies like 0x42…0015 already exist and keep
        // their untouched slots; fresh installs like 0x42…1023 start from the default.
        let mut info = db.basic(diff.address)?.unwrap_or_default();

        if let Some((code, code_hash)) = diff.code {
            info.code_hash = code_hash;
            info.code = Some(Bytecode::new_raw(Bytes::from_static(code)));
        }
        info.balance = info.balance.saturating_add(diff.balance_increment);

        let mut account: Account = info.into();
        for (slot, value) in &diff.storage {
            let original = db.storage(diff.address, *slot)?;
            // Skip writes already at their target value (e.g. the EIP-1967 impl slot of
            // proxies that pre-date the fork): the bundle layer filters unchanged slots
            // anyway, so skipping here keeps the state-hook stream identical to what
            // actually lands in the bundle and the state root.
            if original == *value {
                continue;
            }
            account
                .storage
                .insert(*slot, EvmStorageSlot::new_changed(original, *value, Default::default()));
        }
        account.mark_touch();
        changes.insert(diff.address, account);
    }
    Ok(changes)
}

/// Computes and immediately commits the transition (no state-hook reporting) — the
/// composition used by unit tests and any caller without a streaming state root.
#[cfg(test)]
pub(crate) fn ensure_cgt_v2_predeploys<DB>(
    upgrade18_time: Option<u64>,
    overrides: &Upgrade18Overrides,
    chain_id: u64,
    timestamp: u64,
    db: &mut DB,
) -> Result<(), BlockExecutionError>
where
    DB: Database + revm::DatabaseCommit,
{
    if let Some(changes) = cgt_v2_state_changes(upgrade18_time, overrides, chain_id, timestamp, db)?
    {
        db.commit(changes.into_iter().collect());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{super::upgrade18_data::RawPredeploy, *};
    use alloy_primitives::keccak256;
    use celo_revm::constants::{CELO_MAINNET_CHAIN_ID, CELO_SEPOLIA_CHAIN_ID};
    use revm::database::{CacheDB, EmptyDB};

    /// EIP-1967 implementation slot.
    const IMPL_SLOT: U256 = U256::from_be_bytes(
        alloy_primitives::b256!("360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc")
            .0,
    );

    fn full_overrides() -> Upgrade18Overrides {
        Upgrade18Overrides {
            liquidity_controller_owner: Some(address!("00000000000000000000000000000000000000aa")),
            celo_token_l1: Some(address!("00000000000000000000000000000000000000bb")),
            celo_gas_bridge_l1: Some(address!("00000000000000000000000000000000000000cc")),
            native_asset_liquidity_amount: Some(U256::from(1_000_000u64)),
        }
    }

    /// The embedded artifact is internally consistent: every bytecode matches its pinned
    /// keccak hash, the completion marker is the `CeloGasBridgeL2` proxy, and every
    /// proxy's EIP-1967 impl slot points at its own implementation address.
    #[test]
    fn artifact_integrity() {
        assert_eq!(keccak256(PROXY_SHELL_BYTECODE), PROXY_SHELL_CODE_HASH);
        assert!(
            RAW_PREDEPLOYS.iter().any(|p: &RawPredeploy| p.proxy == CELO_GAS_BRIDGE_L2),
            "the completion marker must be one of the touched proxies"
        );
        for p in RAW_PREDEPLOYS {
            assert_eq!(keccak256(p.impl_code), p.impl_code_hash, "{}: impl code hash", p.name);
            let impl_slot_value = p
                .storage
                .iter()
                .find_map(|(slot, value)| (*slot == IMPL_SLOT).then_some(value))
                .unwrap_or_else(|| panic!("{}: missing EIP-1967 impl slot write", p.name));
            match impl_slot_value {
                SlotValue::Literal(word) => {
                    assert_eq!(
                        Address::from_word((*word).into()),
                        p.impl_address,
                        "{}: impl slot must point at impl_address",
                        p.name
                    );
                }
                SlotValue::Param(_) => panic!("{}: impl slot must be literal", p.name),
            }
        }
    }

    /// Known networks resolve the governance/token params from the artifact but still
    /// require the two migration-time params.
    #[test]
    fn resolution_semantics() {
        for chain_id in [CELO_MAINNET_CHAIN_ID, CELO_SEPOLIA_CHAIN_ID] {
            // Without migration-time params: must fail on one of them.
            let err = resolve_state_diff(chain_id, &Upgrade18Overrides::default()).unwrap_err();
            assert!(
                matches!(
                    err.param,
                    Upgrade18Param::CeloGasBridgeL1 | Upgrade18Param::NativeAssetLiquidityAmount
                ),
                "unexpected missing param: {err}"
            );

            // With just the migration-time params supplied: resolves.
            let overrides = Upgrade18Overrides {
                celo_gas_bridge_l1: Some(address!("00000000000000000000000000000000000000cc")),
                native_asset_liquidity_amount: Some(U256::from(7)),
                ..Default::default()
            };
            let diffs = resolve_state_diff(chain_id, &overrides).unwrap();
            assert_eq!(diffs.len(), RAW_PREDEPLOYS.len() * 2);
        }

        // Unknown network (e.g. a dev chain): every param must be overridden.
        assert!(resolve_state_diff(1337, &Upgrade18Overrides::default()).is_err());
        let diffs = resolve_state_diff(1337, &full_overrides()).unwrap();

        // The reserve seed lands on NativeAssetLiquidity (0x42..0029) and nowhere else.
        let reserve = address!("4200000000000000000000000000000000000029");
        for diff in &diffs {
            let expected =
                if diff.address == reserve { U256::from(1_000_000u64) } else { U256::ZERO };
            assert_eq!(diff.balance_increment, expected, "balance at {}", diff.address);
        }
    }

    /// `known_param` matches the artifact's per-network constants for *every* network
    /// and param the artifact knows — a codegen bug swapping networks or truncating a
    /// word must fail here, not at an activation boundary. Unresolved placeholders
    /// (`celoGasBridgeL1`, `nativeAssetLiquidityAmount`) must stay unknown everywhere.
    #[test]
    fn known_params_match_artifact_for_all_networks() {
        use celo_revm::constants::CELO_CHAOS_CHAIN_ID;

        let artifact: serde_json::Value =
            serde_json::from_str(include_str!("../../res/predeploys.json")).unwrap();
        let networks = [
            ("mainnet", CELO_MAINNET_CHAIN_ID),
            ("sepolia", CELO_SEPOLIA_CHAIN_ID),
            ("chaos", CELO_CHAOS_CHAIN_ID),
        ];
        let params = [
            Upgrade18Param::LiquidityControllerOwner,
            Upgrade18Param::CeloTokenL1,
            Upgrade18Param::CeloGasBridgeL1,
            Upgrade18Param::NativeAssetLiquidityAmount,
        ];
        for param in params {
            let artifact_values = &artifact["params"][param.name()];
            for (network, chain_id) in networks {
                // Mirrors the codegen filter: only `0x…` words in a per-network table
                // become `known_param` entries; unresolved placeholders are plain
                // strings and yield no entry on any network.
                let expected = artifact_values
                    .get(network)
                    .and_then(serde_json::Value::as_str)
                    .and_then(|s| s.strip_prefix("0x"))
                    .map(|hex| U256::from_str_radix(hex, 16).unwrap());
                assert_eq!(
                    super::super::upgrade18_data::known_param(chain_id, param),
                    expected,
                    "{network}: {}",
                    param.name()
                );
            }
        }
        // Sanity: the artifact actually ships the two known params for mainnet, so the
        // loop above is not vacuously comparing None to None.
        assert!(
            super::super::upgrade18_data::known_param(
                CELO_MAINNET_CHAIN_ID,
                Upgrade18Param::LiquidityControllerOwner
            )
            .is_some()
        );
    }

    /// The transition applies exactly once: 1-second blocks mean consecutive blocks can
    /// both satisfy a naive time-window check, and the reserve seed must not mint twice.
    /// After the migration is in state, the params are no longer needed at all.
    #[test]
    fn transition_applies_exactly_once_across_consecutive_blocks() {
        let reserve = address!("4200000000000000000000000000000000000029");
        let overrides = full_overrides();
        let seed = overrides.native_asset_liquidity_amount.unwrap();
        let chain_id = 1337;
        let fork = Some(100);

        let mut db = CacheDB::<EmptyDB>::default();

        // Pre-fork blocks: nothing happens.
        ensure_cgt_v2_predeploys(fork, &overrides, chain_id, 99, &mut db).unwrap();
        assert!(db.cache.accounts.is_empty());

        // The first active block applies the diff (even if it lands past the fork
        // second, e.g. after a sequencer outage).
        ensure_cgt_v2_predeploys(fork, &overrides, chain_id, 101, &mut db).unwrap();
        let marker = db.cache.accounts.get(&CELO_GAS_BRIDGE_L2).unwrap();
        assert_eq!(marker.info.code_hash, PROXY_SHELL_CODE_HASH);
        assert_eq!(db.cache.accounts.get(&reserve).unwrap().info.balance, seed);

        // Every later block sees the marker code and skips — no double mint. This also
        // holds without any params configured (post-migration nodes don't need them).
        ensure_cgt_v2_predeploys(fork, &overrides, chain_id, 102, &mut db).unwrap();
        ensure_cgt_v2_predeploys(fork, &Upgrade18Overrides::default(), chain_id, 103, &mut db)
            .unwrap();
        assert_eq!(
            db.cache.accounts.get(&reserve).unwrap().info.balance,
            seed,
            "reserve seed must be minted exactly once"
        );
    }

    /// An explicit zero is as fatal as an absent value: a zero reserve seed leaves bridged
    /// CELO unbacked and a zero address bricks the receiving contract, so a zeroed
    /// override (operator typo) must fail resolution, not reach state.
    #[test]
    fn zero_valued_params_are_rejected() {
        let mut overrides = full_overrides();
        overrides.native_asset_liquidity_amount = Some(U256::ZERO);
        let err = resolve_state_diff(1337, &overrides).unwrap_err();
        assert_eq!(err.param, Upgrade18Param::NativeAssetLiquidityAmount);

        let mut overrides = full_overrides();
        overrides.liquidity_controller_owner = Some(Address::ZERO);
        let err = resolve_state_diff(1337, &overrides).unwrap_err();
        assert_eq!(err.param, Upgrade18Param::LiquidityControllerOwner);
    }

    /// A scheduled activation with unresolvable params fails the boundary block loudly
    /// instead of skipping the migration.
    #[test]
    fn scheduled_fork_with_missing_params_halts() {
        let mut db = CacheDB::<EmptyDB>::default();
        let err =
            ensure_cgt_v2_predeploys(Some(100), &Upgrade18Overrides::default(), 1337, 100, &mut db)
                .unwrap_err();
        assert!(err.to_string().contains("unresolved"), "got: {err}");
        // Nothing was written: resolution fails before any commit. (The marker *read*
        // may leave an empty entry in CacheDB's cache — that is not state.)
        for (addr, account) in &db.cache.accounts {
            assert!(account.info.is_empty_code_hash(), "unexpected code at {addr}");
            assert_eq!(account.info.balance, U256::ZERO, "unexpected balance at {addr}");
        }
    }

    /// The transition's commits must survive revm's `State` transition/bundle pipeline —
    /// that is how reth persists block execution output. Regression test for the writes
    /// being visible in the bundle (and therefore in the block's state root and the next
    /// block's pre-state).
    #[test]
    fn transition_persists_through_revm_state_bundle() {
        use revm::database::{State, states::bundle_state::BundleRetention};

        let mut db = State::builder()
            .with_database(CacheDB::<EmptyDB>::default())
            .with_bundle_update()
            .build();
        ensure_cgt_v2_predeploys(Some(100), &full_overrides(), 1337, 100, &mut db).unwrap();
        db.merge_transitions(BundleRetention::Reverts);
        let bundle = db.take_bundle();

        let marker = bundle.account(&CELO_GAS_BRIDGE_L2).expect("marker account in bundle");
        assert_eq!(
            marker.info.as_ref().map(|i| i.code_hash),
            Some(PROXY_SHELL_CODE_HASH),
            "marker code must be in the bundle"
        );
        let reserve = bundle
            .account(&address!("4200000000000000000000000000000000000029"))
            .expect("reserve account in bundle");
        assert_eq!(
            reserve.info.as_ref().map(|i| i.balance),
            Some(full_overrides().native_asset_liquidity_amount.unwrap()),
            "reserve seed must be in the bundle"
        );
    }

    /// Unscheduled and pre-fork blocks are no-ops.
    #[test]
    fn ensure_is_noop_when_unscheduled_or_pre_fork() {
        let mut db = CacheDB::<EmptyDB>::default();
        ensure_cgt_v2_predeploys(None, &Upgrade18Overrides::default(), 42220, u64::MAX, &mut db)
            .unwrap();
        ensure_cgt_v2_predeploys(Some(100), &full_overrides(), 42220, 99, &mut db).unwrap();
        assert!(db.cache.accounts.is_empty());
    }

    /// Slots already at their target value are omitted from the reported changes, so the
    /// state-hook stream matches what the bundle layer keeps (its `is_changed` filter)
    /// and what lands in the state root.
    #[test]
    fn already_correct_slots_are_omitted_from_changes() {
        let controller = address!("420000000000000000000000000000000000002a");
        let owner_slot = U256::from(0x33);
        let overrides = full_overrides();
        let owner_word: U256 = overrides.liquidity_controller_owner.unwrap().into_word().into();

        let mut db = CacheDB::<EmptyDB>::default();
        db.insert_account_storage(controller, owner_slot, owner_word).unwrap();

        let changes = cgt_v2_state_changes(Some(100), &overrides, 1337, 100, &mut db)
            .unwrap()
            .expect("boundary block applies the transition");
        let account = changes.get(&controller).unwrap();
        assert!(
            !account.storage.contains_key(&owner_slot),
            "a slot already at its target value must not be reported"
        );
        assert!(!account.storage.is_empty(), "the remaining slots are still written");
    }

    /// Balance is an increment (mint semantics): pre-existing dust on the reserve
    /// survives the seed instead of being overwritten.
    #[test]
    fn reserve_seed_is_added_to_existing_balance() {
        let reserve = address!("4200000000000000000000000000000000000029");
        let overrides = full_overrides();
        let seed = overrides.native_asset_liquidity_amount.unwrap();
        let dust = U256::from(42);

        let mut db = CacheDB::<EmptyDB>::default();
        db.insert_account_info(
            reserve,
            revm::state::AccountInfo { balance: dust, ..Default::default() },
        );

        ensure_cgt_v2_predeploys(Some(100), &overrides, 1337, 100, &mut db).unwrap();
        assert_eq!(db.cache.accounts.get(&reserve).unwrap().info.balance, dust + seed);
    }

    /// Resolved storage writes carry the overrides: the bridge and token words land in
    /// `CeloGasBridgeL2`'s storage, the owner word in `LiquidityController`'s.
    #[test]
    fn overrides_flow_into_resolved_storage() {
        let overrides = full_overrides();
        let diffs = resolve_state_diff(1337, &overrides).unwrap();

        let storage_words = |addr: Address| -> Vec<U256> {
            diffs
                .iter()
                .find(|d| d.address == addr && !d.storage.is_empty())
                .unwrap()
                .storage
                .iter()
                .map(|(_, v)| *v)
                .collect()
        };

        let bridge_words = storage_words(CELO_GAS_BRIDGE_L2);
        let bridge_word: U256 = overrides.celo_gas_bridge_l1.unwrap().into_word().into();
        let token_word: U256 = overrides.celo_token_l1.unwrap().into_word().into();
        assert!(bridge_words.contains(&bridge_word), "otherBridge override applied");
        assert!(bridge_words.contains(&token_word), "celoTokenL1 override applied");

        let controller_words = storage_words(address!("420000000000000000000000000000000000002a"));
        let owner_word: U256 = overrides.liquidity_controller_owner.unwrap().into_word().into();
        assert!(controller_words.contains(&owner_word), "owner override applied");
    }
}
