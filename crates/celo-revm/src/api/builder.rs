use crate::{CeloContext, CeloEvm};
use revm::{
    Database, handler::instructions::EthInstructions, interpreter::interpreter::EthInterpreter,
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

impl<DB> CeloBuilder for CeloContext<DB>
where
    DB: Database,
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
