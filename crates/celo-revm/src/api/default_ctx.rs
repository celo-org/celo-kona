use crate::CeloTransaction;
use op_revm::{L1BlockInfo, OpSpecId};
use revm::{
    Context, Journal, MainContext,
    context::{BlockEnv, CfgEnv, TxEnv},
    database_interface::EmptyDB,
};

/// Default [`OpSpecId`] for context-free Celo EVM construction (see [`DefaultCelo::celo`]).
///
/// Pinned to Celo's latest hardfork on the Celo side rather than using
/// `OpSpecId::default()`: Celo's hardfork schedule is independent of upstream's, so
/// op-revm advancing its `#[default]` to a fork Celo has not adopted (e.g. a future
/// `Interop`) must not silently change ours. This is only the base spec for the
/// context-free default, used where the real fork is otherwise set explicitly by the
/// caller (block execution, the pool's `build_pool_evm`); it just needs to be a modern
/// (post-Shanghai) fork so fee-currency bytecode using PUSH0/MCOPY/TLOAD runs. Bump when
/// Celo adopts a newer fork.
pub const CELO_DEFAULT_SPEC: OpSpecId = OpSpecId::JOVIAN;

/// Type alias for the default context type of the CeloEvm.
pub type CeloContext<DB> =
    Context<BlockEnv, CeloTransaction<TxEnv>, CfgEnv<OpSpecId>, DB, Journal<DB>, L1BlockInfo>;

/// Trait that allows for a default context to be created.
pub trait DefaultCelo {
    /// Create a default context.
    fn celo() -> CeloContext<EmptyDB>;
}

impl DefaultCelo for CeloContext<EmptyDB> {
    fn celo() -> Self {
        Context::mainnet()
            .with_tx(CeloTransaction::default())
            .with_cfg(CfgEnv::new_with_spec(CELO_DEFAULT_SPEC))
            .with_chain(L1BlockInfo::default())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::CeloBuilder;
    use revm::{ExecuteEvm, inspector::NoOpInspector};

    #[test]
    fn default_run_celo() {
        let ctx = Context::celo();
        // convert to celo context
        let mut evm = ctx.build_celo_with_inspector(NoOpInspector {});
        // execute
        let _ = evm.replay();
    }
}
