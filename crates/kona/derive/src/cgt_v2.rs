//! Receipt-triggered CGTv2 derivation. Providers must authenticate receipts against the origin.

use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use alloy_consensus::{Header, Receipt, TxEnvelope};
use alloy_eips::{BlockNumHash, Encodable2718};
use alloy_primitives::{Address, B256, Bytes, U256};
use async_trait::async_trait;
use celo_alloy_consensus::cgt_v2_group::{CGT_V2_GROUP_GAS, CgtV2UpgradeGroup, cgt_v2_params};
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
            let group = CgtV2UpgradeGroup::deposits(self.origin.cfg.l2_chain_id.id(), amount)
                .map_err(|e| invalid(alloc::format!("{e}")))?;
            attrs
                .transactions
                .get_or_insert_default()
                .extend(group.iter().map(|tx| Bytes::from(tx.encoded_2718())));
            let gas =
                attrs.gas_limit.ok_or_else(|| invalid("CGTv2 attributes missing gas limit"))?;
            attrs.gas_limit = Some(
                gas.checked_add(CGT_V2_GROUP_GAS)
                    .ok_or_else(|| invalid("CGTv2 gas limit overflow"))?,
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
mod tests;
