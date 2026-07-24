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
use alloy_primitives::{Address, B256};
use celo_revm::FeeCurrencyContext;
use core::sync::atomic::{AtomicBool, Ordering};
use op_revm::OpSpecId;
use reth_chainspec::EthChainSpec;
use reth_optimism_forks::OpHardforks;
use reth_revm::database::StateProviderDatabase;
use reth_storage_api::{HeaderProvider, StateProviderFactory};
use revm::{
    Database,
    primitives::{StorageKey, StorageValue},
    state::{AccountInfo, Bytecode},
};

use crate::celo_next_block_base_fee;

/// A [`FeeContextResolver`] backed by the node's state provider.
///
/// Cheap to clone; wired onto the shared [`CeloEvmFactory`] at node build time. Memoizes nothing
/// itself — `CeloEvm::transact_raw` caches each resolved context in the factory-shared memo, so
/// `resolve` runs roughly once per block per eviction window (concurrent tracers racing the same
/// miss may each run it; the duplicate work is benign and all store the same value).
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
        match self.provider.header_by_number(block_number) {
            Ok(Some(header)) if header.parent_hash == parent_hash => {
                let env = evm_env_for_op_block(
                    &header,
                    &self.chain_spec,
                    (*self.chain_spec).chain().id(),
                );
                return self.context_from_parent_state(parent_hash, env);
            }
            // No canonical header at this height, or it has a different parent — fall through to
            // the canonical-parent path.
            Ok(_) => {}
            // Provider error: the parent path below may still succeed, but don't swallow it.
            Err(err) => tracing::warn!(
                target: "celo::fee_resolver",
                block = block_number,
                %err,
                "provider error reading canonical header while resolving fee context"
            ),
        }

        // Canonical parent: the block itself has no canonical header (yet) — mid-validation
        // prewarm, `eth_callBundle`, `debug_traceBadBlock` — but its parent does, and parent
        // post-state is block-start state for any child. Refuse pairs whose parent is unknown or
        // at the wrong height (e.g. a fully forged `debug_traceBlock` body).
        let parent = match self.provider.header(parent_hash) {
            // An unknown parent is an expected refusal (forged pair), not a provider fault.
            Ok(parent) => parent?,
            Err(err) => {
                tracing::warn!(
                    target: "celo::fee_resolver",
                    %parent_hash,
                    %err,
                    "provider error reading parent header; refusing to resolve fee context"
                );
                return None;
            }
        };
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
        let state = match self.provider.history_by_block_hash(parent_hash) {
            Ok(state) => state,
            Err(err) => {
                // Pruned history (non-archive node), non-canonical hash, or IO fault — the
                // ProviderError carries which; refusal itself is logged/counted at the EVM layer.
                tracing::warn!(
                    target: "celo::fee_resolver",
                    %parent_hash,
                    %err,
                    "no historical state for parent; refusing to resolve fee context"
                );
                return None;
            }
        };
        let saw_error = Arc::new(AtomicBool::new(false));
        let db = ErrorFlaggingDb {
            inner: StateProviderDatabase::new(state),
            saw_error: Arc::clone(&saw_error),
        };
        let mut evm = CeloEvmFactory::default().create_evm(db, env);
        let context = evm.create_fee_currency_context();
        if saw_error.load(Ordering::Relaxed) {
            tracing::warn!(
                target: "celo::fee_resolver",
                %parent_hash,
                "state read failed while loading the fee-currency context (partial parent \
                 state); refusing to resolve"
            );
            return None;
        }
        Some(context)
    }

    /// Env for a not-yet-canonical child of `parent`, built from parent-derived values only:
    /// number `parent.number + 1`, timestamp `parent.timestamp + 1` (Celo's 1s cadence; selects
    /// the hardfork spec), the parent's beneficiary/prevrandao/gas limit, and the Celo next-block
    /// base fee. The directory/oracle view calls read contract storage, not these env fields —
    /// the copied-from-parent ones the real child header would diverge on are exactly what
    /// `COINBASE`, `PREVRANDAO`, and `GASLIMIT` expose — so a synthesized value differing from
    /// the real child doesn't change the resolved context, and nothing caller-forgeable enters.
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

/// Wraps the resolver's state DB and flags every read error.
///
/// The context load (`get_currency_info` in `celo-revm`) deliberately swallows per-currency read
/// failures — a registered currency whose config can't be read is dropped from the context, the
/// right consensus-side behavior. On the resolver path that would memoize an empty/partial
/// context whenever the parent's state provider opens but its reads fail (e.g. partially pruned
/// history), and every trace of the block would then get a wrong "fee currency not registered"
/// error until eviction. The flag lets `context_from_parent_state` refuse instead.
struct ErrorFlaggingDb<D> {
    inner: D,
    saw_error: Arc<AtomicBool>,
}

// Manual impl (`alloy_evm::Database` requires it): the inner state provider need not be `Debug`.
impl<D> core::fmt::Debug for ErrorFlaggingDb<D> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ErrorFlaggingDb")
            .field("saw_error", &self.saw_error)
            .finish_non_exhaustive()
    }
}

impl<D: Database> Database for ErrorFlaggingDb<D> {
    type Error = D::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.inner.basic(address).inspect_err(|_| self.saw_error.store(true, Ordering::Relaxed))
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.inner
            .code_by_hash(code_hash)
            .inspect_err(|_| self.saw_error.store(true, Ordering::Relaxed))
    }

    fn storage(
        &mut self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        self.inner
            .storage(address, index)
            .inspect_err(|_| self.saw_error.store(true, Ordering::Relaxed))
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash(number).inspect_err(|_| self.saw_error.store(true, Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use celo_revm::test_utils::{TEST_FEE_CURRENCY, make_celo_test_db};
    use reth_optimism_chainspec::{OpChainSpec, OpChainSpecBuilder};
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};

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

    /// Loads the `celo-revm` directory/oracle state fixture (a registered fee currency at rate
    /// 20/10 with 50k intrinsic gas) into `provider`, so a resolve produces a *populated*
    /// context rather than the empty one MockEthProvider's default (empty) state yields.
    fn add_celo_state(provider: &MockEthProvider) {
        for (address, account) in make_celo_test_db().cache.accounts {
            let mut ext = ExtendedAccount::new(account.info.nonce, account.info.balance)
                .extend_storage(account.storage.into_iter().map(|(k, v)| (B256::from(k), v)));
            if let Some(code) = account.info.code {
                ext = ext.with_bytecode(code.original_bytes());
            }
            provider.add_account(address, ext);
        }
    }

    /// The other resolver tests resolve over empty state, so they verify routing but not that
    /// the parent state actually produced the block's rates. Resolve over a real directory
    /// deployment and assert the context carries the fixture currency's config.
    #[test]
    fn canonical_pair_resolves_populated_context() {
        let provider = MockEthProvider::default();
        add_celo_state(&provider);
        let parent_hash = B256::with_last_byte(1);
        provider.add_header(B256::with_last_byte(2), Header { parent_hash, ..header_at(5) });

        let context =
            resolver(provider).resolve(5, parent_hash).expect("canonical pair must resolve");
        assert_eq!(
            context.currency_exchange_rate(Some(TEST_FEE_CURRENCY)),
            Ok((U256::from(20), U256::from(10))),
        );
        assert_eq!(context.currency_intrinsic_gas_cost(Some(TEST_FEE_CURRENCY)), Ok(50_000));
    }

    /// Memo reuse across canonicalization rests on fallback-resolved ≡ canonical-resolved for
    /// the same `(number, parent_hash)`: an entry resolved via the synthesized next-block env
    /// (mid-validation) keeps being served after the block becomes canonical. Env-inertness is
    /// what guarantees it — pin the equality over real directory state.
    #[test]
    fn fallback_resolution_matches_canonical() {
        let parent_hash = B256::with_last_byte(1);

        let canonical = MockEthProvider::default();
        add_celo_state(&canonical);
        canonical.add_header(B256::with_last_byte(2), Header { parent_hash, ..header_at(5) });
        let canonical_ctx =
            resolver(canonical).resolve(5, parent_hash).expect("canonical pair must resolve");

        // Same pair, but no canonical header at the height: the parent-only fallback runs
        // under a synthesized env (different base fee/timestamp than the real header above).
        let fallback = MockEthProvider::default();
        add_celo_state(&fallback);
        fallback.add_header(parent_hash, header_at(4));
        let fallback_ctx =
            resolver(fallback).resolve(5, parent_hash).expect("canonical parent must resolve");

        assert_eq!(canonical_ctx.updated_at_block, fallback_ctx.updated_at_block);
        assert_eq!(
            canonical_ctx.currency_exchange_rate(Some(TEST_FEE_CURRENCY)),
            fallback_ctx.currency_exchange_rate(Some(TEST_FEE_CURRENCY)),
        );
        assert_eq!(
            canonical_ctx.currency_intrinsic_gas_cost(Some(TEST_FEE_CURRENCY)),
            fallback_ctx.currency_intrinsic_gas_cost(Some(TEST_FEE_CURRENCY)),
        );
        assert_eq!(
            canonical_ctx.currency_exchange_rate(Some(TEST_FEE_CURRENCY)),
            Ok((U256::from(20), U256::from(10))),
            "both paths must resolve the populated fixture config, not two empty contexts"
        );
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

    /// The invariant the partial-state refusal rests on: the context load swallows read errors
    /// (dropping the affected currencies), so `ErrorFlaggingDb` must be what surfaces them —
    /// otherwise an empty/partial context would be memoized and served until eviction.
    #[test]
    fn failing_state_read_sets_error_flag() {
        #[derive(Debug)]
        struct ReadError;
        impl core::fmt::Display for ReadError {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str("state read failed")
            }
        }
        impl core::error::Error for ReadError {}
        impl revm::database_interface::DBErrorMarker for ReadError {}

        struct FailingDb;
        impl Database for FailingDb {
            type Error = ReadError;

            fn basic(&mut self, _: Address) -> Result<Option<AccountInfo>, Self::Error> {
                Err(ReadError)
            }
            fn code_by_hash(&mut self, _: B256) -> Result<Bytecode, Self::Error> {
                Err(ReadError)
            }
            fn storage(&mut self, _: Address, _: StorageKey) -> Result<StorageValue, Self::Error> {
                Err(ReadError)
            }
            fn block_hash(&mut self, _: u64) -> Result<B256, Self::Error> {
                Err(ReadError)
            }
        }

        let saw_error = Arc::new(AtomicBool::new(false));
        let db = ErrorFlaggingDb { inner: FailingDb, saw_error: Arc::clone(&saw_error) };
        let mut evm = CeloEvmFactory::default().create_evm(db, EvmEnv::default());
        let context = evm.create_fee_currency_context();
        assert!(
            saw_error.load(Ordering::Relaxed),
            "a failing state read must set the flag; the load itself swallows the error \
             (context resolved anyway: {context:?})"
        );
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
