//! Celo `CallData` Source with Espresso event-based batch authentication.
//!
//! Duplicated from kona's `CalldataSource` (celo-kona wraps upstream kona instead of patching it)
//! with the batch-authentication branch from celo-org/optimism#449 folded in. Pre-Espresso
//! behaviour is byte-identical to upstream; post-Espresso, batches are authorized by
//! `BatchInfoAuthenticated` events instead of by transaction sender.

use crate::batch_auth::{
    BatchAuthCache, BatchAuthConfig, collect_authenticated_batches, compute_calldata_batch_hash,
    is_batch_authorized,
};
use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
};
use alloy_consensus::{Transaction, TxEnvelope};
use alloy_primitives::{Address, B256, Bytes};
use async_trait::async_trait;
use kona_derive::{ChainProvider, DataAvailabilityProvider, PipelineError, PipelineResult};
use kona_protocol::BlockInfo;

/// A data iterator that reads from calldata, with optional Espresso event-based authentication.
#[derive(Debug, Clone)]
pub struct CeloCalldataSource<CP>
where
    CP: ChainProvider + Send,
{
    /// The chain provider to use for the calldata source.
    pub chain_provider: CP,
    /// The batch inbox address.
    pub batch_inbox_address: Address,
    /// Current calldata.
    pub calldata: VecDeque<Bytes>,
    /// Whether the calldata source is open.
    pub open: bool,
    /// Espresso batch-authentication configuration. When `Some` and active for the L1 origin time
    /// of the block being scanned, event-based batch authentication is used. Otherwise (no config,
    /// or fork not yet active) the source falls back to vanilla OP Stack sender verification.
    pub batch_auth_config: Option<BatchAuthConfig>,
    /// LRU caches for batch auth lookback window traversal (receipts + headers). Present iff
    /// [`Self::batch_auth_config`] is set.
    pub(crate) auth_cache: Option<BatchAuthCache>,
}

impl<CP: ChainProvider + Send> CeloCalldataSource<CP> {
    /// Creates a new calldata source.
    pub fn new(
        chain_provider: CP,
        batch_inbox_address: Address,
        batch_auth_config: Option<BatchAuthConfig>,
    ) -> Self {
        let auth_cache = batch_auth_config.map(|_| BatchAuthCache::new());
        Self {
            chain_provider,
            batch_inbox_address,
            calldata: VecDeque::new(),
            open: false,
            batch_auth_config,
            auth_cache,
        }
    }

    /// Loads the calldata into the source if it is not open.
    async fn load_calldata(
        &mut self,
        block_ref: &BlockInfo,
        batcher_address: Address,
    ) -> Result<(), CP::Error> {
        if self.open {
            return Ok(());
        }

        let (_, txs) =
            self.chain_provider.block_info_and_transactions_by_hash(block_ref.hash).await?;

        // Only scan for authenticating events when Espresso is active. Pre-Espresso (or Espresso
        // not yet active) the lookback walk is bypassed entirely so derivation is
        // byte-identical to upstream OP Stack (the BatchAuthenticator events are still
        // emitted on L1 but ignored).
        let espresso_active =
            self.batch_auth_config.is_some_and(|c| c.is_active(block_ref.timestamp));
        let authenticated_hashes: BTreeMap<B256, Address> = if espresso_active {
            let config = self.batch_auth_config.expect("config present when espresso active");
            let cache = self.auth_cache.as_mut().expect("cache present when config present");
            collect_authenticated_batches(
                &mut self.chain_provider,
                block_ref,
                config.authenticator_address,
                cache,
            )
            .await?
        } else {
            BTreeMap::new()
        };

        self.calldata = txs
            .iter()
            .filter_map(|tx| {
                let (tx_kind, data) = match tx {
                    TxEnvelope::Legacy(tx) => (tx.tx().to(), tx.tx().input()),
                    TxEnvelope::Eip2930(tx) => (tx.tx().to(), tx.tx().input()),
                    TxEnvelope::Eip1559(tx) => (tx.tx().to(), tx.tx().input()),
                    TxEnvelope::Eip4844(tx) => (tx.tx().to(), tx.tx().input()),
                    _ => return None,
                };
                let to = tx_kind?;

                if to != self.batch_inbox_address {
                    return None;
                }
                if !is_batch_authorized(
                    tx,
                    compute_calldata_batch_hash(data),
                    self.batch_auth_config.as_ref(),
                    &authenticated_hashes,
                    batcher_address,
                    block_ref.timestamp,
                ) {
                    return None;
                }
                Some(data.to_vec().into())
            })
            .collect::<VecDeque<_>>();

        self.open = true;

        Ok(())
    }
}

#[async_trait]
impl<CP: ChainProvider + Send> DataAvailabilityProvider for CeloCalldataSource<CP> {
    type Item = Bytes;

    async fn next(
        &mut self,
        block_ref: &BlockInfo,
        batcher_address: Address,
    ) -> PipelineResult<Self::Item> {
        self.load_calldata(block_ref, batcher_address).await.map_err(Into::into)?;
        self.calldata.pop_front().ok_or(PipelineError::Eof.temp())
    }

    fn clear(&mut self) {
        self.calldata.clear();
        self.open = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch_auth::BATCH_INFO_AUTHENTICATED_TOPIC;
    use alloc::{vec, vec::Vec};
    use alloy_consensus::{
        Eip658Value, Receipt, Signed, TxEip2930, TxEip4844, TxEip4844Variant, TxEip7702, TxLegacy,
        transaction::SignerRecoverable,
    };
    use alloy_primitives::{Address, Log, LogData, Signature, TxKind, address, keccak256};
    use kona_derive::{PipelineErrorKind, test_utils::TestChainProvider};

    fn test_legacy_tx(to: Address) -> TxEnvelope {
        let sig = Signature::test_signature();
        TxEnvelope::Legacy(Signed::new_unchecked(
            TxLegacy { to: TxKind::Call(to), ..Default::default() },
            sig,
            Default::default(),
        ))
    }

    fn test_eip2930_tx(to: Address) -> TxEnvelope {
        let sig = Signature::test_signature();
        TxEnvelope::Eip2930(Signed::new_unchecked(
            TxEip2930 { to: TxKind::Call(to), ..Default::default() },
            sig,
            Default::default(),
        ))
    }

    fn test_eip7702_tx(to: Address) -> TxEnvelope {
        let sig = Signature::test_signature();
        TxEnvelope::Eip7702(Signed::new_unchecked(
            TxEip7702 { to, ..Default::default() },
            sig,
            Default::default(),
        ))
    }

    fn test_blob_tx(to: Address) -> TxEnvelope {
        let sig = Signature::test_signature();
        TxEnvelope::Eip4844(Signed::new_unchecked(
            TxEip4844Variant::TxEip4844(TxEip4844 { to, ..Default::default() }),
            sig,
            Default::default(),
        ))
    }

    fn default_test_calldata_source() -> CeloCalldataSource<TestChainProvider> {
        CeloCalldataSource::new(TestChainProvider::default(), Default::default(), None)
    }

    /// A `BatchAuthConfig` active from genesis (`espresso_time = 0`).
    fn auth_config(authenticator_address: Address) -> BatchAuthConfig {
        BatchAuthConfig { authenticator_address, espresso_time: 0 }
    }

    #[tokio::test]
    async fn test_clear_calldata() {
        let mut source = default_test_calldata_source();
        source.open = true;
        source.calldata.push_back(Bytes::default());
        source.clear();
        assert!(source.calldata.is_empty());
        assert!(!source.open);
    }

    // --- Tests ported verbatim (adapted to the 3-arg constructor) from kona's `CalldataSource`,
    // exercising the tx-type filtering and error paths that are unchanged from upstream. ---

    #[tokio::test]
    async fn test_load_calldata_open() {
        let mut source = default_test_calldata_source();
        source.open = true;
        assert!(source.load_calldata(&BlockInfo::default(), Address::ZERO).await.is_ok());
    }

    #[tokio::test]
    async fn test_load_calldata_provider_err() {
        let mut source = default_test_calldata_source();
        assert!(source.load_calldata(&BlockInfo::default(), Address::ZERO).await.is_err());
    }

    #[tokio::test]
    async fn test_load_calldata_chain_provider_empty_txs() {
        let mut source = default_test_calldata_source();
        let block_info = BlockInfo::default();
        source.chain_provider.insert_block_with_transactions(0, block_info, Vec::new());
        assert!(!source.open);
        assert!(source.load_calldata(&BlockInfo::default(), Address::ZERO).await.is_ok());
        assert!(source.calldata.is_empty());
        assert!(source.open);
    }

    #[tokio::test]
    async fn test_load_calldata_wrong_batch_inbox_address() {
        let batch_inbox_address = address!("0123456789012345678901234567890123456789");
        let mut source = default_test_calldata_source();
        let block_info = BlockInfo::default();
        let tx = test_legacy_tx(batch_inbox_address);
        source.chain_provider.insert_block_with_transactions(0, block_info, vec![tx]);
        assert!(!source.open);
        // Source `batch_inbox_address` is the default (zero), so the tx is filtered out.
        assert!(source.load_calldata(&BlockInfo::default(), Address::ZERO).await.is_ok());
        assert!(source.calldata.is_empty());
        assert!(source.open);
    }

    #[tokio::test]
    async fn test_load_calldata_valid_eip2930_tx() {
        let batch_inbox_address = address!("0123456789012345678901234567890123456789");
        let mut source = default_test_calldata_source();
        source.batch_inbox_address = batch_inbox_address;
        let tx = test_eip2930_tx(batch_inbox_address);
        let block_info = BlockInfo::default();
        source.chain_provider.insert_block_with_transactions(0, block_info, vec![tx.clone()]);
        source.load_calldata(&BlockInfo::default(), tx.recover_signer().unwrap()).await.unwrap();
        assert_eq!(source.calldata.len(), 1);
        assert!(source.open);
    }

    #[tokio::test]
    async fn test_load_calldata_valid_blob_tx() {
        let batch_inbox_address = address!("0123456789012345678901234567890123456789");
        let mut source = default_test_calldata_source();
        source.batch_inbox_address = batch_inbox_address;
        let tx = test_blob_tx(batch_inbox_address);
        let block_info = BlockInfo::default();
        source.chain_provider.insert_block_with_transactions(0, block_info, vec![tx.clone()]);
        source.load_calldata(&BlockInfo::default(), tx.recover_signer().unwrap()).await.unwrap();
        assert_eq!(source.calldata.len(), 1);
        assert!(source.open);
    }

    #[tokio::test]
    async fn test_load_calldata_eip7702_tx_ignored() {
        let batch_inbox_address = address!("0123456789012345678901234567890123456789");
        let mut source = default_test_calldata_source();
        source.batch_inbox_address = batch_inbox_address;
        let tx = test_eip7702_tx(batch_inbox_address);
        let block_info = BlockInfo::default();
        source.chain_provider.insert_block_with_transactions(0, block_info, vec![tx.clone()]);
        // EIP-7702 txs are not a recognized batch-carrier and must be ignored.
        source.load_calldata(&BlockInfo::default(), tx.recover_signer().unwrap()).await.unwrap();
        assert!(source.calldata.is_empty());
        assert!(source.open);
    }

    #[tokio::test]
    async fn test_next_err_loading_calldata() {
        let mut source = default_test_calldata_source();
        assert!(matches!(
            source.next(&BlockInfo::default(), Address::ZERO).await,
            Err(PipelineErrorKind::Temporary(_))
        ));
    }

    #[tokio::test]
    async fn test_pre_espresso_valid_sender() {
        let batch_inbox = address!("0123456789012345678901234567890123456789");
        let mut source = default_test_calldata_source();
        source.batch_inbox_address = batch_inbox;
        let tx = test_legacy_tx(batch_inbox);
        let block_info = BlockInfo::default();
        source.chain_provider.insert_block_with_transactions(0, block_info, vec![tx.clone()]);
        // Pre-Espresso: sender must match the batcher address.
        source.load_calldata(&BlockInfo::default(), tx.recover_signer().unwrap()).await.unwrap();
        assert_eq!(source.calldata.len(), 1);
    }

    #[tokio::test]
    async fn test_pre_espresso_wrong_sender_rejected() {
        let batch_inbox = address!("0123456789012345678901234567890123456789");
        let mut source = default_test_calldata_source();
        source.batch_inbox_address = batch_inbox;
        let tx = test_legacy_tx(batch_inbox);
        let block_info = BlockInfo::default();
        source.chain_provider.insert_block_with_transactions(0, block_info, vec![tx]);
        source.load_calldata(&BlockInfo::default(), Address::ZERO).await.unwrap();
        assert!(source.calldata.is_empty());
    }

    #[tokio::test]
    async fn test_post_espresso_requires_event() {
        let batch_inbox = address!("0123456789012345678901234567890123456789");
        let auth_addr = address!("00000000000000000000000000000000000000aa");
        let tx = test_legacy_tx(batch_inbox);
        let sender = tx.recover_signer().unwrap();
        let data = match &tx {
            TxEnvelope::Legacy(t) => t.tx().input.clone(),
            _ => unreachable!(),
        };
        let commitment = keccak256(&data);

        // Build a block whose receipts contain the authenticating event. The commitment is the
        // unindexed data word; the authenticating `caller` (= the batch tx sender) is the indexed
        // topic.
        let mut source = CeloCalldataSource::new(
            TestChainProvider::default(),
            batch_inbox,
            Some(auth_config(auth_addr)),
        );

        let block_info = BlockInfo::default();
        let log = Log {
            address: auth_addr,
            data: LogData::new_unchecked(
                vec![BATCH_INFO_AUTHENTICATED_TOPIC, sender.into_word()],
                commitment.as_slice().to_vec().into(),
            ),
        };
        let receipt =
            Receipt { status: Eip658Value::Eip658(true), logs: vec![log], ..Default::default() };
        source.chain_provider.insert_block_with_transactions(0, block_info, vec![tx]);
        source.chain_provider.insert_receipts(block_info.hash, vec![receipt]);

        // Post-Espresso: the commitment is authenticated and the tx sender matches the
        // authenticating caller, so the batch is authorized.
        source.load_calldata(&block_info, Address::ZERO).await.unwrap();
        assert_eq!(source.calldata.len(), 1);
    }

    #[tokio::test]
    async fn test_post_espresso_caller_mismatch_rejected() {
        let batch_inbox = address!("0123456789012345678901234567890123456789");
        let auth_addr = address!("00000000000000000000000000000000000000aa");
        let tx = test_legacy_tx(batch_inbox);
        let data = match &tx {
            TxEnvelope::Legacy(t) => t.tx().input.clone(),
            _ => unreachable!(),
        };
        let commitment = keccak256(&data);

        // The commitment is authenticated, but by a different caller than the batch tx sender.
        let mut source = CeloCalldataSource::new(
            TestChainProvider::default(),
            batch_inbox,
            Some(auth_config(auth_addr)),
        );

        let block_info = BlockInfo::default();
        let other_caller = address!("00000000000000000000000000000000000000bb");
        let log = Log {
            address: auth_addr,
            data: LogData::new_unchecked(
                vec![BATCH_INFO_AUTHENTICATED_TOPIC, other_caller.into_word()],
                commitment.as_slice().to_vec().into(),
            ),
        };
        let receipt =
            Receipt { status: Eip658Value::Eip658(true), logs: vec![log], ..Default::default() };
        source.chain_provider.insert_block_with_transactions(0, block_info, vec![tx]);
        source.chain_provider.insert_receipts(block_info.hash, vec![receipt]);

        // Post-Espresso: commitment authenticated but tx sender != authenticating caller: rejected.
        source.load_calldata(&block_info, Address::ZERO).await.unwrap();
        assert!(source.calldata.is_empty());
    }

    #[tokio::test]
    async fn test_post_espresso_no_event_rejected() {
        let batch_inbox = address!("0123456789012345678901234567890123456789");
        let auth_addr = address!("00000000000000000000000000000000000000aa");
        let tx = test_legacy_tx(batch_inbox);

        let mut source = CeloCalldataSource::new(
            TestChainProvider::default(),
            batch_inbox,
            Some(auth_config(auth_addr)),
        );
        let block_info = BlockInfo::default();
        source.chain_provider.insert_block_with_transactions(0, block_info, vec![tx.clone()]);
        source.chain_provider.insert_receipts(block_info.hash, Vec::new());

        // Post-Espresso with no authenticating event: rejected even though sender matches.
        source.load_calldata(&block_info, tx.recover_signer().unwrap()).await.unwrap();
        assert!(source.calldata.is_empty());
    }

    #[tokio::test]
    async fn test_configured_but_not_active_uses_sender_path() {
        // Espresso is configured but the fork activates in the future, so the block being scanned
        // (timestamp 0) is pre-Espresso and must use vanilla sender verification — byte-identical
        // to upstream OP Stack.
        let batch_inbox = address!("0123456789012345678901234567890123456789");
        let auth_addr = address!("00000000000000000000000000000000000000aa");
        let mut source = CeloCalldataSource::new(
            TestChainProvider::default(),
            batch_inbox,
            Some(BatchAuthConfig { authenticator_address: auth_addr, espresso_time: 1_000 }),
        );
        let tx = test_legacy_tx(batch_inbox);
        let block_info = BlockInfo::default(); // timestamp 0 < espresso_time 1000
        source.chain_provider.insert_block_with_transactions(0, block_info, vec![tx.clone()]);

        // Sender matches: accepted via the pre-Espresso path (no event needed).
        source.load_calldata(&block_info, tx.recover_signer().unwrap()).await.unwrap();
        assert_eq!(source.calldata.len(), 1);

        // Sender mismatch: rejected (pre-Espresso path, no event fallback).
        let mut source2 = CeloCalldataSource::new(
            TestChainProvider::default(),
            batch_inbox,
            Some(BatchAuthConfig { authenticator_address: auth_addr, espresso_time: 1_000 }),
        );
        source2.chain_provider.insert_block_with_transactions(0, block_info, vec![tx]);
        source2.load_calldata(&block_info, Address::ZERO).await.unwrap();
        assert!(source2.calldata.is_empty());
    }
}
