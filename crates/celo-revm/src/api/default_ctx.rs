use crate::CeloTransaction;
use op_revm::{L1BlockInfo, OpSpecId};
use revm::{
    Context, Journal, MainContext,
    context::{BlockEnv, CfgEnv, TxEnv},
    database_interface::EmptyDB,
    primitives::U256,
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
            .with_chain(celo_default_l1_block_info(CELO_DEFAULT_SPEC))
    }
}

/// Default [`L1BlockInfo`] for context-free Celo EVM construction.
///
/// `L1BlockInfo::default()` leaves `operator_fee_scalar`/`operator_fee_constant` as
/// `None`, but op-revm `.expect()`s them in the operator-fee path for Isthmus+ specs, so
/// a native tx run through the default context would panic in
/// `CeloHandler::reimburse_caller`. Seed them with zero for Isthmus+, mirroring
/// `alloy-celo-evm`'s `CeloEvmFactory` (`default_l1_block_info`).
fn celo_default_l1_block_info(spec_id: OpSpecId) -> L1BlockInfo {
    let mut info = L1BlockInfo::default();
    if spec_id.is_enabled_in(OpSpecId::ISTHMUS) {
        info.operator_fee_scalar = Some(U256::ZERO);
        info.operator_fee_constant = Some(U256::ZERO);
    }
    info
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

    // Regression: `CELO_DEFAULT_SPEC` is Isthmus+, and op-revm's `operator_fee_refund`
    // `.expect()`s `L1BlockInfo`'s operator-fee fields for Isthmus+. `DefaultCelo::celo`
    // must seed them (as `alloy-celo-evm`'s `CeloEvmFactory` does), or a native tx run
    // through the default context panics in `CeloHandler::reimburse_caller`.
    #[test]
    fn default_context_operator_fee_refund_does_not_panic() {
        use revm::{context::ContextTr, interpreter::Gas, primitives::U256};
        let ctx = Context::celo();
        let refund = ctx
            .chain()
            .operator_fee_refund(&Gas::new(21_000), CELO_DEFAULT_SPEC);
        assert_eq!(refund, U256::ZERO);
    }
}
