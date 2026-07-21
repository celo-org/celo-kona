//! Provider-backed block-start fee-currency context resolver.
//!
//! The node's [`FeeContextResolver`]: given `(block_number, parent_hash)` it looks up the
//! canonical header at that height, verifies its parent, loads state at `parent_hash`, and runs
//! the fee-currency directory/oracle view calls under that block's env — exactly the load
//! consensus performs at tx 0. Non-canonical `(number, parent)` inputs (e.g. a forged
//! `debug_traceBlock` body) resolve to `None`; canonical ones, historical blocks included,
//! resolve to the true block-start context regardless of the traced body. See
//! [`alloy_celo_evm::fee_context_cache`] for why reth's per-tx `debug_trace*` replay EVMs need
//! this.

use alloc::sync::Arc;
use alloy_celo_evm::{CeloEvmFactory, fee_context_cache::FeeContextResolver};
use alloy_consensus::Header;
use alloy_evm::EvmFactory;
use alloy_primitives::B256;
use celo_revm::FeeCurrencyContext;
use reth_chainspec::EthChainSpec;
use reth_optimism_forks::OpHardforks;
use reth_revm::database::StateProviderDatabase;
use reth_storage_api::{HeaderProvider, StateProviderFactory};

/// A [`FeeContextResolver`] backed by the node's state provider.
///
/// Cheap to clone; wired onto the shared [`CeloEvmFactory`] at node build time. Memoizes nothing
/// itself — `CeloEvm::transact_raw` caches each resolved context in the factory-shared memo, so
/// `resolve` runs at most once per block per eviction window.
#[derive(Debug, Clone)]
pub struct ProviderFeeContextResolver<Provider, ChainSpec> {
    provider: Provider,
    chain_spec: Arc<ChainSpec>,
}

impl<Provider, ChainSpec> ProviderFeeContextResolver<Provider, ChainSpec> {
    /// Creates a resolver from a state-provider handle and the chain spec.
    pub const fn new(provider: Provider, chain_spec: Arc<ChainSpec>) -> Self {
        Self { provider, chain_spec }
    }
}

impl<Provider, ChainSpec> FeeContextResolver for ProviderFeeContextResolver<Provider, ChainSpec>
where
    Provider: StateProviderFactory + HeaderProvider<Header = Header> + Send + Sync + 'static,
    ChainSpec: EthChainSpec<Header = Header> + OpHardforks + Send + Sync + 'static,
{
    fn resolve(&self, block_number: u64, parent_hash: B256) -> Option<FeeCurrencyContext> {
        // A missing canonical header at this height, or a mismatched parent, means the pair is
        // not a canonical block (e.g. a forged `debug_traceBlock` body) — refuse.
        let header = self.provider.header_by_number(block_number).ok().flatten()?;
        if header.parent_hash != parent_hash {
            return None;
        }

        // Block-start state is the parent's post-state; run the directory/oracle view calls under
        // this block's own env, matching what consensus loads at tx 0.
        let state = self.provider.history_by_block_hash(parent_hash).ok()?;
        let env = alloy_op_evm::evm_env_for_op_block(
            &header,
            &self.chain_spec,
            (*self.chain_spec).chain().id(),
        );
        let db = StateProviderDatabase::new(state);
        let mut evm = CeloEvmFactory::default().create_evm(db, env);
        Some(evm.create_fee_currency_context())
    }
}
