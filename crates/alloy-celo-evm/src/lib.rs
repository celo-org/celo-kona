#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::vec::Vec;
use alloy_evm::{Database, Evm, EvmEnv, EvmFactory};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use celo_alloy_consensus::CeloTxType;
use celo_revm::{CeloBuilder, CeloContext, CeloPrecompiles, CeloTransaction, DefaultCelo};
use core::{
    fmt::Debug,
    ops::{Deref, DerefMut},
};
use op_revm::{OpHaltReason, OpSpecId, OpTransaction, OpTransactionError};
use revm::{
    Context, ExecuteEvm, InspectEvm, Inspector,
    context::{BlockEnv, TxEnv},
    context_interface::result::{EVMError, ResultAndState},
    handler::instructions::EthInstructions,
    inspector::NoOpInspector,
    interpreter::interpreter::EthInterpreter,
};

pub mod block;

/// Celo EVM implementation.
///
/// This is a wrapper type around the `revm` evm with optional [`Inspector`] (tracing)
/// support. [`Inspector`] support is configurable at runtime because it's part of the underlying
/// [`CeloEvm`](celo_revm::CeloEvm) type.
#[allow(missing_debug_implementations)] // missing celo_revm::CeloContext Debug impl
pub struct CeloEvm<DB: Database, I> {
    inner: celo_revm::CeloEvm<
        CeloContext<DB>,
        I,
        EthInstructions<EthInterpreter, CeloContext<DB>>,
        CeloPrecompiles,
    >,
    inspect: bool,
}

impl<DB: Database, I> CeloEvm<DB, I> {
    /// Provides a reference to the EVM context.
    pub const fn ctx(&self) -> &CeloContext<DB> {
        &self.inner.0.0.ctx
    }

    /// Provides a mutable reference to the EVM context.
    pub const fn ctx_mut(&mut self) -> &mut CeloContext<DB> {
        &mut self.inner.0.0.ctx
    }

    /// Provides a mutable reference to the EVM inspector.
    pub const fn inspector_mut(&mut self) -> &mut I {
        &mut self.inner.0.0.inspector
    }
}

impl<DB: Database, I> CeloEvm<DB, I> {
    /// Creates a new Celo EVM instance.
    ///
    /// The `inspect` argument determines whether the configured [`Inspector`] of the given
    /// [`CeloEvm`](celo_revm::CeloEvm) should be invoked on [`Evm::transact`].
    pub const fn new(
        evm: celo_revm::CeloEvm<
            CeloContext<DB>,
            I,
            EthInstructions<EthInterpreter, CeloContext<DB>>,
        >,
        inspect: bool,
    ) -> Self {
        Self {
            inner: evm,
            inspect,
        }
    }
}

impl<DB: Database, I> Deref for CeloEvm<DB, I> {
    type Target = CeloContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I> DerefMut for CeloEvm<DB, I> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, I> Evm for CeloEvm<DB, I>
where
    DB: Database,
    I: Inspector<CeloContext<DB>>,
{
    type DB = DB;
    type Tx = CeloTransaction<TxEnv>;
    type Error = EVMError<DB::Error, OpTransactionError>;
    type HaltReason = OpHaltReason;
    type Spec = OpSpecId;
    type Precompiles = CeloPrecompiles;
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
        if self.inspect {
            self.inner.set_tx(tx);
            self.inner.inspect_replay()
        } else {
            self.inner.transact(tx)
        }
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
                    // Setting the gas price to zero enforces that no value is transferred as part of
                    // the call, and that the call will not count against the block's
                    // gas limit
                    gas_price: 0,
                    // The chain ID check is not relevant here and is disabled if set to None
                    chain_id: None,
                    // Setting the gas priority fee to None ensures the effective gas price is derived
                    // from the `gas_price` field, which we need to be zero
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
        let Context {
            block: block_env,
            cfg: cfg_env,
            journaled_state,
            ..
        } = self.inner.0.0.ctx;

        (journaled_state.database, EvmEnv { block_env, cfg_env })
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspect = enabled;
    }

    fn precompiles(&self) -> &Self::Precompiles {
        &self.inner.0.0.precompiles
    }

    fn precompiles_mut(&mut self) -> &mut Self::Precompiles {
        &mut self.inner.0.0.precompiles
    }

    fn inspector(&self) -> &Self::Inspector {
        &self.inner.0.0.inspector
    }

    fn inspector_mut(&mut self) -> &mut Self::Inspector {
        &mut self.inner.0.0.inspector
    }
}

/// Factory producing [`CeloEvm`]s.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct CeloEvmFactory;

impl EvmFactory for CeloEvmFactory {
    type Evm<DB: Database, I: Inspector<CeloContext<DB>>> = CeloEvm<DB, I>;
    type Context<DB: Database> = CeloContext<DB>;
    type Tx = CeloTransaction<TxEnv>;
    type Error<DBError: core::error::Error + Send + Sync + 'static> =
        EVMError<DBError, OpTransactionError>;
    type HaltReason = OpHaltReason;
    type Spec = OpSpecId;
    type Precompiles = CeloPrecompiles;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<OpSpecId>,
    ) -> Self::Evm<DB, NoOpInspector> {
        let spec_id = input.cfg_env.spec;
        CeloEvm {
            inner: Context::celo()
                .with_db(db)
                .with_block(input.block_env)
                .with_cfg(input.cfg_env)
                .build_celo_with_inspector(NoOpInspector {})
                .with_precompiles(CeloPrecompiles::new_with_spec(spec_id)),
            inspect: false,
        }
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<OpSpecId>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let spec_id = input.cfg_env.spec;
        CeloEvm {
            inner: Context::celo()
                .with_db(db)
                .with_block(input.block_env)
                .with_cfg(input.cfg_env)
                .build_celo_with_inspector(inspector)
                .with_precompiles(CeloPrecompiles::new_with_spec(spec_id)),
            inspect: true,
        }
    }
}
