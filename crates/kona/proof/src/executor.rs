//! An executor constructor.

use alloc::boxed::Box;
use alloy_consensus::{Header, Sealed};
use alloy_evm::{EvmFactory, FromRecoveredTx, FromTxWithEncoded};
use alloy_primitives::B256;
use async_trait::async_trait;
use celo_alloy_consensus::CeloTxEnvelope;
use celo_alloy_rpc_types_engine::CeloPayloadAttributes;
use celo_driver::CeloExecutorTr;
use celo_executor::{CeloBlockBuildingOutcome, CeloStatelessL2Builder};
use celo_genesis::CeloRollupConfig;
use kona_executor::TrieDBProvider;
use kona_mpt::TrieHinter;
use op_revm::OpSpecId;

/// An executor wrapper type.
#[derive(Debug)]
pub struct CeloExecutor<'a, P, H, Evm>
where
    P: TrieDBProvider + Send + Sync + Clone,
    H: TrieHinter + Send + Sync + Clone,
    Evm: EvmFactory + Send + Sync + Clone,
{
    /// The rollup config for the executor.
    rollup_config: &'a CeloRollupConfig,
    /// The trie provider for the executor.
    trie_provider: P,
    /// The trie hinter for the executor.
    trie_hinter: H,
    /// The evm factory for the executor.
    evm_factory: Evm,
    /// The executor.
    inner: Option<CeloStatelessL2Builder<'a, P, H, Evm>>,
}

impl<'a, P, H, Evm> CeloExecutor<'a, P, H, Evm>
where
    P: TrieDBProvider + Send + Sync + Clone,
    H: TrieHinter + Send + Sync + Clone,
    Evm: EvmFactory + Send + Sync + Clone,
{
    /// Creates a new executor.
    pub const fn new(
        rollup_config: &'a CeloRollupConfig,
        trie_provider: P,
        trie_hinter: H,
        evm_factory: Evm,
        inner: Option<CeloStatelessL2Builder<'a, P, H, Evm>>,
    ) -> Self {
        Self { rollup_config, trie_provider, trie_hinter, evm_factory, inner }
    }
}

#[async_trait]
impl<P, H, Evm> CeloExecutorTr for CeloExecutor<'_, P, H, Evm>
where
    P: TrieDBProvider + Send + Sync + Clone,
    H: TrieHinter + Send + Sync + Clone,
    Evm: EvmFactory<Spec = OpSpecId> + Send + Sync + Clone + 'static,
    <Evm as EvmFactory>::Tx: FromTxWithEncoded<CeloTxEnvelope> + FromRecoveredTx<CeloTxEnvelope>,
{
    type Error = kona_executor::ExecutorError;

    /// Waits for the executor to be ready.
    async fn wait_until_ready(&mut self) {
        /* no-op for the celo executor */
        /* This is used when an engine api is used instead of a stateless block executor */
    }

    /// Updates the safe header.
    ///
    /// Since the L2 block executor is stateless, on an update to the safe head,
    /// a new executor is created with the updated header.
    fn update_safe_head(&mut self, header: Sealed<Header>) {
        self.inner = Some(CeloStatelessL2Builder::new(
            self.rollup_config,
            self.evm_factory.clone(),
            self.trie_provider.clone(),
            self.trie_hinter.clone(),
            header,
        ));
    }

    /// Execute the given payload attributes.
    async fn execute_payload(
        &mut self,
        attributes: CeloPayloadAttributes,
    ) -> Result<CeloBlockBuildingOutcome, Self::Error> {
        self.inner.as_mut().map_or_else(
            || Err(kona_executor::ExecutorError::MissingExecutor),
            |e| e.build_block(attributes),
        )
    }

    /// Computes the output root.
    fn compute_output_root(&mut self) -> Result<B256, Self::Error> {
        self.inner.as_mut().map_or_else(
            || Err(kona_executor::ExecutorError::MissingExecutor),
            |e| e.compute_output_root(),
        )
    }
}
