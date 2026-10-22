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
    cgt_v2::{CGT_V2_DEPOSITOR, CGT_V2_L1_BLOCK, CgtV2Activation},
};
use op_alloy_consensus::TxDeposit;
use op_revm::OpSpecId;
use revm::{
    Database,
    database::{CacheDB, EmptyDB, State},
    state::{AccountInfo, Bytecode},
};

const VAULT: Address = address!("4200000000000000000000000000000000000011");
const RESERVE: Address = address!("4200000000000000000000000000000000000029");

fn old_vault_code() -> Bytes {
    let mut code = alloy_primitives::hex::decode("60003560e01c").unwrap();
    let mut jumps = Vec::new();
    for sig in ["recipient()", "minWithdrawalAmount()", "withdrawalNetwork()"] {
        code.extend_from_slice(&[0x80, 0x63]);
        code.extend_from_slice(&keccak256(sig)[..4]);
        code.extend_from_slice(&[0x14, 0x61, 0, 0, 0x57]);
        jumps.push(code.len() - 3);
    }
    // A preceding legacy call changes a setting through the old code.
    code.extend_from_slice(&alloy_primitives::hex::decode("601260055500").unwrap());
    for (pos, value_code) in jumps.into_iter().zip(["6099", "600554", "6001"]) {
        let dest = (code.len() as u16).to_be_bytes();
        code[pos..pos + 2].copy_from_slice(&dest);
        code.push(0x5b);
        code.extend_from_slice(&alloy_primitives::hex::decode(value_code).unwrap());
        code.extend_from_slice(&alloy_primitives::hex::decode("60005260206000f3").unwrap());
    }
    code.into()
}

fn database() -> CacheDB<EmptyDB> {
    let mut db = CacheDB::<EmptyDB>::default();
    for (addr, code) in [
        (VAULT, old_vault_code()),
        (CGT_V2_L1_BLOCK, alloy_primitives::hex::decode("60003560025500").unwrap().into()),
    ] {
        db.insert_account_info(
            addr,
            AccountInfo {
                code_hash: keccak256(&code),
                code: Some(Bytecode::new_raw(code)),
                ..Default::default()
            },
        );
    }
    db.insert_account_storage(VAULT, U256::ZERO, U256::from(91)).unwrap();
    db.insert_account_storage(VAULT, U256::from(5), U256::from(17)).unwrap();
    db.cache.accounts.get_mut(&VAULT).unwrap().info.balance = U256::from(55);
    db.insert_account_info(RESERVE, AccountInfo { balance: U256::from(7), ..Default::default() });
    db
}
fn env() -> EvmEnv<OpSpecId> {
    let mut env: EvmEnv<OpSpecId> = EvmEnv::default();
    env.cfg_env.chain_id = 1337;
    env.cfg_env.spec = OpSpecId::ISTHMUS;
    env.block_env.number = U256::from(1);
    env.block_env.timestamp = U256::from(1_750_000_000u64);
    env.block_env.gas_limit = 5_000_000;
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
    recovered(TxDeposit {
        from: CGT_V2_DEPOSITOR,
        to: TxKind::Call(CGT_V2_L1_BLOCK),
        gas_limit: 100_000,
        input: Bytes::from(vec![0x11; 32]),
        source_hash: B256::repeat_byte(0x22),
        ..Default::default()
    })
}
fn signal() -> Recovered<CeloTxEnvelope> {
    recovered(
        CgtV2Activation {
            l1_origin_hash: B256::repeat_byte(0x11),
            migrated_amount: U256::from(600),
        }
        .to_deposit(),
    )
}

#[test]
fn cgt_v2_executes_at_signal_position_and_preserves_vault() {
    let f = factory();
    let mut db = State::builder().with_database(database()).with_bundle_update().build();
    let mut e = f.create_executor(
        f.evm_factory().create_evm(&mut db, env()),
        OpBlockExecutionCtx { parent_beacon_block_root: Some(B256::ZERO), ..Default::default() },
    );
    e.apply_pre_execution_changes().unwrap();
    assert!(
        e.evm_mut()
            .db_mut()
            .basic(CELO_GAS_BRIDGE_L2)
            .unwrap()
            .is_none_or(|a| a.is_empty_code_hash())
    );
    e.execute_transaction(&info()).unwrap();
    e.execute_transaction(&recovered(TxDeposit {
        from: Address::ZERO,
        to: TxKind::Call(VAULT),
        gas_limit: 100_000,
        source_hash: B256::repeat_byte(0x33),
        ..Default::default()
    }))
    .unwrap();
    assert_eq!(e.evm_mut().db_mut().storage(VAULT, U256::from(5)).unwrap(), U256::from(18));
    // Read-only work can leave legacy code cached between transaction executions.
    let old_info = e.evm_mut().db_mut().basic(CGT_V2_L1_BLOCK).unwrap().unwrap();
    e.evm_mut().ctx_mut().journaled_state.state.insert(CGT_V2_L1_BLOCK, old_info.into());
    e.execute_transaction(&signal()).unwrap();
    let getter = recovered(TxDeposit {
        from: Address::ZERO,
        to: TxKind::Call(CGT_V2_L1_BLOCK),
        gas_limit: 100_000,
        source_hash: B256::repeat_byte(0x44),
        input: Bytes::copy_from_slice(&keccak256(b"isCustomGasToken()")[..4]),
        ..Default::default()
    });
    let output = e.execute_transaction_without_commit(&getter).unwrap();
    assert_eq!(
        output.inner.result.result.output().unwrap().as_ref(),
        U256::from(1).to_be_bytes::<32>(),
        "the next deposit must execute newly installed code"
    );
    e.commit_transaction(output);
    let receipts = e.receipts();
    assert!(alloy_consensus::TxReceipt::status(&receipts[2]));
    assert!(
        alloy_consensus::TxReceipt::cumulative_gas_used(&receipts[2])
            > alloy_consensus::TxReceipt::cumulative_gas_used(&receipts[1])
    );
    {
        let db = e.evm_mut().db_mut();
        assert!(db.basic(CELO_GAS_BRIDGE_L2).unwrap().is_some_and(|a| !a.is_empty_code_hash()));
        assert_eq!(db.storage(CGT_V2_L1_BLOCK, upgrade18::cgt_flag_slot()).unwrap(), U256::from(1));
        assert_eq!(db.basic(RESERVE).unwrap().unwrap().balance, U256::from(407));
        assert_eq!(db.storage(VAULT, U256::ZERO).unwrap(), U256::from(91));
        assert_eq!(db.storage(VAULT, U256::from(1)).unwrap(), U256::from(18));
        assert_eq!(db.basic(VAULT).unwrap().unwrap().balance, U256::from(55));
        assert_eq!(db.basic(CGT_V2_DEPOSITOR).unwrap().unwrap().nonce, 2);
    }
    assert!(e.execute_transaction(&signal()).is_err(), "a committed signal cannot repeat");
    drop(e);
    db.merge_transitions(revm::database::states::bundle_state::BundleRetention::Reverts);
    assert!(db.take_bundle().state.contains_key(&CELO_GAS_BRIDGE_L2));
}

#[test]
fn cgt_v2_discarded_candidate_leaves_no_migration() {
    let f = factory();
    let mut db = State::builder().with_database(database()).build();
    let mut e = f.create_executor(
        f.evm_factory().create_evm(&mut db, env()),
        OpBlockExecutionCtx { parent_beacon_block_root: Some(B256::ZERO), ..Default::default() },
    );
    e.apply_pre_execution_changes().unwrap();
    e.execute_transaction(&info()).unwrap();
    let output = e.execute_transaction_without_commit(&signal()).unwrap();
    assert!(output.inner.result.result.is_success());
    assert!(
        e.evm_mut()
            .db_mut()
            .basic(CELO_GAS_BRIDGE_L2)
            .unwrap()
            .is_none_or(|a| a.is_empty_code_hash())
    );
    assert_eq!(e.evm_mut().db_mut().basic(RESERVE).unwrap().unwrap().balance, U256::from(7));
    drop(output);
    assert!(
        e.execute_transaction_with_commit_condition(&signal(), |_| CommitChanges::No)
            .unwrap()
            .is_none()
    );
    assert_eq!(e.receipts().len(), 1);
    assert!(
        e.evm_mut()
            .db_mut()
            .basic(CELO_GAS_BRIDGE_L2)
            .unwrap()
            .is_none_or(|a| a.is_empty_code_hash())
    );
    e.execute_transaction_with_commit_condition(&signal(), |_| CommitChanges::Yes)
        .unwrap()
        .unwrap();
    assert_eq!(e.evm_mut().db_mut().basic(RESERVE).unwrap().unwrap().balance, U256::from(407));
}

#[test]
fn cgt_v2_invalid_origin_and_amount_do_not_commit() {
    for (origin, amount) in [(B256::repeat_byte(0x55), 600), (B256::repeat_byte(0x11), 1001)] {
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
        let tx = recovered(
            CgtV2Activation { l1_origin_hash: origin, migrated_amount: U256::from(amount) }
                .to_deposit(),
        );
        let err = e.execute_transaction(&tx).unwrap_err();
        assert!(
            err.as_validation().is_some(),
            "invalid protocol signals must be INVALID, not an internal engine error: {err:?}"
        );
        assert!(
            e.evm_mut()
                .db_mut()
                .basic(CELO_GAS_BRIDGE_L2)
                .unwrap()
                .is_none_or(|a| a.is_empty_code_hash())
        );
        assert_eq!(e.receipts().len(), 1);
    }
}

#[test]
fn cgt_v2_setter_revert_or_halt_does_not_commit() {
    use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
    use revm::precompile::{PrecompileHalt, PrecompileOutput};
    for revert in [true, false] {
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
        e.evm_mut().components_mut().2.extend_precompiles([(
            CGT_V2_L1_BLOCK,
            DynPrecompile::from(move |_input: PrecompileInput<'_>| {
                Ok(if revert {
                    PrecompileOutput::revert(0, Bytes::new(), 0)
                } else {
                    PrecompileOutput::halt(PrecompileHalt::OutOfGas, 0)
                })
            }),
        )]);
        let err = e.execute_transaction(&signal()).unwrap_err();
        assert!(err.to_string().contains("setter reverted or halted"), "{err}");
        assert_eq!(e.receipts().len(), 1);
        assert_eq!(e.evm_mut().db_mut().basic(RESERVE).unwrap().unwrap().balance, U256::from(7));
        assert!(
            e.evm_mut()
                .db_mut()
                .basic(CELO_GAS_BRIDGE_L2)
                .unwrap()
                .is_none_or(|a| a.is_empty_code_hash())
        );
        assert_eq!(e.evm_mut().db_mut().storage(VAULT, U256::from(1)).unwrap(), U256::ZERO);
    }
}

#[test]
fn cgt_v2_installed_vault_getters_initialization_and_native_withdrawal() {
    let f = factory();
    let mut db = State::builder().with_database(database()).build();
    let mut e = f.create_executor(
        f.evm_factory().create_evm(&mut db, env()),
        OpBlockExecutionCtx { parent_beacon_block_root: Some(B256::ZERO), ..Default::default() },
    );
    e.apply_pre_execution_changes().unwrap();
    e.execute_transaction(&info()).unwrap();
    e.execute_transaction(&signal()).unwrap();
    let make_call = |input: Bytes| {
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
        ("recipient()", U256::from(0x99)),
        ("minWithdrawalAmount()", U256::from(17)),
        ("withdrawalNetwork()", U256::from(1)),
    ] {
        let output = e
            .execute_transaction_without_commit(&make_call(Bytes::copy_from_slice(
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
    let output = e.execute_transaction_without_commit(&make_call(init.into())).unwrap();
    assert!(!output.inner.result.result.is_success(), "a second initializer must revert");
    let output = e
        .execute_transaction_without_commit(&make_call(Bytes::copy_from_slice(
            &keccak256(b"withdraw()")[..4],
        )))
        .unwrap();
    assert!(output.inner.result.result.is_success());
    e.commit_transaction(output);
    assert_eq!(e.evm_mut().db_mut().basic(VAULT).unwrap().unwrap().balance, U256::ZERO);
    assert_eq!(
        e.evm_mut().db_mut().basic(Address::with_last_byte(0x99)).unwrap().unwrap().balance,
        U256::from(55)
    );
    assert_eq!(e.evm_mut().db_mut().storage(VAULT, U256::ZERO).unwrap(), U256::from(146));
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
        if a == VAULT && s == U256::from(5) {
            return Err(MigrationDbError);
        }
        self.0.storage(a, s).map_err(|_| MigrationDbError)
    }
    fn block_hash(&mut self, n: u64) -> Result<B256, Self::Error> {
        self.0.block_hash(n).map_err(|_| MigrationDbError)
    }
}
#[test]
fn cgt_v2_vault_getter_preserves_database_error() {
    use celo_revm::contracts::CoreContractError;
    use revm::context_interface::result::EVMError;
    let f = factory();
    let mut db = State::builder().with_database(FailingDatabase(database())).build();
    let mut e = f.create_executor(
        f.evm_factory().create_evm(&mut db, env()),
        OpBlockExecutionCtx { parent_beacon_block_root: Some(B256::ZERO), ..Default::default() },
    );
    e.apply_pre_execution_changes().unwrap();
    e.execute_transaction(&info()).unwrap();
    let err = e.execute_transaction(&signal()).unwrap_err();
    let core = err
        .as_internal()
        .and_then(|e| {
            e.downcast_other::<CoreContractError<<State<FailingDatabase> as Database>::Error>>()
        })
        .expect("retain the actual getter error");
    assert!(
        matches!(core, CoreContractError::Evm(e) if matches!(e.as_ref(), EVMError::Database(_)))
    );
    assert_eq!(e.receipts().len(), 1);
    assert_eq!(e.evm_mut().db_mut().basic(RESERVE).unwrap().unwrap().balance, U256::from(7));
    assert!(
        e.evm_mut()
            .db_mut()
            .basic(CELO_GAS_BRIDGE_L2)
            .unwrap()
            .is_none_or(|a| a.is_empty_code_hash())
    );
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
    e.execute_transaction(&signal()).unwrap();
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
    assert_eq!(e.evm_mut().db_mut().basic(VAULT).unwrap().unwrap().balance, U256::from(55));
    assert_eq!(e.evm_mut().db_mut().storage(VAULT, U256::ZERO).unwrap(), U256::from(91));
}

#[test]
fn cgt_v2_bad_legacy_vault_getter_aborts() {
    for code in ["60006000fd", "fe", "00"] {
        let mut base = database();
        let code = Bytes::from(alloy_primitives::hex::decode(code).unwrap());
        base.insert_account_info(
            VAULT,
            AccountInfo {
                code_hash: keccak256(&code),
                code: Some(Bytecode::new_raw(code)),
                ..Default::default()
            },
        );
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
        assert!(e.execute_transaction(&signal()).unwrap_err().as_internal().is_some());
        assert_eq!(e.evm_mut().db_mut().basic(RESERVE).unwrap().unwrap().balance, U256::from(7));
        assert!(
            e.evm_mut()
                .db_mut()
                .basic(CELO_GAS_BRIDGE_L2)
                .unwrap()
                .is_none_or(|a| a.is_empty_code_hash())
        );
    }
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
        Address::ZERO,
    )
}

#[test]
fn cgt_v2_incremental_execution_rejects_users_before_and_after_signal() {
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
            e.execute_transaction(&signal()).unwrap_err()
        } else {
            e.execute_transaction(&signal()).unwrap();
            e.execute_transaction(&user_transaction()).unwrap_err()
        };
        assert!(error.to_string().contains("unexpected non-deposit transactions"), "{error}");
    }
}

#[test]
fn cgt_v2_discarded_candidates_do_not_change_deposits_only_rule() {
    for discard_signal in [false, true] {
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
        let discarded = if discard_signal { signal() } else { user_transaction() };
        assert!(
            e.execute_transaction_with_commit_condition(&discarded, |_| CommitChanges::No)
                .unwrap()
                .is_none()
        );
        if discard_signal {
            e.execute_transaction(&user_transaction())
                .expect("discarded signal does not ban users");
        } else {
            e.execute_transaction(&signal()).expect("discarded user does not ban activation");
        }
    }
}
