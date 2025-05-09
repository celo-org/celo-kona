use crate::evm::CeloEvm;
use op_revm::{L1BlockInfo, OpSpecId, transaction::OpTxTr};
use revm::{
    Context, Database,
    context::{Cfg, JournalOutput},
    context_interface::{Block, JournalTr},
    handler::instructions::EthInstructions,
    interpreter::interpreter::EthInterpreter,
};

/// Trait that allows for optimism CeloEvm to be built.
pub trait CeloBuilder: Sized {
    /// Type of the context.
    type Context;

    /// Build the op.
    fn build_celo(
        self,
    ) -> CeloEvm<Self::Context, (), EthInstructions<EthInterpreter, Self::Context>>;

    /// Build the op with an inspector.
    fn build_celo_with_inspector<INSP>(
        self,
        inspector: INSP,
    ) -> CeloEvm<Self::Context, INSP, EthInstructions<EthInterpreter, Self::Context>>;
}

impl<BLOCK, TX, CFG, DB, JOURNAL> CeloBuilder for Context<BLOCK, TX, CFG, DB, JOURNAL, L1BlockInfo>
where
    BLOCK: Block,
    TX: OpTxTr,
    CFG: Cfg<Spec = OpSpecId>,
    DB: Database,
    JOURNAL: JournalTr<Database = DB, FinalOutput = JournalOutput>,
{
    type Context = Self;

    fn build_celo(
        self,
    ) -> CeloEvm<Self::Context, (), EthInstructions<EthInterpreter, Self::Context>> {
        CeloEvm::new(self, ())
    }

    fn build_celo_with_inspector<INSP>(
        self,
        inspector: INSP,
    ) -> CeloEvm<Self::Context, INSP, EthInstructions<EthInterpreter, Self::Context>> {
        CeloEvm::new(self, inspector)
    }
}
