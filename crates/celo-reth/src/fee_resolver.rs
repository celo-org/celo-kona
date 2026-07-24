//! Provider-backed block-start fee-currency context resolver.
//!
//! The node's [`FeeContextResolver`]: given `(block_number, parent_hash)` it runs the
//! fee-currency directory/oracle view calls over the parent's post-state — which *is* the
//! block's start state — under the block's env, exactly the load consensus performs at tx 0.
//! Everything derives from the node's own chain, never from a caller-supplied body, so no RPC
//! caller can influence a resolved value. Two shapes, tried in order:
//!
//! 1. **Canonical block** — the canonical header at `block_number` has parent `parent_hash`
//!    (historical tracing): the view calls run under that header's real env.
//! 2. **Canonical parent only** — no canonical header at this height matches, but `parent_hash`
//!    itself is a known block at `block_number - 1`: the block currently being validated
//!    (engine-tree prewarm EVMs), `eth_callBundle` targeting `stateBlockNumber + 1`, or
//!    `debug_traceBadBlock` of a rejected sibling. The env is synthesized from the parent (see
//!    `next_block_env`); the state provider still refuses non-canonical parents.
//!
//! Pairs whose parent is unknown or at the wrong height (e.g. a fully forged `debug_traceBlock`
//! body) resolve to `None`. See [`alloy_celo_evm::fee_context_cache`] for why reth's per-tx
//! `debug_trace*` replay EVMs need this.

use alloc::sync::Arc;
use alloy_celo_evm::{CeloEvmFactory, fee_context_cache::FeeContextResolver};
use alloy_consensus::Header;
use alloy_evm::{EvmEnv, EvmFactory, eth::NextEvmEnvAttributes};
use alloy_op_evm::{evm_env_for_op_block, evm_env_for_op_next_block};
use alloy_primitives::B256;
use celo_revm::FeeCurrencyContext;
use op_revm::OpSpecId;
use reth_chainspec::EthChainSpec;
use reth_optimism_forks::OpHardforks;
use reth_revm::database::StateProviderDatabase;
use reth_storage_api::{HeaderProvider, StateProviderFactory};

use crate::celo_next_block_base_fee;

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
        // Canonical block: the canonical header at this height matches the pair — run the view
        // calls under its real env.
        if let Ok(Some(header)) = self.provider.header_by_number(block_number) &&
            header.parent_hash == parent_hash
        {
            let env =
                evm_env_for_op_block(&header, &self.chain_spec, (*self.chain_spec).chain().id());
            return self.context_from_parent_state(parent_hash, env);
        }

        // Canonical parent: the block itself has no canonical header (yet) — mid-validation
        // prewarm, `eth_callBundle`, `debug_traceBadBlock` — but its parent does, and parent
        // post-state is block-start state for any child. Refuse pairs whose parent is unknown or
        // at the wrong height (e.g. a fully forged `debug_traceBlock` body).
        let parent = self.provider.header(parent_hash).ok().flatten()?;
        if Some(block_number) != parent.number.checked_add(1) {
            return None;
        }
        let env = self.next_block_env(&parent)?;
        self.context_from_parent_state(parent_hash, env)
    }
}

impl<Provider, ChainSpec> ProviderFeeContextResolver<Provider, ChainSpec>
where
    Provider: StateProviderFactory,
    ChainSpec: EthChainSpec<Header = Header> + OpHardforks,
{
    /// Runs the fee-currency directory/oracle view calls over the parent's post-state (block-start
    /// state) under `env`. The state provider only serves canonical hashes, so a non-canonical
    /// parent fails here and the caller refuses.
    ///
    /// Parity caveat: this is the *raw* parent post-state, whereas consensus loads inside
    /// tx 0's handler — after this block's pre-transaction system calls (EIP-4788 beacon root,
    /// EIP-2935 block-hash history) have run. Parity therefore relies on the fee-currency
    /// directory/oracle calls not reading those system contracts — true today (disjoint
    /// contracts), but revisit if a fee-currency oracle ever reads that state.
    fn context_from_parent_state(
        &self,
        parent_hash: B256,
        env: EvmEnv<OpSpecId>,
    ) -> Option<FeeCurrencyContext> {
        let state = self.provider.history_by_block_hash(parent_hash).ok()?;
        let db = StateProviderDatabase::new(state);
        let mut evm = CeloEvmFactory::default().create_evm(db, env);
        Some(evm.create_fee_currency_context())
    }

    /// Env for a not-yet-canonical child of `parent`, built from parent-derived values only:
    /// number `parent.number + 1`, timestamp `parent.timestamp + 1` (Celo's 1s cadence; selects
    /// the hardfork spec), the parent's beneficiary/prevrandao/gas limit, and the Celo next-block
    /// base fee. The directory/oracle view calls read contract storage, not these env fields, so
    /// a synthesized value differing from the real child header doesn't change the resolved
    /// context — and nothing caller-forgeable enters.
    fn next_block_env(&self, parent: &Header) -> Option<EvmEnv<OpSpecId>> {
        let timestamp = parent.timestamp.saturating_add(1);
        let base_fee = celo_next_block_base_fee(&self.chain_spec, parent, timestamp)?;
        let attributes = NextEvmEnvAttributes {
            timestamp,
            suggested_fee_recipient: parent.beneficiary,
            prev_randao: parent.mix_hash,
            gas_limit: parent.gas_limit,
            slot_number: None,
        };
        Some(evm_env_for_op_next_block(
            parent,
            attributes,
            base_fee,
            &self.chain_spec,
            (*self.chain_spec).chain().id(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use reth_optimism_chainspec::{OpChainSpec, OpChainSpecBuilder};
    use reth_provider::test_utils::MockEthProvider;

    fn resolver(
        provider: MockEthProvider,
    ) -> ProviderFeeContextResolver<MockEthProvider, OpChainSpec> {
        let chain_spec = Arc::new(
            OpChainSpecBuilder::default()
                .chain(reth_chainspec::Chain::from_id(42220))
                .genesis(Default::default())
                .granite_activated()
                .build(),
        );
        ProviderFeeContextResolver::new(provider, chain_spec)
    }

    /// A header rich enough for `next_block_env`: base-fee derivation needs the parent's base
    /// fee and gas values.
    fn header_at(number: u64) -> Header {
        Header {
            number,
            base_fee_per_gas: Some(50_000_000_000),
            gas_limit: 30_000_000,
            gas_used: 15_000_000,
            timestamp: 10,
            ..Default::default()
        }
    }

    #[test]
    fn canonical_pair_resolves() {
        let provider = MockEthProvider::default();
        let parent_hash = B256::with_last_byte(1);
        provider.add_header(B256::with_last_byte(2), Header { parent_hash, ..header_at(5) });

        let context =
            resolver(provider).resolve(5, parent_hash).expect("canonical pair must resolve");
        assert_eq!(context.updated_at_block, Some(U256::from(5)));
    }

    #[test]
    fn missing_child_with_canonical_parent_resolves_from_parent() {
        // No header at the requested height (a block mid-validation, e.g. prewarm), but the
        // parent is known: resolve from parent post-state under a synthesized next-block env.
        let provider = MockEthProvider::default();
        let parent_hash = B256::with_last_byte(1);
        provider.add_header(parent_hash, header_at(4));

        let context =
            resolver(provider).resolve(5, parent_hash).expect("canonical parent must resolve");
        assert_eq!(context.updated_at_block, Some(U256::from(5)));
    }

    #[test]
    fn unknown_pair_refuses() {
        let provider = MockEthProvider::default();
        assert!(resolver(provider).resolve(5, B256::with_last_byte(9)).is_none());
    }

    #[test]
    fn parent_at_wrong_height_refuses() {
        let provider = MockEthProvider::default();
        let parent_hash = B256::with_last_byte(1);
        provider.add_header(parent_hash, header_at(4));
        assert!(resolver(provider).resolve(10, parent_hash).is_none());
    }

    #[test]
    fn canonical_block_with_unknown_claimed_parent_refuses() {
        // A canonical block exists at the height, but the traced pair claims a different,
        // unknown parent — e.g. a forged body.
        let provider = MockEthProvider::default();
        provider.add_header(
            B256::with_last_byte(2),
            Header { parent_hash: B256::with_last_byte(1), ..header_at(5) },
        );
        assert!(resolver(provider).resolve(5, B256::with_last_byte(9)).is_none());
    }
}
