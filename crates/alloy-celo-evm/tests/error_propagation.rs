//! Infrastructure failures must abort execution without classifying the payload as invalid.

use alloy_celo_evm::{CeloEvmFactory, blocklist::FeeCurrencyBlocklist, celo_precompiles_map};
use alloy_evm::{
    Evm, EvmEnv, EvmFactory,
    block::{BlockExecutionError, BlockValidationError},
    precompiles::{DynPrecompile, PrecompileInput},
};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use celo_revm::{
    CeloBuilder, CeloTransaction, DefaultCelo, FeeCurrencyContext, constants::get_addresses,
    fee_currency_context::FeeCurrencyInfo, precompiles::TRANSFER_ADDRESS,
};
use op_revm::{OpSpecId, OpTransaction};
use revm::{
    Context, Database, SystemCallEvm,
    context::{CfgEnv, TxEnv},
    context_interface::result::{AnyError, EVMError},
    database::InMemoryDB,
    precompile::{PrecompileError, PrecompileOutput},
    state::{AccountInfo, Bytecode},
};

const FEE_CURRENCY: Address = Address::with_last_byte(0xc1);
const MISSING_ACCOUNT: Address = Address::with_last_byte(0xd1);

#[derive(Debug)]
struct ReadError;
impl core::fmt::Display for ReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("injected missing account")
    }
}
impl core::error::Error for ReadError {}
impl revm::database_interface::DBErrorMarker for ReadError {}

#[derive(Clone, Copy, Debug)]
enum FeePhase {
    Balance,
    Debit,
    Credit,
}

#[test]
fn fatal_fee_calls_are_not_invalid_transactions_or_blocklisted() {
    for phase in [FeePhase::Balance, FeePhase::Debit, FeePhase::Credit] {
        for erased in [false, true] {
            let mut map = celo_precompiles_map(OpSpecId::FJORD);
            map.extend_precompiles([(
                FEE_CURRENCY,
                DynPrecompile::from(move |input: PrecompileInput<'_>| {
                    let balance_call = input.data.starts_with(&[0x70, 0xa0, 0x82, 0x31]);
                    // debitGasFees(address,uint256) has two ABI arguments; credit has more.
                    let debit_call = !balance_call && input.data.len() == 68;
                    let fail = match phase {
                        FeePhase::Balance => balance_call,
                        FeePhase::Debit => debit_call,
                        FeePhase::Credit => !balance_call && !debit_call,
                    };
                    if fail {
                        return Err(if erased {
                            PrecompileError::FatalAny(AnyError::new(ReadError))
                        } else {
                            PrecompileError::Fatal("injected fatal precompile failure".into())
                        });
                    }
                    let bytes = if balance_call {
                        U256::MAX.to_be_bytes::<32>().to_vec().into()
                    } else {
                        Bytes::new()
                    };
                    Ok(PrecompileOutput::new(0, bytes, 0))
                })
                .stateful(),
            )]);
            let mut cfg = CfgEnv::<OpSpecId>::default();
            cfg.chain_id = 42220;
            cfg.spec = OpSpecId::FJORD;
            let blocklist = FeeCurrencyBlocklist::default();
            let factory = CeloEvmFactory::default().with_blocklist(blocklist.clone());
            let env = EvmEnv::<OpSpecId> { cfg_env: cfg, ..Default::default() };
            let mut evm = factory.create_evm(InMemoryDB::default(), env).with_blocklist_enabled();
            *evm.precompiles_mut() = map;
            evm.ctx_mut().block.basefee = 1;
            let mut currencies = alloy_primitives::map::HashMap::default();
            currencies.insert(
                FEE_CURRENCY,
                FeeCurrencyInfo { exchange_rate: (U256::ONE, U256::ONE), intrinsic_gas: 50_000 },
            );
            evm.set_fee_currency_context(FeeCurrencyContext::new(
                currencies,
                Some(evm.ctx().block.number),
            ));
            let mut tx = CeloTransaction::new(OpTransaction {
                base: TxEnv {
                    tx_type: 0x7b,
                    caller: Address::with_last_byte(1),
                    kind: TxKind::Call(Address::with_last_byte(0xa0)),
                    gas_limit: 200_000,
                    gas_price: 1,
                    chain_id: Some(42220),
                    ..Default::default()
                },
                enveloped_tx: Some(Bytes::new()),
                deposit: Default::default(),
            });
            tx.fee_currency = Some(FEE_CURRENCY);
            let error = evm.transact_raw(tx).expect_err("fatal fee call must abort execution");
            // The pinned alloy PrecompilesMap converts FatalAny to Custom(String).
            assert!(matches!(error, EVMError::Custom(_)), "{phase:?}/{erased}: {error:?}");
            assert!(!blocklist.is_blocked(FEE_CURRENCY));
            let error = BlockExecutionError::evm(error, B256::ZERO);
            assert!(!matches!(
                error,
                BlockExecutionError::Validation(BlockValidationError::InvalidTx { .. })
            ));
        }
    }
}

#[derive(Debug)]
struct MissingAccountDb {
    inner: InMemoryDB,
    failures_left: usize,
}
impl MissingAccountDb {
    fn new(failures_left: usize) -> Self {
        let mut inner = InMemoryDB::default();
        inner.insert_account_info(
            MISSING_ACCOUNT,
            AccountInfo { balance: U256::from(100), ..Default::default() },
        );
        Self { inner, failures_left }
    }
}
impl Database for MissingAccountDb {
    type Error = ReadError;
    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if address == MISSING_ACCOUNT && self.failures_left != 0 {
            self.failures_left -= 1;
            return Err(ReadError);
        }
        Ok(self.inner.basic(address).unwrap())
    }
    fn code_by_hash(&mut self, hash: B256) -> Result<Bytecode, Self::Error> {
        Ok(self.inner.code_by_hash(hash).unwrap())
    }
    fn storage(&mut self, address: Address, slot: U256) -> Result<U256, Self::Error> {
        Ok(self.inner.storage(address, slot).unwrap())
    }
    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        Ok(self.inner.block_hash(number).unwrap())
    }
}

fn transfer_data() -> Bytes {
    let mut data = Vec::new();
    data.extend_from_slice(MISSING_ACCOUNT.into_word().as_slice());
    data.extend_from_slice(Address::with_last_byte(0xd2).into_word().as_slice());
    data.extend_from_slice(&U256::ONE.to_be_bytes::<32>());
    data.into()
}

#[test]
fn transfer_database_failure_aborts_both_precompile_providers() {
    for spec in [OpSpecId::FJORD, OpSpecId::JOVIAN] {
        for failures_left in [1, usize::MAX] {
            let mut cfg = CfgEnv::<OpSpecId>::default();
            cfg.chain_id = 42220;
            cfg.spec = spec;
            let caller = get_addresses(42220).celo_token;
            let mut direct = Context::celo()
                .with_db(MissingAccountDb::new(failures_left))
                .with_cfg(cfg.clone())
                .build_celo();
            let result =
                direct.system_call_one_with_caller(caller, TRANSFER_ADDRESS, transfer_data());
            assert!(matches!(result, Err(EVMError::Database(ReadError))), "{spec:?}: {result:?}");

            let mut mapped = Context::celo()
                .with_db(MissingAccountDb::new(failures_left))
                .with_cfg(cfg)
                .build_celo()
                .with_precompiles(celo_precompiles_map(spec));
            let error = mapped
                .system_call_one_with_caller(caller, TRANSFER_ADDRESS, transfer_data())
                .expect_err("dynamic transfer precompile must propagate database failure");
            assert!(
                matches!(&error, EVMError::Custom(message) if message.contains("injected missing account")),
                "{spec:?}: {error:?}"
            );
        }
    }
}
