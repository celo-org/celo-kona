//! Batch Authentication via L1 Event Scanning
//!
//! This module implements event-based batch authentication for the Espresso integration.
//! Instead of relying on an on-chain `BatchInbox` contract to verify batches, the derivation
//! pipeline scans L1 receipts for `BatchInfoAuthenticated(bytes32 commitment, address indexed
//! caller)` events emitted by the `BatchAuthenticator` contract within a lookback window.
//!
//! Whether batches must be authorized by an event is gated by the Espresso hardfork
//! ([`celo_genesis::CeloEspressoConfig::espresso_time`]). The fork is conceptually an
//! L2-timestamp hardfork, but the per-L1-block decision made at the data source layer is gated
//! on the L1 origin time of the block being scanned — mirroring the upstream `ecotoneTime`
//! precedent.
//!
//! - **Pre-fork (or fork unset):** the pipeline runs vanilla OP Stack semantics. A batch is
//!   authorized iff its sender matches `batcher_address`. The `BatchAuthenticator` event lookback
//!   is bypassed entirely.
//! - **Post-fork:** a batch is authorized iff its commitment hash was authenticated by a
//!   `BatchInfoAuthenticated(bytes32 commitment, address indexed caller)` event emitted by the
//!   configured `BatchAuthenticator` contract within the lookback window AND the batch
//!   transaction's recovered L1 sender equals the `caller` that emitted that event. This
//!   caller-binding prevents one batcher from replaying a batch authenticated by another. On
//!   duplicate authentication of the same commitment within the lookback window, the newest event's
//!   caller wins.
//!
//! The authorization semantics must stay in lockstep with the op-node verifier (the Go batcher
//! emits the `BatchInfoAuthenticated` events that this module consumes).
//!
//! Using event scanning (rather than L1 contract state reads) keeps the derivation pipeline
//! compatible with the op-program fault proof environment, which can only access L1 block headers,
//! transactions, receipts, and blobs — not contract state.

use alloc::{collections::BTreeMap, vec::Vec};
use alloy_consensus::{Receipt, TxEnvelope, TxReceipt, transaction::SignerRecoverable};
use alloy_primitives::{Address, B256, b256, keccak256};
use celo_genesis::BATCH_AUTH_LOOKBACK_WINDOW;
use kona_derive::ChainProvider;
use kona_protocol::BlockInfo;
use lru::LruCache;

/// The `keccak256("BatchInfoAuthenticated(bytes32,address)")` event topic.
///
/// This is the event emitted by the `BatchAuthenticator` contract when a batch is authenticated.
/// The commitment hash is the first (unindexed) data argument, read from `Data[:32]`; the
/// authenticating `caller` is the single indexed topic (`Topics[1]`).
pub const BATCH_INFO_AUTHENTICATED_TOPIC: B256 =
    b256!("731978a77d438b0ea35a9034fb28d9cf9372e1649f18c213110adcfab65c5c5c");

/// Configuration for event-based batch authentication.
///
/// The lookback window is not part of this config: it is the hardcoded consensus constant
/// [`celo_genesis::BATCH_AUTH_LOOKBACK_WINDOW`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchAuthConfig {
    /// The L1 address of the `BatchAuthenticator` contract. Guaranteed non-zero by construction
    /// (see [`crate::CeloEthereumDataSource::new_from_parts`]).
    pub authenticator_address: Address,
    /// Activation timestamp (L2) for the Espresso event-only batch authorization hardfork.
    ///
    /// The fork is conceptually an L2-timestamp hardfork, but the per-L1-block decision made at
    /// the data source layer is gated on the L1 origin time of the block being scanned (see
    /// [`Self::is_active`]) — mirroring the upstream `ecotoneTime` precedent.
    pub espresso_time: u64,
}

impl BatchAuthConfig {
    /// Returns true when Espresso event-only batch authorization is active at the given L1 origin
    /// time, i.e. `l1_origin_time >= espresso_time`.
    ///
    /// Mirrors [`celo_genesis::CeloRollupConfig::is_espresso_active`] for the case where a
    /// `BatchAuthenticator` is configured.
    pub const fn is_active(&self, l1_origin_time: u64) -> bool {
        l1_origin_time >= self.espresso_time
    }
}

/// Computes `keccak256(calldata)`, matching the `BatchAuthenticator` contract's calldata batch
/// validation path.
pub fn compute_calldata_batch_hash(data: &[u8]) -> B256 {
    keccak256(data)
}

/// Computes `keccak256(concat(blob_hashes))`, matching the `BatchAuthenticator` contract's blob
/// batch validation path.
pub fn compute_blob_batch_hash(blob_hashes: &[B256]) -> B256 {
    let mut concatenated = Vec::with_capacity(32 * blob_hashes.len());
    for hash in blob_hashes {
        concatenated.extend_from_slice(hash.as_slice());
    }
    keccak256(&concatenated)
}

/// Extracts all authenticated batch commitments from a single block's receipts.
///
/// Scans for `BatchInfoAuthenticated(bytes32 commitment, address indexed caller)` events emitted
/// by `authenticator_addr` in successful receipts. Returns a map of commitment hash to the
/// `caller` address that authenticated it. Within a single block, a later log for the same
/// commitment overwrites an earlier one.
pub fn collect_auth_events_from_receipts(
    receipts: &[Receipt],
    authenticator_addr: Address,
) -> BTreeMap<B256, Address> {
    let topic0 = BATCH_INFO_AUTHENTICATED_TOPIC;
    let mut result = BTreeMap::new();
    for receipt in receipts {
        if !receipt.status() {
            continue;
        }
        for log in &receipt.logs {
            if log.address != authenticator_addr {
                continue;
            }
            // `BatchInfoAuthenticated(bytes32 commitment, address indexed caller)` carries the
            // commitment as the first (unindexed) data word and the `caller` as the single
            // indexed topic. Require the selector to match and at least two topics (selector +
            // caller) plus at least 32 data bytes for the commitment.
            if log.topics().len() >= 2 && log.topics()[0] == topic0 && log.data.data.len() >= 32 {
                let commitment = B256::from_slice(&log.data.data[..32]);
                let caller = Address::from_word(log.topics()[1]);
                result.insert(commitment, caller);
            }
        }
    }
    result
}

/// Scans L1 receipts in the inclusive range `[block_ref.number - BATCH_AUTH_LOOKBACK_WINDOW,
/// block_ref.number]` — that is `BATCH_AUTH_LOOKBACK_WINDOW + 1` blocks (the batch's L1 origin
/// block plus [`BATCH_AUTH_LOOKBACK_WINDOW`] ancestors) — and returns a map of all batch
/// commitment hashes that were authenticated via `BatchInfoAuthenticated` events to the `caller`
/// that authenticated them.
///
/// This boundary is consensus-critical and must match the op-node batcher/verifier exactly.
///
/// Traversal walks newest block first. When the same commitment is authenticated more than once
/// within the lookback window, the newest event's caller wins (a commitment is only inserted if
/// it is not already present, and the already-present entry came from a newer block).
///
/// This is called once per L1 block by the data source, and the returned map is checked
/// against each candidate batch transaction. This avoids rescanning the lookback window
/// for every individual batch transaction.
///
/// Results are cached per block hash in the provided LRU cache. For consecutive L1 blocks
/// the lookback windows overlap by `BATCH_AUTH_LOOKBACK_WINDOW - 1` blocks, so only one new
/// block's receipts need to be fetched on each call. The cache is keyed by block hash (not
/// number) so it is naturally reorg-safe.
pub async fn collect_authenticated_batches<CP: ChainProvider + Send>(
    provider: &mut CP,
    block_ref: &BlockInfo,
    authenticator_addr: Address,
    cache: &mut BatchAuthCache,
) -> Result<BTreeMap<B256, Address>, CP::Error> {
    let mut all_authenticated: BTreeMap<B256, Address> = BTreeMap::new();
    let mut current_hash = block_ref.hash;
    let mut current_number = block_ref.number;

    loop {
        // Check receipt cache first
        if let Some(cached) = cache.receipts.get(&current_hash) {
            // Newest-wins merge: traversal walks newest -> oldest, so a commitment already in the
            // accumulator came from a newer block and must not be overwritten by this older one.
            for (commitment, caller) in cached {
                all_authenticated.entry(*commitment).or_insert(*caller);
            }
        } else {
            // Cache miss: fetch receipts, extract events, cache the result
            let receipts = provider.receipts_by_hash(current_hash).await?;
            let events = collect_auth_events_from_receipts(&receipts, authenticator_addr);
            for (commitment, caller) in &events {
                all_authenticated.entry(*commitment).or_insert(*caller);
            }
            cache.receipts.put(current_hash, events);
        }

        if current_number == 0 || block_ref.number - current_number >= BATCH_AUTH_LOOKBACK_WINDOW {
            break;
        }

        // Walk backward using header to get parent hash
        let parent_hash = if let Some(&cached_parent) = cache.headers.get(&current_hash) {
            cached_parent
        } else {
            let header = provider.header_by_hash(current_hash).await?;
            cache.headers.put(current_hash, header.parent_hash);
            header.parent_hash
        };
        current_hash = parent_hash;
        current_number = current_number.saturating_sub(1);
    }

    Ok(all_authenticated)
}

/// LRU caches used during the batch authentication lookback window traversal.
///
/// Bundles the receipt-event cache and the header (block hash → parent hash) cache.
/// Both caches are sized slightly larger than the lookback window to avoid thrashing at the
/// boundary.
#[derive(Debug, Clone)]
pub struct BatchAuthCache {
    /// Authenticated batch commitments (commitment hash → authenticating `caller`) extracted from
    /// receipts, keyed by L1 block hash.
    pub receipts: LruCache<B256, BTreeMap<B256, Address>>,
    /// Block parent hashes keyed by block hash
    pub headers: LruCache<B256, B256>,
}

impl Default for BatchAuthCache {
    fn default() -> Self {
        Self::new()
    }
}

impl BatchAuthCache {
    /// Creates a new [`BatchAuthCache`] with both caches sized to
    /// [`BATCH_AUTH_LOOKBACK_WINDOW`] `+ 2`.
    pub fn new() -> Self {
        let cap = usize::try_from(BATCH_AUTH_LOOKBACK_WINDOW)
            .map(|w| w.saturating_add(2))
            .unwrap_or(usize::MAX);
        let cap = core::num::NonZeroUsize::new(cap).expect("cache size must be non-zero");
        Self { receipts: LruCache::new(cap), headers: LruCache::new(cap) }
    }
}

/// Checks whether a batch transaction is authorized.
///
/// Behaviour is gated by `auth_config` together with `l1_origin_time` (the L1 origin time of the
/// block being scanned):
///
/// - **Pre-fork / vanilla OP Stack** — `auth_config` is `None`, or it is `Some` but the fork is not
///   yet active at `l1_origin_time` ([`BatchAuthConfig::is_active`] is false): authorized iff the
///   transaction sender matches `batcher_address`. `authenticated_hashes` is ignored.
/// - **Post-fork** — `auth_config` is `Some` and active: authorized iff `batch_hash` is present in
///   `authenticated_hashes` AND the transaction's recovered L1 sender equals the `caller` that
///   authenticated that commitment. This caller-binding prevents one batcher from replaying a batch
///   authenticated by another.
///
/// Because the fork time lives inside [`BatchAuthConfig`], the post-fork (caller-bound) branch is
/// only reachable when an authenticator is configured — the "fork active but no authenticator"
/// state is unrepresentable.
///
/// If the gap between the authentication transaction and the batch data exceeds the configured
/// lookback window, it's the batcher's responsibility to detect this and re-submit the
/// authentication transaction and batch data.
pub fn is_batch_authorized(
    tx: &TxEnvelope,
    batch_hash: B256,
    auth_config: Option<&BatchAuthConfig>,
    authenticated_hashes: &BTreeMap<B256, Address>,
    batcher_address: Address,
    l1_origin_time: u64,
) -> bool {
    if let Some(config) = auth_config &&
        config.is_active(l1_origin_time)
    {
        // Post-fork: the commitment must be authenticated AND the recovered batch tx sender must
        // equal the `caller` that emitted the authenticating event. Sender-based fallback is
        // rejected.
        let Some(&caller) = authenticated_hashes.get(&batch_hash) else {
            return false;
        };
        return tx.recover_signer().map(|sender| sender == caller).unwrap_or(false);
    }
    // Pre-fork (or fork not yet active): vanilla OP Stack sender verification.
    tx.recover_signer().map(|sender| sender == batcher_address).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{vec, vec::Vec};
    use alloy_consensus::{Eip658Value, Header, Receipt, Signed, TxLegacy};
    use alloy_primitives::{Address, Log, LogData, Signature, TxKind, address, b256};
    use kona_derive::test_utils::TestChainProvider;

    fn make_auth_receipt(
        authenticator_addr: Address,
        commitment: B256,
        caller: Address,
    ) -> Receipt {
        let topic0 = BATCH_INFO_AUTHENTICATED_TOPIC;
        let log = Log {
            address: authenticator_addr,
            data: LogData::new_unchecked(
                vec![topic0, caller.into_word()],
                commitment.as_slice().to_vec().into(),
            ),
        };
        Receipt { status: Eip658Value::Eip658(true), logs: vec![log], ..Default::default() }
    }

    fn make_failed_auth_receipt(
        authenticator_addr: Address,
        commitment: B256,
        caller: Address,
    ) -> Receipt {
        let topic0 = BATCH_INFO_AUTHENTICATED_TOPIC;
        let log = Log {
            address: authenticator_addr,
            data: LogData::new_unchecked(
                vec![topic0, caller.into_word()],
                commitment.as_slice().to_vec().into(),
            ),
        };
        Receipt { status: Eip658Value::Eip658(false), logs: vec![log], ..Default::default() }
    }

    fn test_legacy_tx(to: Address) -> TxEnvelope {
        let sig = Signature::test_signature();
        TxEnvelope::Legacy(Signed::new_unchecked(
            TxLegacy { to: TxKind::Call(to), ..Default::default() },
            sig,
            Default::default(),
        ))
    }

    #[test]
    fn test_compute_calldata_batch_hash() {
        let data = b"hello world";
        let hash = compute_calldata_batch_hash(data);
        assert_eq!(hash, keccak256(data));
    }

    #[test]
    fn test_compute_calldata_batch_hash_empty() {
        // Empty calldata must hash to keccak256("") — the contract's calldata path and an empty
        // input produce the same digest on both sides.
        let hash = compute_calldata_batch_hash(&[]);
        assert_eq!(hash, keccak256([]));
    }

    #[test]
    fn test_compute_blob_batch_hash() {
        let h1 = b256!("0000000000000000000000000000000000000000000000000000000000000001");
        let h2 = b256!("0000000000000000000000000000000000000000000000000000000000000002");
        let hash = compute_blob_batch_hash(&[h1, h2]);

        let mut expected_input = Vec::new();
        expected_input.extend_from_slice(h1.as_slice());
        expected_input.extend_from_slice(h2.as_slice());
        assert_eq!(hash, keccak256(&expected_input));
    }

    #[test]
    fn test_compute_blob_batch_hash_single() {
        // A single blob hash must hash to keccak256 of that hash's bytes, i.e. concatenation is a
        // no-op for one element.
        let h = b256!("0000000000000000000000000000000000000000000000000000000000abcdef");
        let hash = compute_blob_batch_hash(&[h]);
        assert_eq!(hash, keccak256(h.as_slice()));
    }

    #[test]
    fn test_collect_auth_events_from_receipts_success() {
        let auth_addr = address!("1234567890123456789012345678901234567890");
        let commitment = b256!("abcdef0000000000000000000000000000000000000000000000000000000000");
        let caller = address!("00000000000000000000000000000000000000aa");

        let receipt = make_auth_receipt(auth_addr, commitment, caller);
        let result = collect_auth_events_from_receipts(&[receipt], auth_addr);

        assert_eq!(result.get(&commitment), Some(&caller));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_collect_auth_events_from_receipts_wrong_address() {
        let auth_addr = address!("1234567890123456789012345678901234567890");
        let wrong_addr = address!("0000000000000000000000000000000000000001");
        let commitment = b256!("abcdef0000000000000000000000000000000000000000000000000000000000");
        let caller = address!("00000000000000000000000000000000000000aa");

        let receipt = make_auth_receipt(wrong_addr, commitment, caller);
        let result = collect_auth_events_from_receipts(&[receipt], auth_addr);

        assert!(result.is_empty());
    }

    #[test]
    fn test_collect_auth_events_from_receipts_failed_receipt() {
        let auth_addr = address!("1234567890123456789012345678901234567890");
        let commitment = b256!("abcdef0000000000000000000000000000000000000000000000000000000000");
        let caller = address!("00000000000000000000000000000000000000aa");

        let receipt = make_failed_auth_receipt(auth_addr, commitment, caller);
        let result = collect_auth_events_from_receipts(&[receipt], auth_addr);

        assert!(result.is_empty());
    }

    #[test]
    fn test_collect_auth_events_multiple_events() {
        let auth_addr = address!("1234567890123456789012345678901234567890");
        let c1 = b256!("0000000000000000000000000000000000000000000000000000000000000001");
        let c2 = b256!("0000000000000000000000000000000000000000000000000000000000000002");
        let caller1 = address!("00000000000000000000000000000000000000aa");
        let caller2 = address!("00000000000000000000000000000000000000bb");

        let r1 = make_auth_receipt(auth_addr, c1, caller1);
        let r2 = make_auth_receipt(auth_addr, c2, caller2);
        let result = collect_auth_events_from_receipts(&[r1, r2], auth_addr);

        assert_eq!(result.len(), 2);
        assert_eq!(result.get(&c1), Some(&caller1));
        assert_eq!(result.get(&c2), Some(&caller2));
    }

    /// A `BatchAuthConfig` active from genesis (`espresso_time = 0`).
    fn active_config(authenticator_address: Address) -> BatchAuthConfig {
        BatchAuthConfig { authenticator_address, espresso_time: 0 }
    }

    // L1 origin time used as "post-fork" for an `active_config` (espresso_time = 0).
    const POST_FORK_TIME: u64 = 0;

    #[test]
    fn test_is_batch_authorized_post_fork_matching_caller_accepted() {
        let auth_addr = address!("1234567890123456789012345678901234567890");
        let config = active_config(auth_addr);
        let batch_hash = b256!("abcdef0000000000000000000000000000000000000000000000000000000000");

        let tx = test_legacy_tx(Address::ZERO);
        let sender = tx.recover_signer().unwrap();
        let mut authenticated = BTreeMap::new();
        // The commitment was authenticated by the tx's own sender.
        authenticated.insert(batch_hash, sender);

        // Post-fork, commitment authenticated and tx sender == authenticating caller: authorized.
        assert!(is_batch_authorized(
            &tx,
            batch_hash,
            Some(&config),
            &authenticated,
            Address::ZERO,
            POST_FORK_TIME,
        ));
    }

    #[test]
    fn test_is_batch_authorized_post_fork_caller_mismatch_rejected() {
        let auth_addr = address!("1234567890123456789012345678901234567890");
        let config = active_config(auth_addr);
        let batch_hash = b256!("abcdef0000000000000000000000000000000000000000000000000000000000");

        let tx = test_legacy_tx(Address::ZERO);
        let other_caller = address!("00000000000000000000000000000000000000aa");
        let mut authenticated = BTreeMap::new();
        // The commitment is authenticated, but by a different caller than the tx sender.
        authenticated.insert(batch_hash, other_caller);

        // Post-fork, commitment authenticated but tx sender != authenticating caller: rejected.
        assert!(!is_batch_authorized(
            &tx,
            batch_hash,
            Some(&config),
            &authenticated,
            Address::ZERO,
            POST_FORK_TIME,
        ));
    }

    #[test]
    fn test_is_batch_authorized_post_fork_no_event_rejected() {
        let auth_addr = address!("1234567890123456789012345678901234567890");
        let config = active_config(auth_addr);
        let batch_hash = b256!("abcdef0000000000000000000000000000000000000000000000000000000000");
        let authenticated = BTreeMap::new(); // empty

        let tx = test_legacy_tx(Address::ZERO);
        // Post-fork, commitment absent: rejected even for an empty map.
        assert!(!is_batch_authorized(
            &tx,
            batch_hash,
            Some(&config),
            &authenticated,
            Address::ZERO,
            POST_FORK_TIME,
        ));
    }

    #[test]
    fn test_is_batch_authorized_post_fork_sender_fallback_rejected() {
        // Even when sender matches batcher_address, post-fork requires an authenticating event
        // for the commitment.
        let batch_hash = B256::ZERO;
        let authenticated = BTreeMap::new();

        let tx = test_legacy_tx(Address::ZERO);
        let sender = tx.recover_signer().unwrap();
        // With an active auth_config but no matching event: rejected (no sender fallback).
        let auth_addr = address!("1234567890123456789012345678901234567890");
        let config = active_config(auth_addr);
        assert!(!is_batch_authorized(
            &tx,
            batch_hash,
            Some(&config),
            &authenticated,
            sender,
            POST_FORK_TIME,
        ));
    }

    #[test]
    fn test_is_batch_authorized_pre_fork_vanilla_sender_path() {
        let batch_hash = B256::ZERO;
        let authenticated = BTreeMap::new();

        let tx = test_legacy_tx(Address::ZERO);
        let sender = tx.recover_signer().unwrap();
        // No config (Espresso off): sender matches batcher_address => authorized.
        assert!(is_batch_authorized(&tx, batch_hash, None, &authenticated, sender, 0));
        // No config: sender mismatch => rejected.
        assert!(!is_batch_authorized(
            &tx,
            batch_hash,
            None,
            &authenticated,
            address!("0000000000000000000000000000000000000001"),
            0,
        ));
    }

    #[test]
    fn test_is_batch_authorized_not_yet_active_ignores_auth_event() {
        // Config present but fork not yet active at this L1 time: only the sender check is honored,
        // even if a matching auth event exists.
        let auth_addr = address!("1234567890123456789012345678901234567890");
        let config = BatchAuthConfig { authenticator_address: auth_addr, espresso_time: 1_000 };
        let batch_hash = b256!("abcdef0000000000000000000000000000000000000000000000000000000000");
        let mut authenticated = BTreeMap::new();

        let tx = test_legacy_tx(Address::ZERO);
        let sender = tx.recover_signer().unwrap();
        authenticated.insert(batch_hash, sender);
        let wrong_addr = address!("0000000000000000000000000000000000000001");
        let pre_fork_time = 999; // < espresso_time

        // Sender mismatch: even with an event present, we reject (event path is gated off).
        assert!(!is_batch_authorized(
            &tx,
            batch_hash,
            Some(&config),
            &authenticated,
            wrong_addr,
            pre_fork_time,
        ));
        // Sender match: authorized via the pre-fork path.
        assert!(is_batch_authorized(
            &tx,
            batch_hash,
            Some(&config),
            &authenticated,
            sender,
            pre_fork_time,
        ));
    }

    #[test]
    fn test_batch_info_authenticated_topic_is_correct() {
        assert_eq!(
            BATCH_INFO_AUTHENTICATED_TOPIC,
            keccak256("BatchInfoAuthenticated(bytes32,address)")
        );
    }

    #[test]
    fn test_new_batch_auth_cache() {
        let cache = BatchAuthCache::new();
        let expected_cap = (BATCH_AUTH_LOOKBACK_WINDOW as usize) + 2;
        assert_eq!(cache.receipts.len(), 0);
        assert_eq!(cache.receipts.cap().get(), expected_cap);
        assert_eq!(cache.headers.len(), 0);
        assert_eq!(cache.headers.cap().get(), expected_cap);
    }

    // ---- Multi-block lookback traversal -------------------------------------------------------
    //
    // These mirror op-node's `TestCollectAuthenticatedBatches`
    // (`espresso_batch_authenticator_test.go`). Every other test in this module operates on
    // `BlockInfo::default()` (block 0), so the lookback loop breaks after a single iteration and
    // never exercises the backward parent-hash walk, the cross-block newest-wins merge, the
    // genesis clamp, or the inclusive `[n - WINDOW, n]` window boundary. The chains below do.

    const TEST_AUTH_ADDR: Address = address!("1234567890123456789012345678901234567890");

    /// Builds a parent-linked L1 header chain for blocks `0..=top` and registers each block's
    /// header and receipts with `provider`, returning the [`BlockInfo`] for every block (indexed
    /// by number). Blocks absent from `receipts` get an empty receipt list — still inserted so
    /// [`TestChainProvider::receipts_by_hash`] resolves for every block the walk visits.
    ///
    /// Block hashes are the real `header.hash_slow()` and each header's `parent_hash` points at
    /// its predecessor, so [`collect_authenticated_batches`] performs a genuine backward walk.
    fn build_l1_chain(
        provider: &mut TestChainProvider,
        top: u64,
        receipts: &BTreeMap<u64, Vec<Receipt>>,
    ) -> Vec<BlockInfo> {
        let mut headers: Vec<Header> = Vec::with_capacity(top as usize + 1);
        let mut parent = B256::ZERO;
        for number in 0..=top {
            let header = Header { parent_hash: parent, number, ..Default::default() };
            parent = header.hash_slow();
            headers.push(header);
        }
        headers
            .into_iter()
            .map(|header| {
                let hash = header.hash_slow();
                let number = header.number;
                provider.insert_header(hash, header);
                provider.insert_receipts(hash, receipts.get(&number).cloned().unwrap_or_default());
                BlockInfo { hash, number, ..Default::default() }
            })
            .collect()
    }

    /// Convenience: a single-event receipt set for one block.
    fn auth_receipts(commitment: B256, caller: Address) -> Vec<Receipt> {
        vec![make_auth_receipt(TEST_AUTH_ADDR, commitment, caller)]
    }

    /// Event in the ref (newest) block itself is found. The ref sits one block above the lookback
    /// window (`number = WINDOW + 1`) so the walk terminates via the window-distance break rather
    /// than the genesis clamp — covering the realistic case where the ref block is scanned at a
    /// height `>= WINDOW`.
    #[tokio::test]
    async fn test_collect_authenticated_batches_event_in_ref_block() {
        let ref_number = BATCH_AUTH_LOOKBACK_WINDOW + 1;
        let commitment = b256!("00000000000000000000000000000000000000000000000000000000000000a1");
        let caller = address!("00000000000000000000000000000000000000aa");
        let mut provider = TestChainProvider::default();
        let mut receipts = BTreeMap::new();
        receipts.insert(ref_number, auth_receipts(commitment, caller));
        let infos = build_l1_chain(&mut provider, ref_number, &receipts);

        let mut cache = BatchAuthCache::new();
        let result = collect_authenticated_batches(
            &mut provider,
            &infos[ref_number as usize],
            TEST_AUTH_ADDR,
            &mut cache,
        )
        .await
        .unwrap();

        assert_eq!(result.get(&commitment), Some(&caller));
        assert_eq!(result.len(), 1);
    }

    /// Event in the *earliest* block of the window (`ref - BATCH_AUTH_LOOKBACK_WINDOW`) is found —
    /// the inclusive lower bound. Exercises the full-depth backward walk.
    #[tokio::test]
    async fn test_collect_authenticated_batches_event_in_earliest_window_block() {
        let window = BATCH_AUTH_LOOKBACK_WINDOW;
        let commitment = b256!("00000000000000000000000000000000000000000000000000000000000000b2");
        let caller = address!("00000000000000000000000000000000000000bb");
        let mut provider = TestChainProvider::default();
        let mut receipts = BTreeMap::new();
        // Block 0 == ref(window) - WINDOW: the oldest block still in range.
        receipts.insert(0, auth_receipts(commitment, caller));
        let infos = build_l1_chain(&mut provider, window, &receipts);

        let mut cache = BatchAuthCache::new();
        let result = collect_authenticated_batches(
            &mut provider,
            &infos[window as usize],
            TEST_AUTH_ADDR,
            &mut cache,
        )
        .await
        .unwrap();

        assert_eq!(result.get(&commitment), Some(&caller), "event at ref-WINDOW must be in range");
        assert_eq!(result.len(), 1);
    }

    /// Event one block *older* than the window (`ref - BATCH_AUTH_LOOKBACK_WINDOW - 1`) is
    /// excluded. Together with the previous test this pins the inclusive `[n - WINDOW, n]`
    /// boundary exactly.
    #[tokio::test]
    async fn test_collect_authenticated_batches_event_outside_window_excluded() {
        let window = BATCH_AUTH_LOOKBACK_WINDOW;
        let commitment = b256!("00000000000000000000000000000000000000000000000000000000000000c3");
        let caller = address!("00000000000000000000000000000000000000cc");
        let mut provider = TestChainProvider::default();
        let mut receipts = BTreeMap::new();
        // Block 0 == ref(window + 1) - WINDOW - 1: just outside the window.
        receipts.insert(0, auth_receipts(commitment, caller));
        let infos = build_l1_chain(&mut provider, window + 1, &receipts);

        let mut cache = BatchAuthCache::new();
        let result = collect_authenticated_batches(
            &mut provider,
            &infos[(window + 1) as usize],
            TEST_AUTH_ADDR,
            &mut cache,
        )
        .await
        .unwrap();

        assert!(result.is_empty(), "event one block older than the window must be excluded");
    }

    /// No authenticating event anywhere in the window: an empty (non-error) map.
    #[tokio::test]
    async fn test_collect_authenticated_batches_not_found_returns_empty() {
        let mut provider = TestChainProvider::default();
        let infos = build_l1_chain(&mut provider, 10, &BTreeMap::new());

        let mut cache = BatchAuthCache::new();
        let result =
            collect_authenticated_batches(&mut provider, &infos[10], TEST_AUTH_ADDR, &mut cache)
                .await
                .unwrap();

        assert!(result.is_empty());
    }

    /// When `ref.number < WINDOW`, the walk clamps at genesis (`current_number == 0`) without
    /// underflowing, and an event in block 0 is still found.
    #[tokio::test]
    async fn test_collect_authenticated_batches_window_clamps_at_genesis() {
        let commitment = b256!("00000000000000000000000000000000000000000000000000000000000000d4");
        let caller = address!("00000000000000000000000000000000000000dd");
        let mut provider = TestChainProvider::default();
        let mut receipts = BTreeMap::new();
        receipts.insert(0, auth_receipts(commitment, caller));
        // ref number 10 is far below WINDOW (100), so only the genesis guard can stop the walk.
        let infos = build_l1_chain(&mut provider, 10, &receipts);

        let mut cache = BatchAuthCache::new();
        let result =
            collect_authenticated_batches(&mut provider, &infos[10], TEST_AUTH_ADDR, &mut cache)
                .await
                .unwrap();

        assert_eq!(result.get(&commitment), Some(&caller));
        assert_eq!(result.len(), 1);
    }

    /// Distinct commitments authenticated in different blocks of the window each keep their own
    /// caller.
    #[tokio::test]
    async fn test_collect_authenticated_batches_multiple_commitments() {
        let c1 = b256!("0000000000000000000000000000000000000000000000000000000000000001");
        let c2 = b256!("0000000000000000000000000000000000000000000000000000000000000002");
        let caller1 = address!("00000000000000000000000000000000000000a1");
        let caller2 = address!("00000000000000000000000000000000000000a2");
        let mut provider = TestChainProvider::default();
        let mut receipts = BTreeMap::new();
        receipts.insert(3, auth_receipts(c1, caller1));
        receipts.insert(7, auth_receipts(c2, caller2));
        let infos = build_l1_chain(&mut provider, 10, &receipts);

        let mut cache = BatchAuthCache::new();
        let result =
            collect_authenticated_batches(&mut provider, &infos[10], TEST_AUTH_ADDR, &mut cache)
                .await
                .unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result.get(&c1), Some(&caller1));
        assert_eq!(result.get(&c2), Some(&caller2));
    }

    /// The same commitment authenticated in two blocks resolves to the *newest* block's caller —
    /// the newest-wins merge across the newest-first walk.
    #[tokio::test]
    async fn test_collect_authenticated_batches_newest_caller_wins() {
        let commitment = b256!("00000000000000000000000000000000000000000000000000000000000000e5");
        let older_caller = address!("00000000000000000000000000000000000000a0");
        let newer_caller = address!("00000000000000000000000000000000000000a9");
        let mut provider = TestChainProvider::default();
        let mut receipts = BTreeMap::new();
        receipts.insert(2, auth_receipts(commitment, older_caller));
        receipts.insert(8, auth_receipts(commitment, newer_caller));
        let infos = build_l1_chain(&mut provider, 10, &receipts);

        let mut cache = BatchAuthCache::new();
        let result =
            collect_authenticated_batches(&mut provider, &infos[10], TEST_AUTH_ADDR, &mut cache)
                .await
                .unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get(&commitment),
            Some(&newer_caller),
            "newest block's caller must win on duplicate commitments"
        );
    }
}
