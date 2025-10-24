use crate::{CeloBlockEnv, CeloTransaction};
use op_revm::OpSpecId;
use revm::{
    Context, Journal, MainContext,
    context::{BlockEnv, CfgEnv, TxEnv},
    database_interface::EmptyDB,
};

/// Type alias for the default context type of the CeloEvm.
pub type CeloContext<DB> =
    Context<BlockEnv, CeloTransaction<TxEnv>, CfgEnv<OpSpecId>, DB, Journal<DB>, CeloBlockEnv>;

/// Trait that allows for a default context to be created.
pub trait DefaultCelo {
    /// Create a default context.
    fn celo() -> CeloContext<EmptyDB>;
}

impl DefaultCelo for CeloContext<EmptyDB> {
    fn celo() -> Self {
        Context::mainnet()
            .with_tx(CeloTransaction::default())
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BEDROCK))
            .with_chain(CeloBlockEnv::default())
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
