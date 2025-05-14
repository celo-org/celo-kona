use crate::{CeloEvm, api::celo_block_env::CeloBlockEnv, transaction::CeloTxTr};
use op_revm::OpSpecId;
use revm::{
    Context, Database,
    context::{Cfg, JournalOutput},
    context_interface::{Block, JournalTr},
    handler::instructions::EthInstructions,
    interpreter::interpreter::EthInterpreter,
};

/// Trait that allows for celo CeloEvm to be built.
pub trait CeloBuilder: Sized {
    /// Type of the context.
    type Context;

    /// Build the CeloEvm.
    fn build_celo(
        self,
    ) -> CeloEvm<Self::Context, (), EthInstructions<EthInterpreter, Self::Context>>;

    /// Build the CeloEvm with an inspector.
    fn build_celo_with_inspector<INSP>(
        self,
        inspector: INSP,
    ) -> CeloEvm<Self::Context, INSP, EthInstructions<EthInterpreter, Self::Context>>;
}

impl<BLOCK, TX, CFG, DB, JOURNAL> CeloBuilder for Context<BLOCK, TX, CFG, DB, JOURNAL, CeloBlockEnv>
where
    BLOCK: Block,
    TX: CeloTxTr,
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
