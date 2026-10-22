//! CGTv2 irregular state changes staged at the activation deposit, before its setter call.
//! Bytecode and storage definitions come from the pinned predeploy artifact. The old
//! fee-vault settings come from live getter calls immediately before the signal.

use super::upgrade18_data::{
    PROXY_SHELL_BYTECODE, PROXY_SHELL_CODE_HASH, RAW_PREDEPLOYS, SlotValue,
};
use alloc::vec::Vec;
use alloy_evm::{
    Database,
    block::{BlockExecutionError, BlockValidationError},
};
use alloy_primitives::{Address, B256, Bytes, U256, address};
use celo_alloy_consensus::cgt_v2_params::CgtV2NetworkParams;
use revm::state::{Account, Bytecode, EvmState, EvmStorageSlot};

/// Code presence at this predeploy is the existing migration completion marker.
pub const CELO_GAS_BRIDGE_L2: Address = address!("4200000000000000000000000000000000001023");

/// A runtime value in the pinned migration artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upgrade18Param {
    /// The fixed network governance owner.
    LiquidityControllerOwner,
    /// The fixed L1 CELO token.
    CeloTokenL1,
    /// The fixed L1 bridge proxy.
    CeloGasBridgeL1,
    /// The checked reserve increment S-A.
    NativeAssetLiquidityAmount,
}
impl Upgrade18Param {
    /// JSON artifact key.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::LiquidityControllerOwner => "liquidityControllerOwner",
            Self::CeloTokenL1 => "celoTokenL1",
            Self::CeloGasBridgeL1 => "celoGasBridgeL1",
            Self::NativeAssetLiquidityAmount => "nativeAssetLiquidityAmount",
        }
    }
}
#[derive(Debug, Clone)]
struct ResolvedAccountDiff {
    address: Address,
    code: Option<(&'static [u8], B256)>,
    storage: Vec<(U256, U256)>,
    balance_increment: U256,
}
fn resolve_state_diff(params: &CgtV2NetworkParams, reserve_seed: U256) -> Vec<ResolvedAccountDiff> {
    let resolve = |param: Upgrade18Param| match param {
        Upgrade18Param::LiquidityControllerOwner => {
            params.liquidity_controller_owner.into_word().into()
        }
        Upgrade18Param::CeloTokenL1 => params.celo_token_l1.into_word().into(),
        Upgrade18Param::CeloGasBridgeL1 => params.celo_gas_bridge_l1.into_word().into(),
        Upgrade18Param::NativeAssetLiquidityAmount => reserve_seed,
    };
    let mut diffs = Vec::with_capacity(RAW_PREDEPLOYS.len() * 2);
    for predeploy in RAW_PREDEPLOYS {
        diffs.push(ResolvedAccountDiff {
            address: predeploy.impl_address,
            code: Some((predeploy.impl_code, predeploy.impl_code_hash)),
            storage: Vec::new(),
            balance_increment: U256::ZERO,
        });
        diffs.push(ResolvedAccountDiff {
            address: predeploy.proxy,
            code: Some((PROXY_SHELL_BYTECODE, PROXY_SHELL_CODE_HASH)),
            storage: predeploy
                .storage
                .iter()
                .map(|(slot, value)| {
                    (
                        *slot,
                        match value {
                            SlotValue::Literal(word) => *word,
                            SlotValue::Param(param) => resolve(*param),
                        },
                    )
                })
                .collect(),
            balance_increment: predeploy.balance.map(resolve).unwrap_or_default(),
        });
    }
    diffs
}

fn build_state_changes<DB: Database>(
    db: &mut DB,
    diffs: &[ResolvedAccountDiff],
) -> Result<EvmState, BlockExecutionError> {
    let mut changes = EvmState::default();
    for diff in diffs {
        let mut account: Account =
            db.basic(diff.address).map_err(BlockExecutionError::other)?.unwrap_or_default().into();
        if let Some((code, hash)) = diff.code {
            account.info.code_hash = hash;
            account.info.code = Some(Bytecode::new_raw(Bytes::from_static(code)));
        }
        account.info.balance = account
            .info
            .balance
            .checked_add(diff.balance_increment)
            .ok_or_else(|| BlockExecutionError::msg("CGTv2 reserve balance overflow"))?;
        for (slot, value) in &diff.storage {
            let original = db.storage(diff.address, *slot).map_err(BlockExecutionError::other)?;
            if original != *value {
                account.storage.insert(
                    *slot,
                    EvmStorageSlot::new_changed(original, *value, Default::default()),
                );
            }
        }
        account.mark_touch();
        changes.insert(diff.address, account);
    }
    Ok(changes)
}

/// Legacy vault settings captured immediately before replacing its implementation.
#[derive(Debug, Clone, Copy)]
pub struct FeeVaultConfig {
    /// Existing recipient returned by the old implementation.
    pub recipient: Address,
    /// Existing minimum returned by the old implementation.
    pub min_withdrawal_amount: U256,
    /// Existing L1/L2 network returned by the old implementation.
    pub withdrawal_network: u8,
}

pub(crate) const fn cgt_flag_slot() -> U256 {
    super::upgrade18_data::CGT_FLAG_SLOT
}

pub(crate) const fn fee_vault_initializer_slot() -> U256 {
    super::upgrade18_data::FEE_VAULT_INITIALIZER_SLOT
}

/// Prepare an uncommitted migration against state at the activation transaction boundary.
pub(crate) fn prepare_cgt_v2_migration<DB: Database>(
    db: &mut DB,
    params: &celo_alloy_consensus::cgt_v2_params::CgtV2NetworkParams,
    activation: &celo_alloy_consensus::cgt_v2::CgtV2Activation,
    vault: FeeVaultConfig,
) -> Result<EvmState, BlockExecutionError> {
    let amount = params.reserve_seed(activation.migrated_amount).ok_or_else(|| {
        BlockExecutionError::Validation(BlockValidationError::msg(
            "CGTv2 migrated amount exceeds total supply",
        ))
    })?;
    let mut diffs = resolve_state_diff(params, amount);
    for diff in &mut diffs {
        if diff.address == celo_alloy_consensus::cgt_v2::CGT_V2_L1_BLOCK {
            for (slot, value) in &mut diff.storage {
                if *slot == cgt_flag_slot() {
                    *value = U256::ZERO;
                }
            }
        }
        if diff.address == address!("4200000000000000000000000000000000000011") {
            diff.storage.push((
                super::upgrade18_data::FEE_VAULT_MIN_WITHDRAWAL_SLOT,
                vault.min_withdrawal_amount,
            ));
            let recipient_slot = super::upgrade18_data::FEE_VAULT_RECIPIENT_SLOT;
            let packed =
                db.storage(diff.address, recipient_slot).map_err(BlockExecutionError::other)?;
            let initialized = db
                .storage(diff.address, fee_vault_initializer_slot())
                .map_err(BlockExecutionError::other)?;
            diff.storage.push((
                recipient_slot,
                (packed & (U256::MAX << 168))
                    | U256::from_be_bytes(vault.recipient.into_word().0)
                    | (U256::from(vault.withdrawal_network) << 160),
            ));
            diff.storage.push((
                fee_vault_initializer_slot(),
                (initialized & (U256::MAX << 72)) | U256::from(1),
            ));
        }
    }
    build_state_changes(db, &diffs)
}

#[cfg(test)]
mod artifact_tests {
    use super::{super::upgrade18_data::RawPredeploy, *};
    use alloy_primitives::{b256, keccak256};
    const IMPL_SLOT: U256 = U256::from_be_bytes(
        b256!("360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc").0,
    );
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
}

#[cfg(test)]
mod signal_tests {
    use super::*;
    use celo_alloy_consensus::{cgt_v2::CgtV2Activation, cgt_v2_params::cgt_v2_params};
    use revm::{
        Database, DatabaseCommit,
        database::{CacheDB, EmptyDB},
        state::AccountInfo,
    };

    #[test]
    fn cgt_v2_migration_preserves_existing_state() {
        let mut db = CacheDB::<EmptyDB>::default();
        let reserve = address!("4200000000000000000000000000000000000029");
        let vault = address!("4200000000000000000000000000000000000011");
        db.insert_account_info(
            reserve,
            AccountInfo { balance: U256::from(7), nonce: 4, ..Default::default() },
        );
        db.insert_account_info(
            vault,
            AccountInfo { balance: U256::from(55), ..Default::default() },
        );
        db.insert_account_storage(vault, U256::ZERO, U256::from(91)).unwrap();
        let reserved_bits = U256::from(1) << 200;
        db.insert_account_storage(vault, U256::from(2), reserved_bits).unwrap();
        db.insert_account_storage(vault, fee_vault_initializer_slot(), reserved_bits).unwrap();
        let settings = FeeVaultConfig {
            recipient: Address::with_last_byte(0x99),
            min_withdrawal_amount: U256::from(17),
            withdrawal_network: 1,
        };
        let activation = CgtV2Activation {
            l1_origin_hash: B256::repeat_byte(0x11),
            migrated_amount: U256::from(600),
        };
        let changes =
            prepare_cgt_v2_migration(&mut db, &cgt_v2_params(1337).unwrap(), &activation, settings)
                .unwrap();
        assert_eq!(
            db.basic(reserve).unwrap().unwrap().balance,
            U256::from(7),
            "preparation cannot commit"
        );
        assert_eq!(changes[&reserve].info.balance, U256::from(407));
        assert_eq!(changes[&reserve].info.nonce, 4);
        assert_eq!(changes[&vault].info.balance, U256::from(55));
        db.commit(changes);
        assert_eq!(db.storage(vault, U256::ZERO).unwrap(), U256::from(91));
        assert_eq!(db.storage(vault, U256::from(1)).unwrap(), U256::from(17));
        assert_eq!(
            db.storage(vault, U256::from(2)).unwrap(),
            reserved_bits | U256::from(0x99) | (U256::from(1) << 160)
        );
        assert_eq!(
            db.storage(vault, fee_vault_initializer_slot()).unwrap(),
            reserved_bits | U256::from(1)
        );
        assert_eq!(
            db.storage(address!("4200000000000000000000000000000000000015"), cgt_flag_slot())
                .unwrap(),
            U256::ZERO
        );
    }

    #[test]
    fn cgt_v2_migration_accepts_zero_reserve_and_rejects_overflow() {
        let mut db = CacheDB::<EmptyDB>::default();
        let params = cgt_v2_params(1337).unwrap();
        let settings = FeeVaultConfig {
            recipient: Address::with_last_byte(1),
            min_withdrawal_amount: U256::ZERO,
            withdrawal_network: 0,
        };
        let mut a =
            CgtV2Activation { l1_origin_hash: B256::ZERO, migrated_amount: U256::from(1000) };
        assert!(prepare_cgt_v2_migration(&mut db, &params, &a, settings).is_ok());
        a.migrated_amount += U256::from(1);
        assert!(prepare_cgt_v2_migration(&mut db, &params, &a, settings).is_err());
        a.migrated_amount = U256::ZERO;
        db.insert_account_info(
            address!("4200000000000000000000000000000000000029"),
            AccountInfo { balance: U256::MAX, ..Default::default() },
        );
        assert!(prepare_cgt_v2_migration(&mut db, &params, &a, settings).is_err());
    }
}
