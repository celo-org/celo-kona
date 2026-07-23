//! Provider-backed block-start fee-currency context resolver.
//!
//! The node's [`FeeContextResolver`]: given `(block_number, parent_hash)`, it computes the
//! block-start fee-currency context purely from stored chain state, two ways:
//!
//! - **Canonical block** at that height with a matching parent — the `debug_trace*` / `trace_*` /
//!   `ots_*` replay paths: state at `parent_hash`, directory/oracle view calls under the block's
//!   own env — exactly the load consensus performed at tx 0.
//! - **Child of a known parent** (`block_number == parent.number + 1`, no canonical block at that
//!   height yet): reth's engine-tree prewarming of the payload currently being validated,
//!   `eth_callBundle` next-block bundles, pending-tag simulations, and `debug_traceBadBlock` /
//!   raw-RLP `debug_traceBlock` head extensions. The parent's post-state *is* such a block's
//!   block-start state, so the context still derives purely from chain state; only the env is
//!   synthesized, like the sequencer's next-block env, with the child timestamp guessed as parent +
//!   1s (Celo's block time). The guess can only matter for spec and base-fee-floor selection
//!   exactly at a hardfork boundary: exchange rates are contract state, and EIP-1559 base fees
//!   don't depend on the child timestamp.
//!
//! Anything else — unknown heights, forged `(number, parent)` pairs, parents whose state is
//! unavailable (pruned, or a non-canonical sidechain block real providers serve no historical
//! state for) — resolves to `None`. In every resolvable case the returned context derives solely
//! from stored chain state, never from a caller-supplied block body, so no RPC caller can
//! influence a resolved value. See [`alloy_celo_evm::fee_context_cache`] for why reth's per-tx
//! `debug_trace*` replay EVMs need this.

use alloc::sync::Arc;
use alloy_celo_evm::{CeloEvmFactory, fee_context_cache::FeeContextResolver};
use alloy_consensus::Header;
use alloy_evm::{EvmEnv, EvmFactory};
use alloy_primitives::B256;
use celo_revm::FeeCurrencyContext;
use op_revm::OpSpecId;
use reth_chainspec::EthChainSpec;
use reth_evm::eth::NextEvmEnvAttributes;
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

impl<Provider, ChainSpec> ProviderFeeContextResolver<Provider, ChainSpec>
where
    Provider: StateProviderFactory + HeaderProvider<Header = Header> + Send + Sync + 'static,
    ChainSpec: EthChainSpec<Header = Header> + OpHardforks + Send + Sync + 'static,
{
    /// Runs the fee-currency directory/oracle view calls over `parent_hash`'s post-state (the
    /// block-start state) under `env`.
    ///
    /// Parity caveat: this is the *raw* parent post-state, whereas consensus loads inside
    /// tx 0's handler — after this block's pre-transaction system calls (EIP-4788 beacon root,
    /// EIP-2935 block-hash history) have run. Parity therefore relies on the fee-currency
    /// directory/oracle calls not reading those system contracts — true today (disjoint
    /// contracts), but revisit if a fee-currency oracle ever reads that state.
    fn context_at_parent(
        &self,
        parent_hash: B256,
        env: EvmEnv<OpSpecId>,
    ) -> Option<FeeCurrencyContext> {
        let state = self.provider.history_by_block_hash(parent_hash).ok()?;
        let db = StateProviderDatabase::new(state);
        let mut evm = CeloEvmFactory::default().create_evm(db, env);
        Some(evm.create_fee_currency_context())
    }
}

impl<Provider, ChainSpec> FeeContextResolver for ProviderFeeContextResolver<Provider, ChainSpec>
where
    Provider: StateProviderFactory + HeaderProvider<Header = Header> + Send + Sync + 'static,
    ChainSpec: EthChainSpec<Header = Header> + OpHardforks + Send + Sync + 'static,
{
    fn resolve(&self, block_number: u64, parent_hash: B256) -> Option<FeeCurrencyContext> {
        let chain_id = (*self.chain_spec).chain().id();

        // Canonical block at this height with a matching parent: run the view calls under its
        // own env — exactly the load consensus performed at tx 0.
        if let Some(header) = self.provider.header_by_number(block_number).ok().flatten() &&
            header.parent_hash == parent_hash
        {
            let env = alloy_op_evm::evm_env_for_op_block(&header, &self.chain_spec, chain_id);
            return self.context_at_parent(parent_hash, env);
        }

        // No canonical block at this height with this parent, but the pair may name a *child of
        // a known parent* — a block that doesn't exist (yet): prewarming of the payload being
        // validated, a next-block bundle simulation, a bad-block head extension. Its block-start
        // state is the parent's post-state; synthesize the env the way the sequencer builds a
        // next-block env (see the module docs for the timestamp-guess caveat).
        let parent = self.provider.header(parent_hash).ok().flatten()?;
        if parent.number.checked_add(1) != Some(block_number) {
            return None;
        }
        let timestamp = parent.timestamp.saturating_add(1);
        let base_fee = celo_next_block_base_fee(&self.chain_spec, &parent, timestamp)?;
        let env = alloy_op_evm::evm_env_for_op_next_block(
            &parent,
            NextEvmEnvAttributes {
                timestamp,
                suggested_fee_recipient: parent.beneficiary,
                prev_randao: parent.mix_hash,
                gas_limit: parent.gas_limit,
                slot_number: None,
            },
            base_fee,
            &self.chain_spec,
            chain_id,
        );
        self.context_at_parent(parent_hash, env)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use reth_optimism_chainspec::{OpChainSpec, OpChainSpecBuilder};
    use reth_provider::test_utils::MockEthProvider;

    fn chain_spec() -> Arc<OpChainSpec> {
        Arc::new(
            OpChainSpecBuilder::default()
                .chain(reth_chainspec::Chain::from_id(42220))
                .genesis(Default::default())
                .granite_activated()
                .build(),
        )
    }

    fn resolver_with_headers(
        headers: &[(B256, Header)],
    ) -> ProviderFeeContextResolver<MockEthProvider, OpChainSpec> {
        let provider = MockEthProvider::default();
        for (hash, header) in headers {
            provider.add_header(*hash, header.clone());
        }
        ProviderFeeContextResolver::new(provider, chain_spec())
    }

    fn header(number: u64, parent_hash: B256) -> Header {
        Header {
            number,
            parent_hash,
            timestamp: 1_000 + number,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        }
    }

    const PARENT: B256 = B256::repeat_byte(1);
    const GRANDPARENT: B256 = B256::repeat_byte(2);

    /// Canonical path: a stored block at the height whose parent matches resolves, and the
    /// context is stamped with the requested block number (the property the memo key and the
    /// handler's `updated_at_block` short-circuit both rest on).
    #[test]
    fn canonical_block_resolves_under_its_own_env() {
        let child_hash = B256::repeat_byte(3);
        let resolver = resolver_with_headers(&[
            (PARENT, header(9, GRANDPARENT)),
            (child_hash, header(10, PARENT)),
        ]);
        let ctx = resolver.resolve(10, PARENT).expect("canonical pair must resolve");
        assert_eq!(ctx.updated_at_block, Some(U256::from(10)));
    }

    /// Fallback path: no block at the height, but the parent is known one level below — the
    /// prewarm / next-block-bundle / bad-block-head-extension shape. Resolves from parent
    /// post-state under a synthetic next-block env, stamped with the requested number.
    #[test]
    fn child_of_known_parent_resolves_via_fallback() {
        let resolver = resolver_with_headers(&[(PARENT, header(9, GRANDPARENT))]);
        let ctx = resolver.resolve(10, PARENT).expect("child of a known parent must resolve");
        assert_eq!(ctx.updated_at_block, Some(U256::from(10)));
    }

    /// A forged pair — nothing stored at the height, parent unknown — must refuse.
    #[test]
    fn unknown_number_and_parent_refuses() {
        let resolver = resolver_with_headers(&[]);
        assert!(resolver.resolve(10, PARENT).is_none());
    }

    /// A stored block at the height whose parent does NOT match the request, with the requested
    /// parent unknown, must refuse (same-height sibling forgery).
    #[test]
    fn parent_mismatch_refuses() {
        let child_hash = B256::repeat_byte(3);
        let resolver = resolver_with_headers(&[(child_hash, header(10, GRANDPARENT))]);
        assert!(resolver.resolve(10, PARENT).is_none());
    }

    /// The fallback only covers direct children: a known parent more than one level below the
    /// requested height must refuse.
    #[test]
    fn non_child_heights_refuse() {
        let resolver = resolver_with_headers(&[(PARENT, header(9, GRANDPARENT))]);
        assert!(resolver.resolve(15, PARENT).is_none());
        assert!(resolver.resolve(9, PARENT).is_none(), "same height as the parent is not a child");
    }
}
