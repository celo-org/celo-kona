use crate::{CeloContext, CeloEvm};
use revm::{Database, inspector::NoOpInspector};

/// Trait that allows for celo CeloEvm to be built.
pub trait CeloBuilder: Sized {
    type DB: revm::Database;

    /// Build the CeloEvm.
    fn build_celo(self) -> CeloEvm<Self::DB, NoOpInspector>;

    /// Build the CeloEvm with an inspector.
    fn build_celo_with_inspector<INSP>(self, inspector: INSP) -> CeloEvm<Self::DB, INSP>;
}

impl<DB: Database> CeloBuilder for CeloContext<DB> {
    type DB = DB;

    fn build_celo(self) -> CeloEvm<DB, NoOpInspector> {
        CeloEvm::new(self, NoOpInspector {})
    }

    fn build_celo_with_inspector<INSP>(self, inspector: INSP) -> CeloEvm<DB, INSP> {
        CeloEvm::new(self, inspector)
    }
}
