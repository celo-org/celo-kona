//! Receipt-triggered CGTv2 derivation. Providers must authenticate receipts against the origin.

use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use alloy_consensus::{Header, Receipt, TxEnvelope};
use alloy_eips::{BlockNumHash, Encodable2718};
use alloy_primitives::{Address, B256, Bytes, U256};
use async_trait::async_trait;
use celo_alloy_consensus::cgt_v2_group::{CgtV2UpgradeGroup, cgt_v2_params};
use kona_derive::{
    AttributesBuilder, AttributesProvider, BuilderError, ChainProvider, OriginAdvancer,
    OriginProvider, PipelineError, PipelineErrorKind, PipelineResult, Stage,
};
use kona_genesis::{RollupConfig, SystemConfig};
use kona_protocol::{BlockInfo, L2BlockInfo, SingleBatch};
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use spin::Mutex;

fn invalid(message: impl Into<String>) -> PipelineErrorKind {
    PipelineError::AttributesBuilder(BuilderError::Custom(message.into())).crit()
}

// keccak256("EscrowSeeded(address,uint256)").
const ESCROW_SEEDED: B256 =
    alloy_primitives::b256!("72912711e0dfcb1e457e32744afaae70f87ec629dbcd99f031d2f993c3a875fe");

fn escrow_amount(
    receipts: &[Receipt],
    bridge: Address,
    portal: Address,
) -> Result<Option<U256>, &'static str> {
    for receipt in receipts {
        let Some(success) = receipt.status.as_eip658() else {
            return Err("CGTv2 origin receipt has noncanonical poststate");
        };
        if !success {
            continue;
        }
        for log in &receipt.logs {
            if log.address != bridge || log.topics().first() != Some(&ESCROW_SEEDED) {
                continue;
            }
            if log.topics().len() != 2 || log.data.data.len() != 32 {
                return Err("malformed CGTv2 EscrowSeeded event");
            }
            if log.topics()[1] == portal.into_word() {
                return Ok(Some(U256::from_be_slice(&log.data.data)));
            }
        }
    }
    Ok(None)
}

/// Fixed protocol inputs and an L1 provider. No activation-status cache or bootstrap lookup.
#[derive(Debug, Clone)]
pub struct CgtV2Origin<P> {
    cfg: Arc<RollupConfig>,
    provider: P,
}
impl<P> CgtV2Origin<P> {
    /// The provider must authenticate headers and receipts, as OracleL1ChainProvider does.
    pub const fn new(cfg: Arc<RollupConfig>, provider: P) -> Self {
        Self { cfg, provider }
    }
}
impl<P: ChainProvider + Send> CgtV2Origin<P> {
    /// Returns A only for the first L2 block of the event's origin. Replays read the same receipts.
    pub async fn amount(
        &mut self,
        parent: L2BlockInfo,
        epoch: BlockNumHash,
    ) -> PipelineResult<Option<U256>> {
        if parent.l1_origin == epoch {
            return Ok(None);
        }
        let Ok(params) = cgt_v2_params(self.cfg.l2_chain_id.id()) else {
            return Ok(None);
        };
        let header = self.provider.header_by_hash(epoch.hash).await.map_err(Into::into)?;
        if header.hash_slow() != epoch.hash || header.number != epoch.number {
            return Err(invalid("CGTv2 origin header hash or number mismatch"));
        }
        if parent.l1_origin.number.checked_add(1) != Some(epoch.number) ||
            header.parent_hash != parent.l1_origin.hash
        {
            return Err(invalid("CGTv2 origin is not the next authenticated epoch"));
        }
        // OracleL1ChainProvider walks the authenticated receipt trie before returning these
        // type-erased Receipt values. Re-encoding them would lose typed receipt envelopes.
        let receipts = self.provider.receipts_by_hash(epoch.hash).await.map_err(Into::into)?;
        let portal = self.cfg.deposit_contract_address;
        escrow_amount(&receipts, params.celo_gas_bridge_l1, portal).map_err(invalid)
    }
}

/// Appends the NUT/funding group after all origin deposits and scheduled upgrades.
#[derive(Debug)]
pub struct CgtV2AttributesBuilder<B, P> {
    inner: B,
    origin: CgtV2Origin<P>,
}
impl<B, P> CgtV2AttributesBuilder<B, P> {
    /// Wrap the pinned upstream builder without changing its deposit or hardfork logic.
    pub const fn new(cfg: Arc<RollupConfig>, inner: B, provider: P) -> Self {
        Self { inner, origin: CgtV2Origin::new(cfg, provider) }
    }
}
#[async_trait]
impl<B: AttributesBuilder, P: ChainProvider + Send + core::fmt::Debug> AttributesBuilder
    for CgtV2AttributesBuilder<B, P>
{
    async fn prepare_payload_attributes(
        &mut self,
        parent: L2BlockInfo,
        epoch: BlockNumHash,
    ) -> PipelineResult<OpPayloadAttributes> {
        let amount = self.origin.amount(parent, epoch).await?;
        let mut attrs = self.inner.prepare_payload_attributes(parent, epoch).await?;
        if let Some(amount) = amount {
            let chain_id = self.origin.cfg.l2_chain_id.id();
            let group = CgtV2UpgradeGroup::deposits(chain_id, amount)
                .map_err(|e| invalid(alloc::format!("{e}")))?;
            attrs
                .transactions
                .get_or_insert_default()
                .extend(group.iter().map(|tx| Bytes::from(tx.encoded_2718())));
            let gas =
                attrs.gas_limit.ok_or_else(|| invalid("CGTv2 attributes missing gas limit"))?;
            let upgrade_gas = CgtV2UpgradeGroup::gas_limit(chain_id)
                .map_err(|e| invalid(alloc::format!("{e}")))?;
            attrs.gas_limit = Some(
                gas.checked_add(upgrade_gas).ok_or_else(|| invalid("CGTv2 gas limit overflow"))?,
            );
            attrs.no_tx_pool = Some(true);
        }
        Ok(attrs)
    }
}

/// Prevents batch transactions from entering the activation block.
#[derive(Debug)]
pub struct CgtV2BatchProvider<B, P> {
    /// The standard provider, including its span/channel buffers.
    pub inner: B,
    origin: CgtV2Origin<P>,
    pending: Option<SingleBatch>,
    last_in_span: bool,
}
impl<B, P> CgtV2BatchProvider<B, P> {
    /// Wrap the shared post-decomposition batch stage.
    pub const fn new(cfg: Arc<RollupConfig>, inner: B, provider: P) -> Self {
        Self { inner, origin: CgtV2Origin::new(cfg, provider), pending: None, last_in_span: false }
    }
}
#[async_trait]
impl<B: AttributesProvider + Stage + Send, P: ChainProvider + Send> AttributesProvider
    for CgtV2BatchProvider<B, P>
{
    async fn next_batch(&mut self, parent: L2BlockInfo) -> PipelineResult<SingleBatch> {
        if self.pending.is_none() {
            self.pending = Some(self.inner.next_batch(parent).await?);
            self.last_in_span = self.inner.is_last_in_span();
        }
        let batch = self.pending.as_ref().expect("batch just loaded");
        if !batch.transactions.is_empty() &&
            self.origin.amount(parent, batch.epoch()).await?.is_some()
        {
            // Drop the invalid block and the rest of its channel/span. Already derived
            // prefix blocks remain accepted, matching Holocene's sequential validation.
            self.inner.flush_channel().await?;
            self.pending = None;
            self.last_in_span = false;
            return Err(PipelineError::NotEnoughData.temp());
        }
        Ok(self.pending.take().expect("pending batch retained across provider errors"))
    }
    fn is_last_in_span(&self) -> bool {
        self.last_in_span
    }
}
impl<B: OriginProvider, P> OriginProvider for CgtV2BatchProvider<B, P> {
    fn origin(&self) -> Option<BlockInfo> {
        self.inner.origin()
    }
}
#[async_trait]
impl<B: OriginAdvancer + Send, P: Send> OriginAdvancer for CgtV2BatchProvider<B, P> {
    async fn advance_origin(&mut self) -> PipelineResult<()> {
        self.inner.advance_origin().await
    }
}
#[async_trait]
impl<B: Stage + Send, P: Send> Stage for CgtV2BatchProvider<B, P> {
    async fn reset(&mut self, origin: BlockNumHash, config: SystemConfig) -> PipelineResult<()> {
        self.pending = None;
        self.last_in_span = false;
        self.inner.reset(origin, config).await
    }
    async fn activate(&mut self) -> PipelineResult<()> {
        self.pending = None;
        self.last_in_span = false;
        self.inner.activate().await
    }
    async fn flush_channel(&mut self) -> PipelineResult<()> {
        self.pending = None;
        self.last_in_span = false;
        self.inner.flush_channel().await
    }
}

#[derive(Debug)]
struct CachedReceipts {
    block_hash: B256,
    receipts: Vec<Receipt>,
}

/// Caches only the last origin's successful receipt read, keyed by its exact hash.
/// This contains authenticated L1 data, not migration status. Reorgs use different keys.
#[derive(Debug, Clone)]
pub struct ReceiptCacheProvider<P> {
    inner: P,
    receipts: Arc<Mutex<Option<CachedReceipts>>>,
}
impl<P> ReceiptCacheProvider<P> {
    /// Creates a cache; clones share the single bounded entry.
    pub fn new(inner: P) -> Self {
        Self { inner, receipts: Arc::new(Mutex::new(None)) }
    }
}
#[async_trait]
impl<P: ChainProvider + Send> ChainProvider for ReceiptCacheProvider<P> {
    type Error = P::Error;
    async fn header_by_hash(&mut self, hash: B256) -> Result<Header, Self::Error> {
        self.inner.header_by_hash(hash).await
    }
    async fn block_info_by_number(&mut self, number: u64) -> Result<BlockInfo, Self::Error> {
        self.inner.block_info_by_number(number).await
    }
    async fn receipts_by_hash(&mut self, hash: B256) -> Result<Vec<Receipt>, Self::Error> {
        if let Some(cached) = &*self.receipts.lock() &&
            cached.block_hash == hash
        {
            return Ok(cached.receipts.clone());
        }
        let receipts = self.inner.receipts_by_hash(hash).await?;
        *self.receipts.lock() =
            Some(CachedReceipts { block_hash: hash, receipts: receipts.clone() });
        Ok(receipts)
    }
    async fn block_info_and_transactions_by_hash(
        &mut self,
        hash: B256,
    ) -> Result<(BlockInfo, Vec<TxEnvelope>), Self::Error> {
        self.inner.block_info_and_transactions_by_hash(hash).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{vec, vec::Vec};
    use alloy_consensus::{Eip658Value, Receipt};
    use alloy_primitives::{Log, LogData};
    use kona_derive::test_utils::TestChainProvider;

    fn receipt(bridge: Address, portal: Address, amount: U256) -> Receipt {
        Receipt {
            status: Eip658Value::Eip658(true),
            logs: vec![Log {
                address: bridge,
                data: LogData::new_unchecked(
                    vec![ESCROW_SEEDED, portal.into_word()],
                    amount.to_be_bytes::<32>().to_vec().into(),
                ),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn cgt_v2_escrow_filters_and_amount() {
        let bridge = Address::with_last_byte(0xcc);
        let portal = Address::with_last_byte(0x44);
        let valid = receipt(bridge, portal, U256::from(600));
        assert_eq!(
            escrow_amount(core::slice::from_ref(&valid), bridge, portal).unwrap(),
            Some(U256::from(600))
        );
        assert_eq!(
            escrow_amount(&[receipt(bridge, portal, U256::ZERO)], bridge, portal).unwrap(),
            Some(U256::ZERO)
        );
        for mut wrong in [
            receipt(Address::ZERO, portal, U256::from(600)),
            receipt(bridge, Address::ZERO, U256::from(600)),
        ] {
            assert_eq!(escrow_amount(core::slice::from_ref(&wrong), bridge, portal).unwrap(), None);
            wrong.status = Eip658Value::Eip658(false);
            assert_eq!(escrow_amount(&[wrong], bridge, portal).unwrap(), None);
        }
        let mut malformed = valid.clone();
        malformed.logs[0].data.data = Bytes::new();
        assert!(escrow_amount(&[malformed], bridge, portal).is_err());
        let mut poststate = valid;
        poststate.status = Eip658Value::PostState(B256::ZERO);
        assert!(escrow_amount(&[poststate], bridge, portal).is_err());
    }

    #[tokio::test]
    async fn cgt_v2_new_origin_only_and_replay() {
        let portal = Address::with_last_byte(0x44);
        let parent_hash = B256::with_last_byte(1);
        let header = alloy_consensus::Header { parent_hash, number: 11, ..Default::default() };
        let epoch = BlockNumHash::new(11, header.hash_slow());
        let provider = TestChainProvider {
            headers: vec![(epoch.hash, header)],
            receipts: vec![(
                epoch.hash,
                vec![receipt(Address::with_last_byte(0xcc), portal, U256::from(600))],
            )],
            ..Default::default()
        };
        let cfg = Arc::new(RollupConfig {
            l2_chain_id: 1337.into(),
            deposit_contract_address: portal,
            ..Default::default()
        });
        let mut detector = CgtV2Origin::new(cfg.clone(), provider.clone());
        let mut parent =
            L2BlockInfo { l1_origin: BlockNumHash::new(10, parent_hash), ..Default::default() };
        assert_eq!(detector.amount(parent, epoch).await.unwrap(), Some(U256::from(600)));
        parent.l1_origin = epoch;
        assert_eq!(detector.amount(parent, epoch).await.unwrap(), None);
        parent.l1_origin = BlockNumHash::new(10, parent_hash);
        let mut restarted = CgtV2Origin::new(cfg, provider);
        assert_eq!(restarted.amount(parent, epoch).await.unwrap(), Some(U256::from(600)));
        assert!(detector.amount(parent, BlockNumHash::new(12, epoch.hash)).await.is_err());
    }

    fn setup() -> (Arc<RollupConfig>, TestChainProvider, L2BlockInfo, BlockNumHash) {
        let portal = Address::with_last_byte(0x44);
        let parent_hash = B256::with_last_byte(1);
        let header = alloy_consensus::Header { parent_hash, number: 11, ..Default::default() };
        let epoch = BlockNumHash::new(11, header.hash_slow());
        let provider = TestChainProvider {
            headers: vec![(epoch.hash, header)],
            receipts: vec![(
                epoch.hash,
                vec![receipt(Address::with_last_byte(0xcc), portal, U256::from(600))],
            )],
            ..Default::default()
        };
        let cfg = Arc::new(RollupConfig {
            l2_chain_id: 1337.into(),
            deposit_contract_address: portal,
            ..Default::default()
        });
        let parent =
            L2BlockInfo { l1_origin: BlockNumHash::new(10, parent_hash), ..Default::default() };
        (cfg, provider, parent, epoch)
    }

    #[tokio::test]
    async fn cgt_v2_attributes_append_exact_group_and_restore_next_epoch() {
        use kona_derive::test_utils::TestAttributesBuilder;
        let (cfg, provider, parent, epoch) = setup();
        // Represents L1-info, deposits before and after the L1 event, and an upstream upgrade.
        let earlier = vec![
            Bytes::from_static(b"info"),
            Bytes::from_static(b"before"),
            Bytes::from_static(b"after"),
            Bytes::from_static(b"scheduled"),
        ];
        let normal = OpPayloadAttributes {
            transactions: Some(earlier.clone()),
            gas_limit: Some(30_000_000),
            ..Default::default()
        };
        let inner = TestAttributesBuilder { attributes: vec![Ok(normal.clone()), Ok(normal)] };
        let mut builder = CgtV2AttributesBuilder::new(cfg, inner, provider);
        let active = builder.prepare_payload_attributes(parent, epoch).await.unwrap();
        let txs = active.transactions.unwrap();
        assert_eq!(&txs[..4], earlier.as_slice());
        let group = CgtV2UpgradeGroup::deposits(1337, U256::from(600)).unwrap();
        assert_eq!(
            &txs[4..],
            group.iter().map(|tx| Bytes::from(tx.encoded_2718())).collect::<Vec<_>>()
        );
        assert_eq!(active.gas_limit, Some(70_600_000));
        assert_eq!(active.no_tx_pool, Some(true));
        let next = builder
            .prepare_payload_attributes(L2BlockInfo { l1_origin: epoch, ..parent }, epoch)
            .await
            .unwrap();
        assert_eq!(next.gas_limit, Some(30_000_000));
        assert_eq!(next.transactions.unwrap(), earlier);
    }

    #[tokio::test]
    async fn cgt_v2_attributes_invalid_amount_and_gas_overflow() {
        use kona_derive::test_utils::TestAttributesBuilder;
        for invalid_amount in [false, true] {
            let (cfg, mut provider, parent, epoch) = setup();
            if invalid_amount {
                provider.receipts[0].1[0].logs[0].data.data =
                    U256::from(1001).to_be_bytes::<32>().to_vec().into();
            }
            let inner = TestAttributesBuilder {
                attributes: vec![Ok(OpPayloadAttributes {
                    gas_limit: Some(u64::MAX),
                    ..Default::default()
                })],
            };
            let mut builder = CgtV2AttributesBuilder::new(cfg, inner, provider);
            assert!(builder.prepare_payload_attributes(parent, epoch).await.is_err());
        }
    }

    #[tokio::test]
    async fn cgt_v2_batch_guard_retry_flush_and_reset() {
        use kona_derive::{AttributesProvider, Stage, test_utils::new_test_attributes_provider};
        use kona_protocol::SingleBatch;
        let (cfg, provider, parent, epoch) = setup();
        let batch = SingleBatch {
            epoch_num: epoch.number,
            epoch_hash: epoch.hash,
            transactions: vec![Bytes::from_static(&[0x02])],
            ..Default::default()
        };
        let inner = new_test_attributes_provider(None, vec![Ok(batch.clone())]);
        let mut guard = CgtV2BatchProvider::new(cfg.clone(), inner, provider.clone());
        // A temporary L1 fetch failure must retain the batch for retry.
        guard.origin.provider.headers.clear();
        assert!(guard.next_batch(parent).await.is_err());
        assert!(guard.pending.is_some());
        guard.origin.provider = provider.clone();
        assert_eq!(
            guard.next_batch(parent).await.unwrap_err(),
            PipelineError::NotEnoughData.temp()
        );
        assert!(guard.inner.flushed);
        assert!(guard.pending.is_none());
        guard.reset(parent.l1_origin, Default::default()).await.unwrap();
        assert!(guard.inner.reset);
        // The same user tx is accepted in a subsequent block with the same origin.
        let mut next = CgtV2BatchProvider::new(
            cfg,
            new_test_attributes_provider(None, vec![Ok(batch.clone())]),
            provider,
        );
        assert_eq!(
            next.next_batch(L2BlockInfo { l1_origin: epoch, ..parent }).await.unwrap(),
            batch
        );
    }

    #[tokio::test]
    async fn cgt_v2_batch_rejects_post_exec() {
        use kona_derive::{AttributesProvider, test_utils::new_test_attributes_provider};
        let (cfg, provider, parent, epoch) = setup();
        let mut cfg = (*cfg).clone();
        // The CGTv2 batch rule is unconditional, even if a fixture enables Lagoon.
        cfg.hardforks.lagoon_time = Some(0);
        let batch = SingleBatch {
            epoch_num: epoch.number,
            epoch_hash: epoch.hash,
            // PostExec(version=1, block=1, refunds=[(0, 1)]).
            transactions: vec![Bytes::from_static(&[
                0x7f, 0xc6, 0x01, 0x01, 0xc3, 0xc2, 0x80, 0x01,
            ])],
            ..Default::default()
        };
        let inner = new_test_attributes_provider(None, vec![Ok(batch)]);
        let mut guard = CgtV2BatchProvider::new(Arc::new(cfg), inner, provider);
        assert_eq!(
            guard.next_batch(parent).await.unwrap_err(),
            PipelineError::NotEnoughData.temp()
        );
        assert!(guard.inner.flushed);
        assert!(guard.pending.is_none());
    }

    #[tokio::test]
    async fn cgt_v2_receipt_cache_is_hash_keyed_and_shared() {
        use kona_derive::ChainProvider;
        let (_, provider, _, epoch) = setup();
        let mut cache = ReceiptCacheProvider::new(provider);
        let receipts = cache.receipts_by_hash(epoch.hash).await.unwrap();
        let mut clone = cache.clone();
        // Prove the cloned provider can use the cached authenticated result, while another hash
        // fails.
        clone.inner.receipts.clear();
        assert_eq!(clone.receipts_by_hash(epoch.hash).await.unwrap(), receipts);
        assert!(clone.receipts_by_hash(B256::ZERO).await.is_err());
        assert_eq!(cache.receipts_by_hash(epoch.hash).await.unwrap(), receipts);
    }

    #[tokio::test]
    async fn cgt_v2_real_span_buffer_keeps_prefix_and_discards_tail() {
        use kona_derive::{
            AttributesProvider, BatchProvider, BatchStream, BatchValidator,
            test_utils::{TestBatchStreamProvider, TestL2ChainProvider},
        };
        use kona_protocol::{BlockInfo, SingleBatch};
        let (cfg, provider, mut parent, epoch) = setup();
        let mut cfg = (*cfg).clone();
        cfg.block_time = 2;
        cfg.seq_window_size = 100;
        cfg.hardforks.holocene_time = Some(0);
        let cfg = Arc::new(cfg);
        parent.block_info.timestamp = 100;
        let old = BlockInfo {
            hash: parent.l1_origin.hash,
            number: 10,
            timestamp: 100,
            ..Default::default()
        };
        let activation =
            BlockInfo { hash: epoch.hash, number: 11, timestamp: 102, ..Default::default() };
        let inclusion = BlockInfo {
            hash: B256::with_last_byte(12),
            number: 12,
            timestamp: 104,
            ..Default::default()
        };
        let reader = TestBatchStreamProvider { origin: Some(inclusion), ..Default::default() };
        let mut stream = BatchStream::new(reader, cfg.clone(), TestL2ChainProvider::default());
        // Hydrated output of a span: a valid prefix, the invalid activation block, and an unread
        // tail.
        stream.buffer.extend([
            SingleBatch {
                epoch_num: 10,
                epoch_hash: old.hash,
                timestamp: 102,
                ..Default::default()
            },
            SingleBatch {
                epoch_num: 11,
                epoch_hash: epoch.hash,
                timestamp: 104,
                transactions: vec![Bytes::from_static(&[0x02])],
                ..Default::default()
            },
            SingleBatch {
                epoch_num: 11,
                epoch_hash: epoch.hash,
                timestamp: 106,
                ..Default::default()
            },
        ]);
        let mut validator = BatchValidator::new(cfg.clone(), stream);
        validator.origin = Some(inclusion);
        validator.l1_blocks = vec![old, activation, inclusion];
        let mut mux =
            BatchProvider::new(cfg.clone(), validator.prev, TestL2ChainProvider::default());
        let stream = mux.prev.take().unwrap();
        let mut validator = BatchValidator::new(cfg.clone(), stream);
        validator.origin = Some(inclusion);
        validator.l1_blocks = vec![old, activation, inclusion];
        mux.batch_validator = Some(validator);
        let mut guard = CgtV2BatchProvider::new(cfg, mux, provider);
        let prefix = guard.next_batch(parent).await.unwrap();
        assert_eq!(prefix.timestamp, 102);
        parent.block_info.timestamp = prefix.timestamp;
        assert_eq!(
            guard.next_batch(parent).await.unwrap_err(),
            PipelineError::NotEnoughData.temp()
        );
        let real_stream = &guard.inner.batch_validator.as_ref().unwrap().prev;
        assert!(real_stream.buffer.is_empty());
        assert!(real_stream.span.is_none());
        assert!(real_stream.prev.flushed);
        assert_eq!(parent.block_info.timestamp, 102, "accepted prefix is retained");
    }

    #[tokio::test]
    async fn cgt_v2_production_origin_requires_the_configured_emitter() {
        for id in [42220, 11142220, 11162320] {
            let (mut cfg, mut provider, parent, epoch) = setup();
            Arc::make_mut(&mut cfg).l2_chain_id = id.into();
            let mut detector = CgtV2Origin::new(cfg.clone(), provider.clone());
            // A foreign emitter with identical topics cannot trigger activation or stop derivation.
            assert_eq!(detector.amount(parent, epoch).await.unwrap(), None);
            provider.receipts[0].1[0].logs[0].address =
                cgt_v2_params(id).unwrap().celo_gas_bridge_l1;
            let mut detector = CgtV2Origin::new(cfg, provider);
            assert_eq!(detector.amount(parent, epoch).await.unwrap(), Some(U256::from(600)));
        }
    }

    #[tokio::test]
    async fn cgt_v2_deprecated_chains_skip_origin_queries() {
        for id in [44787, 62320] {
            let (mut cfg, _, parent, epoch) = setup();
            Arc::make_mut(&mut cfg).l2_chain_id = id.into();
            let mut detector = CgtV2Origin::new(cfg, TestChainProvider::default());
            assert_eq!(detector.amount(parent, epoch).await.unwrap(), None);
        }
    }
}
