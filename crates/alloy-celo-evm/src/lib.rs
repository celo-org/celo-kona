#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::{borrow::Cow, format, vec::Vec};
use alloy_evm::{
    Database, Evm, EvmEnv, EvmFactory,
    precompiles::{DynPrecompile, PrecompilesMap},
};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use celo_alloy_consensus::CeloTxType;
use celo_revm::{
    CeloBuilder, CeloContext, CeloPrecompiles, CeloTransaction, DefaultCelo,
    constants,
    precompiles::transfer::{TRANSFER_ADDRESS, TRANSFER_GAS_COST},
};
use core::{
    fmt::Debug,
    ops::{Deref, DerefMut},
};
use op_revm::{
    L1BlockInfo, OpHaltReason, OpSpecId, OpTransaction, OpTransactionError,
    precompiles::OpPrecompiles,
};
use revm::{
    Context, ExecuteEvm, InspectEvm, Inspector,
    context::{BlockEnv, TxEnv},
    context_interface::result::{EVMError, ResultAndState},
    handler::PrecompileProvider,
    inspector::NoOpInspector,
    interpreter::InterpreterResult,
    precompile::{PrecompileError, PrecompileOutput},
};

pub mod block;
pub mod cip64_storage;

use cip64_storage::Cip64Storage;

/// Creates a default [`L1BlockInfo`] with zeroed operator fee fields for specs that require
/// them. Without this, `eth_call` panics on Isthmus+ because
/// `operator_fee_scalar`/`operator_fee_constant` are `None`.
fn default_l1_block_info(spec_id: OpSpecId) -> L1BlockInfo {
    let mut info = L1BlockInfo::default();
    if spec_id.is_enabled_in(OpSpecId::ISTHMUS) {
        info.operator_fee_scalar = Some(U256::ZERO);
        info.operator_fee_constant = Some(U256::ZERO);
    }
    info
}

/// Creates a [`PrecompilesMap`] containing the standard OP Stack precompiles plus the Celo
/// transfer precompile for the given spec.
pub fn celo_precompiles_map(spec_id: OpSpecId) -> PrecompilesMap {
    let mut map =
        PrecompilesMap::from_static(OpPrecompiles::new_with_spec(spec_id).precompiles());
    map.extend_precompiles([(
        TRANSFER_ADDRESS,
        make_transfer_precompile(spec_id),
    )]);
    map
}

/// Creates the Celo transfer [`DynPrecompile`] for the given spec.
fn make_transfer_precompile(spec_id: OpSpecId) -> DynPrecompile {
    fn coerce<F: Fn(alloy_evm::precompiles::PrecompileInput<'_>) -> revm::precompile::PrecompileResult + Send + Sync + 'static>(f: F) -> F { f }
    DynPrecompile::from(coerce(move |input| transfer_precompile(spec_id, input))).stateful()
}

/// Transfer precompile implementation for use as a [`DynPrecompile`].
fn transfer_precompile(
    spec_id: OpSpecId,
    mut input: alloy_evm::precompiles::PrecompileInput<'_>,
) -> revm::precompile::PrecompileResult {
    if input.is_static {
        return Err(PrecompileError::Other(Cow::Borrowed(
            "transfer precompile cannot be called in static context",
        )));
    }

    if input.gas < TRANSFER_GAS_COST {
        return Err(PrecompileError::OutOfGas);
    }

    let chain_id = input.internals.chain_id();
    if input.caller != constants::get_addresses(chain_id).celo_token {
        return Err(PrecompileError::Other(Cow::Borrowed(
            "invalid caller for transfer precompile",
        )));
    }

    if input.data.len() != 96 {
        return Err(PrecompileError::Other(Cow::Borrowed("invalid input length")));
    }

    let from = Address::from_slice(&input.data[12..32]);
    let to = Address::from_slice(&input.data[44..64]);
    let value = U256::from_be_slice(&input.data[64..96]);

    let revert_cold_status = !spec_id.is_enabled_in(OpSpecId::JOVIAN);
    let revert_from_cold = revert_cold_status
        && input.internals.load_account(from).map(|a| a.is_cold).unwrap_or(true);
    let revert_to_cold = revert_cold_status
        && input.internals.load_account(to).map(|a| a.is_cold).unwrap_or(true);

    let result = input.internals.transfer(from, to, value);

    if revert_from_cold {
        if let Ok(mut account) = input.internals.load_account_mut(from) {
            account.data.unsafe_mark_cold();
        }
    }
    if revert_to_cold {
        if let Ok(mut account) = input.internals.load_account_mut(to) {
            account.data.unsafe_mark_cold();
        }
    }

    match result {
        Ok(None) => Ok(PrecompileOutput::new(TRANSFER_GAS_COST, Bytes::new())),
        Ok(Some(transfer_err)) => Err(PrecompileError::Other(Cow::Owned(format!(
            "transfer error occurred: {transfer_err:?}"
        )))),
        Err(db_err) => Err(PrecompileError::Other(Cow::Owned(format!(
            "database error occurred: {db_err:?}"
        )))),
    }
}

/// Celo EVM implementation.
///
/// This is a wrapper type around the `revm` evm with optional [`Inspector`] (tracing)
/// support. [`Inspector`] support is configurable at runtime because it's part of the underlying
/// [`CeloEvm`](celo_revm::CeloEvm) type.
#[allow(missing_debug_implementations)] // missing celo_revm::CeloContext Debug impl
pub struct CeloEvm<DB: Database, I, P = CeloPrecompiles> {
    inner: celo_revm::CeloEvm<DB, I, P>,
    inspect: bool,
    cip64_storage: Cip64Storage,
}

impl<DB: Database, I, P> CeloEvm<DB, I, P> {
    /// Provides a reference to the EVM context.
    pub const fn ctx(&self) -> &CeloContext<DB> {
        &self.inner.inner.0.ctx
    }

    /// Provides a mutable reference to the EVM context.
    pub const fn ctx_mut(&mut self) -> &mut CeloContext<DB> {
        &mut self.inner.inner.0.ctx
    }

    /// Provides a mutable reference to the EVM inspector.
    pub const fn inspector_mut(&mut self) -> &mut I {
        &mut self.inner.inner.0.inspector
    }

    /// Creates a FeeCurrencyContext from the current EVM state.
    pub fn create_fee_currency_context(&mut self) -> celo_revm::FeeCurrencyContext
    where
        I: Inspector<CeloContext<DB>>,
        P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
    {
        celo_revm::FeeCurrencyContext::new_from_evm(&mut self.inner)
    }

    /// Provides a reference to the CIP-64 storage.
    pub const fn cip64_storage(&self) -> &Cip64Storage {
        &self.cip64_storage
    }
}

impl<DB: Database, I, P> CeloEvm<DB, I, P> {
    /// Creates a new Celo EVM instance.
    ///
    /// The `inspect` argument determines whether the configured [`Inspector`] of the given
    /// [`CeloEvm`](celo_revm::CeloEvm) should be invoked on [`Evm::transact`].
    pub fn new(evm: celo_revm::CeloEvm<DB, I, P>, inspect: bool) -> Self {
        Self { inner: evm, inspect, cip64_storage: Cip64Storage::default() }
    }
}

impl<DB: Database, I, P> Deref for CeloEvm<DB, I, P> {
    type Target = CeloContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I, P> DerefMut for CeloEvm<DB, I, P> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, I, P> Evm for CeloEvm<DB, I, P>
where
    DB: Database,
    I: Inspector<CeloContext<DB>>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
{
    type DB = DB;
    type Tx = CeloTransaction<TxEnv>;
    type Error = EVMError<DB::Error, OpTransactionError>;
    type HaltReason = OpHaltReason;
    type Spec = OpSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = P;
    type Inspector = I;

    fn block(&self) -> &BlockEnv {
        &self.block
    }

    fn chain_id(&self) -> u64 {
        self.cfg.chain_id
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        // Capture fee_currency before execution (it's consumed by transact)
        let fee_currency = tx.fee_currency;

        let result = if self.inspect { self.inner.inspect_tx(tx) } else { self.inner.transact(tx) };

        // CIP64 NOTE:
        // Extract and store the cip64 info to a shared storage to be able to add the credit/debit
        // logs when building the receipt in the receipts_builder (alloy-celo-evm)

        // After execution, extract the UPDATED CIP-64 info from the context
        // The handler modifies this during execution to set the reverted flag
        if let Some(cip64_info) = self.inner.inner.0.ctx.tx.cip64_tx_info.clone() {
            self.cip64_storage.store_cip64_info(fee_currency, cip64_info);
        }

        result
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        let tx = CeloTransaction {
            op_tx: OpTransaction {
                base: TxEnv {
                    caller,
                    kind: TxKind::Call(contract),
                    // Explicitly set nonce to 0 so revm does not do any nonce checks
                    nonce: 0,
                    gas_limit: 30_000_000,
                    value: U256::ZERO,
                    data,
                    // Setting the gas price to zero enforces that no value is transferred as part
                    // of the call, and that the call will not count against the
                    // block's gas limit
                    gas_price: 0,
                    // The chain ID check is not relevant here and is disabled if set to None
                    chain_id: None,
                    // Setting the gas priority fee to None ensures the effective gas price is
                    // derived from the `gas_price` field, which we need to be
                    // zero
                    gas_priority_fee: None,
                    access_list: Default::default(),
                    // blob fields can be None for this tx
                    blob_hashes: Vec::new(),
                    max_fee_per_blob_gas: 0,
                    tx_type: CeloTxType::Deposit as u8,
                    authorization_list: Default::default(),
                },
                // The L1 fee is not charged for the EIP-4788 transaction, submit zero bytes for the
                // enveloped tx size.
                enveloped_tx: Some(Bytes::default()),
                deposit: Default::default(),
            },
            fee_currency: None,
            cip64_tx_info: None,
            effective_gas_price: None,
        };

        let mut gas_limit = tx.op_tx.base.gas_limit;
        let mut basefee = 0;
        let mut disable_nonce_check = true;

        // ensure the block gas limit is >= the tx
        core::mem::swap(&mut self.block.gas_limit, &mut gas_limit);
        // disable the base fee check for this call by setting the base fee to zero
        core::mem::swap(&mut self.block.basefee, &mut basefee);
        // disable the nonce check
        core::mem::swap(&mut self.cfg.disable_nonce_check, &mut disable_nonce_check);

        let mut res = self.transact(tx);

        // swap back to the previous gas limit
        core::mem::swap(&mut self.block.gas_limit, &mut gas_limit);
        // swap back to the previous base fee
        core::mem::swap(&mut self.block.basefee, &mut basefee);
        // swap back to the previous nonce check flag
        core::mem::swap(&mut self.cfg.disable_nonce_check, &mut disable_nonce_check);

        // NOTE: We assume that only the contract storage is modified. Revm currently marks the
        // caller and block beneficiary accounts as "touched" when we do the above transact calls,
        // and includes them in the result.
        //
        // We're doing this state cleanup to make sure that changeset only includes the changed
        // contract storage.
        if let Ok(res) = &mut res {
            res.state.retain(|addr, _| *addr == contract);
        }

        res
    }

    fn db_mut(&mut self) -> &mut Self::DB {
        &mut self.journaled_state.database
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec>) {
        let Context { block: block_env, cfg: cfg_env, journaled_state, .. } =
            self.inner.inner.0.ctx;

        (journaled_state.database, EvmEnv { block_env, cfg_env })
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspect = enabled;
    }

    fn precompiles(&self) -> &Self::Precompiles {
        &self.inner.inner.0.precompiles
    }

    fn precompiles_mut(&mut self) -> &mut Self::Precompiles {
        &mut self.inner.inner.0.precompiles
    }

    fn inspector(&self) -> &Self::Inspector {
        &self.inner.inner.0.inspector
    }

    fn inspector_mut(&mut self) -> &mut Self::Inspector {
        &mut self.inner.inner.0.inspector
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        let inner_evm = &self.inner.inner.0;
        (&inner_evm.ctx.journaled_state.database, &inner_evm.inspector, &inner_evm.precompiles)
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        let inner_evm = &mut self.inner.inner.0;
        (
            &mut inner_evm.ctx.journaled_state.database,
            &mut inner_evm.inspector,
            &mut inner_evm.precompiles,
        )
    }
}

/// Factory producing [`CeloEvm`]s.
#[derive(Debug, Default, Clone)]
pub struct CeloEvmFactory {
    /// Shared CIP-64 storage. When set, all EVMs created by this factory will use this
    /// storage instance, enabling the receipt builder to read CIP-64 data written by the EVM.
    pub cip64_storage: Option<Cip64Storage>,
}

impl CeloEvmFactory {
    /// Creates a new factory with shared CIP-64 storage.
    pub fn with_cip64_storage(cip64_storage: Cip64Storage) -> Self {
        Self { cip64_storage: Some(cip64_storage) }
    }
}

impl EvmFactory for CeloEvmFactory {
    type Evm<DB: Database, I: Inspector<CeloContext<DB>>> = CeloEvm<DB, I, Self::Precompiles>;
    type Context<DB: Database> = CeloContext<DB>;
    type Tx = CeloTransaction<TxEnv>;
    type Error<DBError: core::error::Error + Send + Sync + 'static> =
        EVMError<DBError, OpTransactionError>;
    type HaltReason = OpHaltReason;
    type Spec = OpSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        mut input: EvmEnv<OpSpecId>,
    ) -> Self::Evm<DB, NoOpInspector> {
        input.cfg_env.limit_contract_code_size = Some(constants::CELO_MAX_CODE_SIZE);
        let spec_id = input.cfg_env.spec;
        CeloEvm {
            inner: Context::celo()
                .with_db(db)
                .with_block(input.block_env)
                .with_cfg(input.cfg_env)
                .with_chain(default_l1_block_info(spec_id))
                .build_celo_with_inspector(NoOpInspector {})
                .with_precompiles(celo_precompiles_map(spec_id)),
            inspect: false,
            cip64_storage: self.cip64_storage.clone().unwrap_or_default(),
        }
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        mut input: EvmEnv<OpSpecId>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        input.cfg_env.limit_contract_code_size = Some(constants::CELO_MAX_CODE_SIZE);
        let spec_id = input.cfg_env.spec;
        CeloEvm {
            inner: Context::celo()
                .with_db(db)
                .with_block(input.block_env)
                .with_cfg(input.cfg_env)
                .with_chain(default_l1_block_info(spec_id))
                .build_celo_with_inspector(inspector)
                .with_precompiles(celo_precompiles_map(spec_id)),
            inspect: true,
            cip64_storage: self.cip64_storage.clone().unwrap_or_default(),
        }
    }
}
