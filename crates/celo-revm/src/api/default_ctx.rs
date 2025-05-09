use crate::CeloTxEnv;
use op_revm::transaction::deposit::DepositTransactionParts;
use op_revm::{L1BlockInfo, OpSpecId, OpTransaction};
use revm::{
    Context, Journal, MainContext,
    context::{BlockEnv, CfgEnv},
    database_interface::EmptyDB,
};

/// Type alias for the default context type of the CeloEvm.
pub type CeloContext<DB> =
    Context<BlockEnv, OpTransaction<CeloTxEnv>, CfgEnv<OpSpecId>, DB, Journal<DB>, L1BlockInfo>;

/// Trait that allows for a default context to be created.
pub trait DefaultCelo {
    /// Create a default context.
    fn celo() -> CeloContext<EmptyDB>;
}

impl DefaultCelo for CeloContext<EmptyDB> {
    fn celo() -> Self {
        Context::mainnet()
            .with_tx(OpTransaction {
                base: CeloTxEnv::default(),
                enveloped_tx: Some(vec![0x00].into()),
                deposit: DepositTransactionParts::default(),
            })
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BEDROCK))
            .with_chain(L1BlockInfo::default())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::api::builder::CeloBuilder;
    use revm::{
        ExecuteEvm,
        inspector::{InspectEvm, NoOpInspector},
    };

    #[test]
    fn default_run_op() {
        let ctx = Context::celo();
        let mut evm = ctx.build_celo_with_inspector(NoOpInspector {});
        let _ = evm.replay();
        let _ = evm.inspect_replay();
    }
}
