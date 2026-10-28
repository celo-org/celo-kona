use super::*;
use alloy_consensus::{Sealable, transaction::Recovered};
use alloy_evm::{
    Evm, EvmEnv, EvmFactory,
    block::{BlockExecutor, BlockExecutorFactory, CommitChanges},
};
use alloy_op_evm::OpBlockExecutionCtx;
use alloy_op_hardforks::OpChainHardforks;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address, keccak256};
use celo_alloy_consensus::{
    CeloTxEnvelope,
    cgt_v2_group::{
        CGT_V2_DEPOSITOR, CGT_V2_FLAG_SLOT, CGT_V2_L1_BLOCK, CGT_V2_RESERVE, CGT_V2_STATIC_STEPS,
        CGT_V2_STEPS, CgtV2UpgradeGroup,
    },
};
use op_alloy_consensus::TxDeposit;
use op_revm::OpSpecId;
use revm::{
    Database,
    database::{CacheDB, EmptyDB, State},
    state::{AccountInfo, Bytecode},
};
const VAULT: Address = address!("4200000000000000000000000000000000000011");
fn database() -> CacheDB<EmptyDB> {
    let state: serde_json::Value =
        serde_json::from_str(include_str!("cgt_v2_prestate.json")).unwrap();
    let mut db = CacheDB::<EmptyDB>::default();
    for (address, account) in state.as_object().unwrap() {
        let address = address.parse::<Address>().unwrap();
        let code: Bytes = account["code"].as_str().unwrap().parse().unwrap();
        db.insert_account_info(
            address,
            AccountInfo {
                balance: account["balance"].as_str().unwrap().parse().unwrap(),
                code_hash: keccak256(&code),
                code: Some(Bytecode::new_raw(code)),
                ..Default::default()
            },
        );
        for (slot, value) in account["storage"].as_object().unwrap() {
            db.insert_account_storage(
                address,
                slot.parse().unwrap(),
                value.as_str().unwrap().parse().unwrap(),
            )
            .unwrap();
        }
    }
    db
}
fn env() -> EvmEnv<OpSpecId> {
    let mut env: EvmEnv<OpSpecId> = EvmEnv::default();
    env.cfg_env.chain_id = 1337;
    env.cfg_env.spec = OpSpecId::ISTHMUS;
    env.block_env.number = U256::from(1);
    env.block_env.timestamp = U256::from(1_750_000_000u64);
    env.block_env.gas_limit = 70_000_000;
    env
}
fn factory() -> CeloBlockExecutorFactory<CeloAlloyReceiptBuilder, OpChainHardforks> {
    CeloBlockExecutorFactory::new(OpChainHardforks::op_mainnet(), crate::CeloEvmFactory::default())
}
fn recovered(tx: TxDeposit) -> Recovered<CeloTxEnvelope> {
    let from = tx.from;
    Recovered::new_unchecked(CeloTxEnvelope::Deposit(tx.seal_slow()), from)
}
fn info() -> Recovered<CeloTxEnvelope> {
    let mut data = keccak256(
        b"setL1BlockValues(uint64,uint64,uint256,bytes32,uint64,bytes32,uint256,uint256)",
    )[..4]
        .to_vec();
    data.extend_from_slice(&[0; 256]);
    recovered(TxDeposit {
        from: CGT_V2_DEPOSITOR,
        to: TxKind::Call(CGT_V2_L1_BLOCK),
        gas_limit: 200_000,
        input: data.into(),
        source_hash: B256::repeat_byte(0x22),
        ..Default::default()
    })
}
fn group() -> Vec<Recovered<CeloTxEnvelope>> {
    CgtV2UpgradeGroup::deposits(1337, U256::from(600)).unwrap().into_iter().map(recovered).collect()
}
#[test]
fn cgt_v2_already_initialized_predeploy_rejects_group() {
    let initializer_v5 = U256::from_be_bytes(
        alloy_primitives::b256!("f0c57e16840df040f15088dc2f81fe391c3923bec73e23a9662efc9c229c6a00")
            .0,
    );
    for (target, slot) in [
        (address!("420000000000000000000000000000000000002a"), U256::ZERO),
        (address!("42000000000000000000000000000000000007fe"), U256::ZERO),
        (VAULT, initializer_v5),
    ] {
        let f = factory();
        let mut base = database();
        base.insert_account_storage(target, slot, U256::from(1)).unwrap();
        let mut db = State::builder().with_database(base).build();
        let mut executor = f.create_executor(
            f.evm_factory().create_evm(&mut db, env()),
            OpBlockExecutionCtx {
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
        );
        executor.apply_pre_execution_changes().unwrap();
        executor.execute_transaction(&info()).unwrap();
        let result = group().iter().try_for_each(|tx| executor.execute_transaction(tx).map(|_| ()));
        assert!(result.is_err(), "migration reset an existing initializer at {target}");
    }
}

#[test]
fn cgt_v2_real_group_executes_and_preserves_vault() {
    let f = factory();
    let mut db = State::builder().with_database(database()).with_bundle_update().build();
    let mut e = f.create_executor(
        f.evm_factory().create_evm(&mut db, env()),
        OpBlockExecutionCtx { parent_beacon_block_root: Some(B256::ZERO), ..Default::default() },
    );
    e.apply_pre_execution_changes().unwrap();
    e.execute_transaction(&info()).unwrap();
    assert_eq!(
        e.evm_mut()
            .db_mut()
            .storage(CGT_V2_L1_BLOCK, U256::from_be_bytes(CGT_V2_FLAG_SLOT.0))
            .unwrap(),
        U256::ZERO
    );
    for (i, tx) in group().iter().enumerate() {
        e.execute_transaction(tx).unwrap_or_else(|err| panic!("group step {i}: {err:?}"));
        assert!(e.receipts().last().unwrap().status(), "failed group step {i}");
    }
    let db = e.evm_mut().db_mut();
    assert_eq!(db.basic(CGT_V2_RESERVE).unwrap().unwrap().balance, U256::from(400));
    assert_eq!(
        db.storage(CGT_V2_L1_BLOCK, U256::from_be_bytes(CGT_V2_FLAG_SLOT.0)).unwrap(),
        U256::from(1)
    );
    assert_eq!(db.basic(VAULT).unwrap().unwrap().balance, U256::from(41));
    assert_eq!(db.storage(VAULT, U256::ZERO).unwrap(), U256::from(73));
    assert_eq!(
        db.storage(
            address!("4200000000000000000000000000000000000018"),
            "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc".parse().unwrap()
        )
        .unwrap(),
        U256::from(0x1001),
        "CGTv2 must retain the legacy ProxyAdmin implementation"
    );
    assert_eq!(e.receipts().len(), CGT_V2_STEPS + 1);
    assert!(e.finish().is_ok());
}
#[test]
fn cgt_v2_truncated_group_rejects_block() {
    for length in 1..CGT_V2_STEPS {
        let f = factory();
        let mut db = State::builder().with_database(database()).build();
        let mut e = f.create_executor(
            f.evm_factory().create_evm(&mut db, env()),
            OpBlockExecutionCtx {
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
        );
        e.apply_pre_execution_changes().unwrap();
        e.execute_transaction(&info()).unwrap();
        for tx in &group()[..length] {
            e.execute_transaction(tx).unwrap();
        }
        assert!(e.finish().is_err(), "accepted {length} of {CGT_V2_STEPS} required steps");
    }
}
#[test]
fn cgt_v2_reordered_or_modified_group_rejects_block() {
    for malformed in [false, true] {
        let f = factory();
        let mut db = State::builder().with_database(database()).build();
        let mut e = f.create_executor(
            f.evm_factory().create_evm(&mut db, env()),
            OpBlockExecutionCtx {
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
        );
        e.apply_pre_execution_changes().unwrap();
        e.execute_transaction(&info()).unwrap();
        let mut txs = CgtV2UpgradeGroup::deposits(1337, U256::from(600)).unwrap();
        if malformed {
            txs[0].gas_limit += 1;
        } else {
            txs.swap(0, 1);
        }
        assert!(e.execute_transaction(&recovered(txs.remove(0))).is_err());
    }
}
#[test]
fn cgt_v2_discarded_first_step_has_no_effect() {
    let f = factory();
    let mut db = State::builder().with_database(database()).build();
    let mut e = f.create_executor(
        f.evm_factory().create_evm(&mut db, env()),
        OpBlockExecutionCtx { parent_beacon_block_root: Some(B256::ZERO), ..Default::default() },
    );
    e.apply_pre_execution_changes().unwrap();
    e.execute_transaction(&info()).unwrap();
    let txs = group();
    e.execute_transaction_with_commit_condition(&txs[0], |_| CommitChanges::No).unwrap();
    assert_eq!(e.receipts().len(), 1);
    for tx in &txs {
        e.execute_transaction(tx).unwrap();
    }
    assert!(e.finish().is_ok());
}

fn user_transaction() -> Recovered<CeloTxEnvelope> {
    use alloy_consensus::{SignableTransaction, TxLegacy};
    use alloy_primitives::Signature;
    Recovered::new_unchecked(
        CeloTxEnvelope::Legacy(
            TxLegacy {
                gas_limit: 21_000,
                to: TxKind::Call(Address::with_last_byte(0x88)),
                ..Default::default()
            }
            .into_signed(Signature::new(U256::ZERO, U256::ZERO, false)),
        ),
        Address::with_last_byte(0x88),
    )
}

#[test]
fn cgt_v2_users_cannot_precede_or_follow_group() {
    for user_first in [false, true] {
        let f = factory();
        let mut db = State::builder().with_database(database()).build();
        let mut e = f.create_executor(
            f.evm_factory().create_evm(&mut db, env()),
            OpBlockExecutionCtx {
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
        );
        e.apply_pre_execution_changes().unwrap();
        e.execute_transaction(&info()).unwrap();
        let error = if user_first {
            e.execute_transaction(&user_transaction()).unwrap();
            e.execute_transaction(&group()[0]).unwrap_err()
        } else {
            for tx in group() {
                e.execute_transaction(&tx).unwrap();
            }
            e.execute_transaction(&user_transaction()).unwrap_err()
        };
        assert!(error.to_string().contains("unexpected non-deposit transactions"), "{error}");
    }
}

#[test]
fn cgt_v2_discarded_user_or_upgrade_does_not_change_group_policy() {
    for discard_group in [false, true] {
        let f = factory();
        let mut db = State::builder().with_database(database()).build();
        let mut e = f.create_executor(
            f.evm_factory().create_evm(&mut db, env()),
            OpBlockExecutionCtx {
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
        );
        e.apply_pre_execution_changes().unwrap();
        e.execute_transaction(&info()).unwrap();
        let tx = if discard_group { group().remove(0) } else { user_transaction() };
        assert!(
            e.execute_transaction_with_commit_condition(&tx, |_| CommitChanges::No)
                .unwrap()
                .is_none()
        );
        if discard_group {
            e.execute_transaction(&user_transaction()).unwrap();
        } else {
            for tx in group() {
                e.execute_transaction(&tx).unwrap();
            }
        }
        assert!(e.finish().is_ok());
    }
}

#[test]
fn cgt_v2_origin_deposit_must_precede_group() {
    let f = factory();
    let mut db = State::builder().with_database(database()).build();
    let mut e = f.create_executor(
        f.evm_factory().create_evm(&mut db, env()),
        OpBlockExecutionCtx { parent_beacon_block_root: Some(B256::ZERO), ..Default::default() },
    );
    e.apply_pre_execution_changes().unwrap();
    e.execute_transaction(&info()).unwrap();
    let recipient = Address::with_last_byte(0x77);
    let deposit = recovered(TxDeposit {
        from: Address::ZERO,
        to: TxKind::Call(recipient),
        mint: 5,
        value: U256::from(5),
        gas_limit: 100_000,
        source_hash: B256::repeat_byte(0x33),
        ..Default::default()
    });
    e.execute_transaction(&deposit).unwrap();
    assert_eq!(e.evm_mut().db_mut().basic(recipient).unwrap().unwrap().balance, U256::from(5));
    for tx in group() {
        e.execute_transaction(&tx).unwrap();
    }
    assert!(
        e.execute_transaction(&deposit)
            .unwrap_err()
            .to_string()
            .contains("origin deposits must precede")
    );
}

#[test]
fn cgt_v2_initializer_or_setter_failure_rejects_required_group() {
    use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
    use revm::precompile::{PrecompileHalt, PrecompileOutput};
    struct FailureCase {
        step: usize,
        target: Address,
    }
    for FailureCase { step, target } in [
        FailureCase { step: 9, target: address!("420000000000000000000000000000000000002a") },
        FailureCase { step: 10, target: address!("42000000000000000000000000000000000007fe") },
        FailureCase { step: 11, target: VAULT },
        FailureCase { step: CGT_V2_STATIC_STEPS - 1, target: CGT_V2_L1_BLOCK },
    ] {
        for revert in [false, true] {
            let f = factory();
            let mut db = State::builder().with_database(database()).build();
            let mut e = f.create_executor(
                f.evm_factory().create_evm(&mut db, env()),
                OpBlockExecutionCtx {
                    parent_beacon_block_root: Some(B256::ZERO),
                    ..Default::default()
                },
            );
            e.apply_pre_execution_changes().unwrap();
            e.execute_transaction(&info()).unwrap();
            let txs = group();
            for tx in &txs[..step] {
                e.execute_transaction(tx).unwrap();
            }
            e.evm_mut().components_mut().2.extend_precompiles([(
                target,
                DynPrecompile::from(move |_input: PrecompileInput<'_>| {
                    Ok(if revert {
                        PrecompileOutput::revert(0, Bytes::new(), 0)
                    } else {
                        PrecompileOutput::halt(PrecompileHalt::OutOfGas, 0)
                    })
                }),
            )]);
            let error = e.execute_transaction(&txs[step]).unwrap_err();
            assert!(error.as_validation().is_some(), "{error}");
            assert_eq!(e.receipts().len(), step + 1);
            assert!(e.finish().is_err());
        }
    }
}
#[test]
fn cgt_v2_next_block_resumes_users_and_rejects_reactivation() {
    let f = factory();
    let mut db = State::builder().with_database(database()).build();
    let ctx =
        || OpBlockExecutionCtx { parent_beacon_block_root: Some(B256::ZERO), ..Default::default() };
    let mut e = f.create_executor(f.evm_factory().create_evm(&mut db, env()), ctx());
    e.apply_pre_execution_changes().unwrap();
    e.execute_transaction(&info()).unwrap();
    for tx in group() {
        e.execute_transaction(&tx).unwrap();
    }
    drop(e.finish().unwrap());
    let mut e = f.create_executor(f.evm_factory().create_evm(&mut db, env()), ctx());
    e.apply_pre_execution_changes().unwrap();
    e.execute_transaction(&info()).unwrap();
    assert!(e.execute_transaction(&group()[0]).unwrap_err().to_string().contains("cannot repeat"));
    e.execute_transaction(&user_transaction()).unwrap();
    assert!(e.finish().is_ok());
}

#[test]
fn cgt_v2_pinned_vault_settings_do_not_call_legacy_getters() {
    for code in ["60006000fd", "fe", "00"] {
        let mut base = database();
        let code: Bytes = alloy_primitives::hex::decode(code).unwrap().into();
        base.insert_account_info(
            Address::from_word(U256::from(0x1003).into()),
            AccountInfo {
                code_hash: keccak256(&code),
                code: Some(Bytecode::new_raw(code)),
                ..Default::default()
            },
        );
        let impl_slot: U256 =
            "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc".parse().unwrap();
        base.insert_account_storage(VAULT, impl_slot, U256::from(0x1003)).unwrap();
        let f = factory();
        let mut db = State::builder().with_database(base).build();
        let mut e = f.create_executor(
            f.evm_factory().create_evm(&mut db, env()),
            OpBlockExecutionCtx {
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
        );
        e.apply_pre_execution_changes().unwrap();
        e.execute_transaction(&info()).unwrap();
        for tx in group() {
            e.execute_transaction(&tx).unwrap();
        }
        assert_eq!(e.evm_mut().db_mut().storage(VAULT, U256::ZERO).unwrap(), U256::from(73));
        assert!(e.finish().is_ok());
    }
}

#[test]
fn cgt_v2_zero_reserve_and_existing_reserve_balance() {
    for migrated in [600, 1000] {
        let mut base = database();
        base.cache.accounts.get_mut(&CGT_V2_RESERVE).unwrap().info.balance = U256::from(7);
        let f = factory();
        let mut db = State::builder().with_database(base).build();
        let mut e = f.create_executor(
            f.evm_factory().create_evm(&mut db, env()),
            OpBlockExecutionCtx {
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
        );
        e.apply_pre_execution_changes().unwrap();
        e.execute_transaction(&info()).unwrap();
        for tx in CgtV2UpgradeGroup::deposits(1337, U256::from(migrated)).unwrap() {
            e.execute_transaction(&recovered(tx)).unwrap();
        }
        assert_eq!(
            e.evm_mut().db_mut().basic(CGT_V2_RESERVE).unwrap().unwrap().balance,
            U256::from(1007 - migrated)
        );
        assert!(e.finish().is_ok());
    }
}

#[derive(Debug)]
struct MigrationDbError;
impl core::fmt::Display for MigrationDbError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("injected migration DB error")
    }
}
impl core::error::Error for MigrationDbError {}
impl revm::database_interface::DBErrorMarker for MigrationDbError {}
#[derive(Debug)]
struct FailingDatabase(CacheDB<EmptyDB>);
impl Database for FailingDatabase {
    type Error = MigrationDbError;
    fn basic(&mut self, a: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.0.basic(a).map_err(|_| MigrationDbError)
    }
    fn code_by_hash(&mut self, h: B256) -> Result<Bytecode, Self::Error> {
        self.0.code_by_hash(h).map_err(|_| MigrationDbError)
    }
    fn storage(&mut self, a: Address, s: U256) -> Result<U256, Self::Error> {
        if a == VAULT
            && s == "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc"
                .parse::<U256>()
                .unwrap()
        {
            return Err(MigrationDbError);
        }
        self.0.storage(a, s).map_err(|_| MigrationDbError)
    }
    fn block_hash(&mut self, n: u64) -> Result<B256, Self::Error> {
        self.0.block_hash(n).map_err(|_| MigrationDbError)
    }
}

#[test]
fn cgt_v2_direct_upgrade_preserves_database_error() {
    let f = factory();
    let mut db = State::builder().with_database(FailingDatabase(database())).build();
    let mut e = f.create_executor(
        f.evm_factory().create_evm(&mut db, env()),
        OpBlockExecutionCtx { parent_beacon_block_root: Some(B256::ZERO), ..Default::default() },
    );
    e.apply_pre_execution_changes().unwrap();
    e.execute_transaction(&info()).unwrap();
    let txs = group();
    let vault_upgrade_step = 11;
    for tx in &txs[..vault_upgrade_step] {
        e.execute_transaction(tx).unwrap();
    }
    let error = e.execute_transaction(&txs[vault_upgrade_step]).unwrap_err();
    assert!(error.as_internal().is_some(), "database error became transaction invalidity: {error}");
    assert!(error.to_string().contains("injected migration DB error"), "{error}");
    assert_eq!(e.receipts().len(), vault_upgrade_step + 1);
}

#[test]
fn cgt_v2_migrated_vault_getters_initializer_and_native_withdrawal() {
    let f = factory();
    let mut db = State::builder().with_database(database()).build();
    let ctx =
        || OpBlockExecutionCtx { parent_beacon_block_root: Some(B256::ZERO), ..Default::default() };
    let mut e = f.create_executor(f.evm_factory().create_evm(&mut db, env()), ctx());
    e.apply_pre_execution_changes().unwrap();
    e.execute_transaction(&info()).unwrap();
    for tx in group() {
        e.execute_transaction(&tx).unwrap();
    }
    drop(e.finish().unwrap());
    let mut e = f.create_executor(f.evm_factory().create_evm(&mut db, env()), ctx());
    e.apply_pre_execution_changes().unwrap();
    e.execute_transaction(&info()).unwrap();
    let call = |input: Bytes| {
        recovered(TxDeposit {
            from: Address::ZERO,
            to: TxKind::Call(VAULT),
            gas_limit: 200_000,
            source_hash: keccak256(&input),
            input,
            ..Default::default()
        })
    };
    for (signature, value) in [
        ("recipient()", U256::from(0xdd)),
        ("minWithdrawalAmount()", U256::from(23)),
        ("withdrawalNetwork()", U256::from(1)),
    ] {
        let output = e
            .execute_transaction_without_commit(&call(Bytes::copy_from_slice(
                &keccak256(signature)[..4],
            )))
            .unwrap();
        assert_eq!(
            output.inner.result.result.output().unwrap().as_ref(),
            value.to_be_bytes::<32>()
        );
    }
    let mut init = keccak256(b"initialize(address,uint256,uint8)")[..4].to_vec();
    init.extend_from_slice(&[0; 96]);
    assert!(
        !e.execute_transaction_without_commit(&call(init.into()))
            .unwrap()
            .inner
            .result
            .result
            .is_success()
    );
    let output = e
        .execute_transaction_without_commit(&call(Bytes::copy_from_slice(
            &keccak256(b"withdraw()")[..4],
        )))
        .unwrap();
    assert!(output.inner.result.result.is_success());
    e.commit_transaction(output);
    assert_eq!(e.evm_mut().db_mut().basic(VAULT).unwrap().unwrap().balance, U256::ZERO);
    assert_eq!(
        e.evm_mut().db_mut().basic(Address::with_last_byte(0xdd)).unwrap().unwrap().balance,
        U256::from(41)
    );
    assert_eq!(e.evm_mut().db_mut().storage(VAULT, U256::ZERO).unwrap(), U256::from(114));
}

fn token_fixture_code() -> Bytes {
    let mut code = alloy_primitives::hex::decode("60003560e01c").unwrap();
    let mut jumps = Vec::new();
    for signature in ["balanceOf(address)", "transfer(address,uint256)"] {
        code.extend_from_slice(&[0x80, 0x63]);
        code.extend_from_slice(&keccak256(signature)[..4]);
        code.extend_from_slice(&[0x14, 0x61, 0, 0, 0x57]);
        jumps.push(code.len() - 3);
    }
    code.extend_from_slice(&[0x60, 0, 0x60, 0, 0xfd]);
    for (pos, body) in jumps
        .into_iter()
        .zip(["60005460005260206000f3", "602435600154016001556000600055600160005260206000f3"])
    {
        let dest = (code.len() as u16).to_be_bytes();
        code[pos..pos + 2].copy_from_slice(&dest);
        code.push(0x5b);
        code.extend_from_slice(&alloy_primitives::hex::decode(body).unwrap());
    }
    code.into()
}

#[test]
fn cgt_v2_installed_vault_preserves_and_withdraws_tokens() {
    let mut base = database();
    let token = Address::with_last_byte(0xee);
    for (addr, code) in [
        (
            Address::with_last_byte(0xdd),
            alloy_primitives::hex::decode("6001600052600060205260406000f3").unwrap().into(),
        ),
        (
            address!("000000000000000000000000000000000000ce10"),
            alloy_primitives::hex::decode("60dd60005260206000f3").unwrap().into(),
        ),
        (token, token_fixture_code()),
    ] {
        base.insert_account_info(
            addr,
            AccountInfo {
                code_hash: keccak256(&code),
                code: Some(Bytecode::new_raw(code)),
                ..Default::default()
            },
        );
    }
    base.insert_account_storage(token, U256::ZERO, U256::from(100)).unwrap();
    base.insert_account_storage(token, U256::from(1), U256::from(5)).unwrap();
    let f = factory();
    let mut db = State::builder().with_database(base).build();
    let mut e = f.create_executor(
        f.evm_factory().create_evm(&mut db, env()),
        OpBlockExecutionCtx { parent_beacon_block_root: Some(B256::ZERO), ..Default::default() },
    );
    e.apply_pre_execution_changes().unwrap();
    e.execute_transaction(&info()).unwrap();
    for tx in group() {
        e.execute_transaction(&tx).unwrap();
    }
    drop(e.finish().unwrap());
    let mut e = f.create_executor(
        f.evm_factory().create_evm(&mut db, env()),
        OpBlockExecutionCtx { parent_beacon_block_root: Some(B256::ZERO), ..Default::default() },
    );
    e.apply_pre_execution_changes().unwrap();
    e.execute_transaction(&info()).unwrap();
    assert_eq!(e.evm_mut().db_mut().storage(token, U256::ZERO).unwrap(), U256::from(100));
    let mut data = keccak256(b"withdrawToken(address)")[..4].to_vec();
    data.extend_from_slice(token.into_word().as_slice());
    let tx = recovered(TxDeposit {
        from: Address::ZERO,
        to: TxKind::Call(VAULT),
        gas_limit: 300_000,
        input: data.into(),
        source_hash: B256::repeat_byte(0x77),
        ..Default::default()
    });
    let output = e.execute_transaction_without_commit(&tx).unwrap();
    assert!(output.inner.result.result.is_success(), "{:?}", output.inner.result.result);
    e.commit_transaction(output);
    assert_eq!(e.evm_mut().db_mut().storage(token, U256::ZERO).unwrap(), U256::ZERO);
    assert_eq!(e.evm_mut().db_mut().storage(token, U256::from(1)).unwrap(), U256::from(105));
    let mut mapping = [0; 64];
    mapping[..32].copy_from_slice(token.into_word().as_slice());
    mapping[63] = 49;
    assert_eq!(
        e.evm_mut().db_mut().storage(VAULT, U256::from_be_bytes(keccak256(mapping).0)).unwrap(),
        U256::from(100)
    );
    assert_eq!(e.evm_mut().db_mut().basic(VAULT).unwrap().unwrap().balance, U256::from(41));
    assert_eq!(e.evm_mut().db_mut().storage(VAULT, U256::ZERO).unwrap(), U256::from(73));
}

#[test]
fn cgt_v2_failed_funding_discards_entire_block_candidate() {
    use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
    use revm::precompile::PrecompileOutput;
    let mut base = database();
    {
        let f = factory();
        let mut candidate = State::builder().with_database(&mut base).build();
        let mut e = f.create_executor(
            f.evm_factory().create_evm(&mut candidate, env()),
            OpBlockExecutionCtx {
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
        );
        e.apply_pre_execution_changes().unwrap();
        e.execute_transaction(&info()).unwrap();
        let txs = group();
        for tx in &txs[..CGT_V2_STATIC_STEPS] {
            e.execute_transaction(tx).unwrap();
        }
        e.evm_mut().components_mut().2.extend_precompiles([(
            CGT_V2_RESERVE,
            DynPrecompile::from(|_input: PrecompileInput<'_>| {
                Ok(PrecompileOutput::revert(0, Bytes::new(), 0))
            }),
        )]);
        assert!(
            e.execute_transaction(&txs[CGT_V2_STATIC_STEPS]).unwrap_err().as_validation().is_some()
        );
        assert!(e.finish().is_err());
        // The block builder drops its candidate on this error instead of publishing its bundle.
    }
    assert_eq!(
        base.storage(CGT_V2_L1_BLOCK, U256::from_be_bytes(CGT_V2_FLAG_SLOT.0)).unwrap(),
        U256::ZERO
    );
    assert_eq!(base.basic(CGT_V2_RESERVE).unwrap().unwrap().balance, U256::ZERO);
    assert_eq!(base.basic(VAULT).unwrap().unwrap().balance, U256::from(41));
}
