/*
use crate::CeloTxEnv;
use op_revm::OpTransaction;

impl Default for OpTransaction<CeloTxEnv> {
    fn default() -> Self {
        Self {
            base: CeloTxEnv::default(),
            enveloped_tx: Some(vec![0x00].into()),
            deposit: DepositTransactionParts::default(),
        }
    }
}
*/
