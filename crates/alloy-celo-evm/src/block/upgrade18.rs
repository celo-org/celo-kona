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
//! The state diff is *deliberately data*: [`UPGRADE18_STATE_DIFF`] must be generated from
//! the frozen `celo-org/optimism` `celo-contracts/v6.0.0` contracts by running
//! `L2Genesis.s.sol`'s `set*` routines (`setL1Block`, `setLiquidityController`,
//! `setNativeAssetLiquidity`, `setCeloGasBridgeL2`, …) and dumping the resulting allocs,
//! so that the post-fork state of the touched accounts is byte-identical to a fresh
//! v6.0.0 genesis. It covers (see the CGT v2 migration plan):
//!
//! | Address    | Contract                | Change |
//! |------------|-------------------------|--------|
//! | `0x42…0015`| `L1BlockCGT`            | new impl code + impl slot + gas-token config |
//! | `0x42…0016`| `L2ToL1MessagePasserCGT`| new impl code + impl slot |
//! | `0x42…0011`| `CeloSequencerFeeVault` | new impl code + impl slot |
//! | `0x42…0029`| `NativeAssetLiquidity`  | impl on the existing empty proxy shell + **CELO balance seed** |
//! | `0x42…002A`| `LiquidityController`   | impl on the existing empty proxy shell + initializer/minter storage |
//! | `0x42…1023`| `CeloGasBridgeL2`       | fresh proxy install (no code on-chain today) + initializer storage |
//!
//! The `0x42…0029` balance seed is the migration's single deliberate "mint native CELO"
//! step: it backs every CELO deposited through the legacy `OptimismPortal` escrow on L1
//! (research issue celo-blockchain-planning#1405 decides the exact amount and accounting
//! invariant). Balances are applied as *increments* so any dust force-sent to a predeploy
//! before the fork is preserved rather than burned.

use alloy_evm::Database;
use alloy_primitives::{Bytes, U256, address};
use revm::{
    DatabaseCommit,
    primitives::{Address, AddressMap},
    state::{Account, Bytecode, EvmStorageSlot},
};

/// Target state diff for one account touched by the Upgrade 18 transition.
///
/// Entries are generated from the frozen `celo-contracts/v6.0.0` `L2Genesis.s.sol` alloc
/// dump — do not hand-edit values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PredeployStateDiff {
    /// The account to write.
    pub address: Address,
    /// Runtime bytecode to install; `None` leaves the existing code untouched.
    pub code: Option<&'static [u8]>,
    /// Storage slots to write as `(slot, value)`. Slots not listed keep their current
    /// value.
    pub storage: &'static [(U256, U256)],
    /// Native CELO *added* to the account's balance. Non-zero only for the
    /// `NativeAssetLiquidity` reserve seed.
    pub balance_increment: U256,
    /// Nonce to set; `None` leaves the existing nonce untouched.
    pub nonce: Option<u64>,
}

/// The Upgrade 18 predeploy state diff.
///
/// TODO(celo-blockchain-planning#1409, #1405): populate from the pinned
/// `celo-contracts/v6.0.0` `L2Genesis.s.sol` alloc dump once the artifacts bundle exists
/// and the `NativeAssetLiquidity` seed amount is decided. Until then the transition is a
/// structural no-op (gating still evaluates, no state is written).
pub const UPGRADE18_STATE_DIFF: &[PredeployStateDiff] = &[];

/// The `CeloGasBridgeL2` predeploy proxy. Serves as the transition's completion marker:
/// it is the one touched address guaranteed to have **no code** before the fork (verified
/// in the live-chain audit, research issue celo-blockchain-planning#1404 — the address is
/// not CREATE/CREATE2-reachable, so nobody can deploy to it) and code after.
pub const CELO_GAS_BRIDGE_L2: Address = address!("0x4200000000000000000000000000000000001023");

/// Returns `true` iff Upgrade 18 is scheduled and active at `timestamp`.
pub(crate) const fn is_upgrade18_active(upgrade18_time: Option<u64>, timestamp: u64) -> bool {
    match upgrade18_time {
        Some(activation) => timestamp >= activation,
        None => false,
    }
}

/// Applies the CGT v2 predeploy state diff iff the executed block is the first
/// Upgrade-18-active block whose pre-state does not already contain the migration.
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
/// The cost is one account read per post-activation block.
pub(crate) fn ensure_cgt_v2_predeploys<DB>(
    upgrade18_time: Option<u64>,
    timestamp: u64,
    db: &mut DB,
) -> Result<(), DB::Error>
where
    DB: Database + DatabaseCommit,
{
    ensure_with_diff(upgrade18_time, timestamp, db, UPGRADE18_STATE_DIFF)
}

fn ensure_with_diff<DB>(
    upgrade18_time: Option<u64>,
    timestamp: u64,
    db: &mut DB,
    diff: &[PredeployStateDiff],
) -> Result<(), DB::Error>
where
    DB: Database + DatabaseCommit,
{
    if !is_upgrade18_active(upgrade18_time, timestamp) || diff.is_empty() {
        return Ok(());
    }
    let already_applied =
        db.basic(CELO_GAS_BRIDGE_L2)?.is_some_and(|account| !account.is_empty_code_hash());
    if already_applied {
        return Ok(());
    }
    apply_state_diff(db, diff)
}

/// Commits the given account diffs directly to `db`, bypassing the EVM.
fn apply_state_diff<DB>(db: &mut DB, diffs: &[PredeployStateDiff]) -> Result<(), DB::Error>
where
    DB: Database + DatabaseCommit,
{
    if diffs.is_empty() {
        return Ok(());
    }

    let mut changes: AddressMap<Account> = AddressMap::default();
    for diff in diffs {
        // Start from the live account: proxies like 0x42…0015 already exist and keep
        // their untouched slots; fresh installs like 0x42…1023 start from the default.
        let mut info = db.basic(diff.address)?.unwrap_or_default();

        if let Some(code) = diff.code {
            let bytecode = Bytecode::new_raw(Bytes::from_static(code));
            info.code_hash = bytecode.hash_slow();
            info.code = Some(bytecode);
        }
        if let Some(nonce) = diff.nonce {
            info.nonce = nonce;
        }
        info.balance = info.balance.saturating_add(diff.balance_increment);

        let mut account: Account = info.into();
        for (slot, value) in diff.storage {
            let original = db.storage(diff.address, *slot)?;
            account
                .storage
                .insert(*slot, EvmStorageSlot::new_changed(original, *value, Default::default()));
        }
        account.mark_touch();
        changes.insert(diff.address, account);
    }

    db.commit(changes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, keccak256};
    use revm::database::{CacheDB, EmptyDB};

    #[test]
    fn activation_boundary_semantics() {
        assert!(!is_upgrade18_active(None, 0));
        assert!(!is_upgrade18_active(None, u64::MAX));
        assert!(!is_upgrade18_active(Some(100), 99));
        assert!(is_upgrade18_active(Some(100), 100));
        assert!(is_upgrade18_active(Some(100), 101));
    }

    /// Every non-empty state diff must install code at the [`CELO_GAS_BRIDGE_L2`] marker,
    /// otherwise the transition would re-apply (and re-mint the reserve seed) on every
    /// post-activation block. Guards the generated table, not the current placeholder.
    #[test]
    fn state_diff_installs_the_completion_marker() {
        if UPGRADE18_STATE_DIFF.is_empty() {
            return;
        }
        assert!(
            UPGRADE18_STATE_DIFF
                .iter()
                .any(|d| d.address == CELO_GAS_BRIDGE_L2 && d.code.is_some_and(|c| !c.is_empty())),
            "UPGRADE18_STATE_DIFF must install code at CELO_GAS_BRIDGE_L2 (0x42..1023); \
             without it the exactly-once marker never trips",
        );
    }

    /// The transition applies exactly once: 1-second blocks mean consecutive blocks can
    /// both satisfy a naive time-window check, and the reserve seed must not mint twice.
    #[test]
    fn transition_applies_exactly_once_across_consecutive_blocks() {
        const CODE: &[u8] = &[0x60, 0x00, 0x60, 0x00, 0xfd];
        let seed = U256::from(1_000_000u64);
        let reserve = address!("4200000000000000000000000000000000000029");
        let diff = [
            PredeployStateDiff {
                address: CELO_GAS_BRIDGE_L2,
                code: Some(CODE),
                storage: &[],
                balance_increment: U256::ZERO,
                nonce: None,
            },
            PredeployStateDiff {
                address: reserve,
                code: None,
                storage: &[],
                balance_increment: seed,
                nonce: None,
            },
        ];

        let mut db = CacheDB::<EmptyDB>::default();
        let fork = Some(100);

        // Pre-fork blocks: nothing happens.
        ensure_with_diff(fork, 99, &mut db, &diff).unwrap();
        assert!(db.cache.accounts.is_empty());

        // The first active block applies the diff (even if it lands past the fork
        // second, e.g. after a sequencer outage).
        ensure_with_diff(fork, 101, &mut db, &diff).unwrap();
        let balance = db.cache.accounts.get(&reserve).unwrap().info.balance;
        assert_eq!(balance, seed);

        // Every later block sees the marker code and skips — no double mint.
        ensure_with_diff(fork, 102, &mut db, &diff).unwrap();
        ensure_with_diff(fork, 103, &mut db, &diff).unwrap();
        let balance = db.cache.accounts.get(&reserve).unwrap().info.balance;
        assert_eq!(balance, seed, "reserve seed must be minted exactly once");
    }

    #[test]
    fn apply_state_diff_writes_code_storage_and_mints_balance() {
        const CODE: &[u8] = &[0x60, 0x00, 0x60, 0x00, 0xfd]; // PUSH0 PUSH0 REVERT
        const SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);
        const VALUE: U256 = U256::from_limbs([0xdead_beef, 0, 0, 0]);
        const STORAGE: &[(U256, U256)] = &[(SLOT, VALUE)];
        let addr = address!("4200000000000000000000000000000000000029");
        let seed = U256::from(10).pow(U256::from(27)); // 1B CELO in wei

        let diff = [PredeployStateDiff {
            address: addr,
            code: Some(CODE),
            storage: STORAGE,
            balance_increment: seed,
            nonce: Some(1),
        }];

        let mut db = CacheDB::<EmptyDB>::default();
        // Pre-fund with "dust" to pin the increment (mint) semantics.
        let dust = U256::from(42);
        db.insert_account_info(
            addr,
            revm::state::AccountInfo { balance: dust, ..Default::default() },
        );

        apply_state_diff(&mut db, &diff).unwrap();

        let account = db.cache.accounts.get(&addr).unwrap();
        let info = &account.info;
        assert_eq!(info.balance, dust + seed, "seed must be added, not overwrite dust");
        assert_eq!(info.nonce, 1);
        assert_eq!(info.code_hash, keccak256(CODE));
        assert_eq!(info.code.as_ref().unwrap().original_byte_slice(), CODE);
        assert_eq!(account.storage.get(&SLOT), Some(&VALUE));
    }

    #[test]
    fn ensure_is_noop_when_unscheduled_or_table_empty() {
        let mut db = CacheDB::<EmptyDB>::default();
        // Unscheduled, pre-fork, and (currently) an empty static table are all no-ops.
        ensure_cgt_v2_predeploys(None, u64::MAX, &mut db).unwrap();
        ensure_cgt_v2_predeploys(Some(100), 98, &mut db).unwrap();
        ensure_cgt_v2_predeploys(Some(100), 100, &mut db).unwrap();
        assert!(db.cache.accounts.is_empty());
    }
}
