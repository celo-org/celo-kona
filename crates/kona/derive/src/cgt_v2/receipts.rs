//! A bounded data cache shared by the batch guard and attributes builder.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use alloy_consensus::{Header, Receipt, TxEnvelope};
use alloy_primitives::B256;
use async_trait::async_trait;
use kona_derive::ChainProvider;
use kona_protocol::BlockInfo;
use spin::Mutex;

#[derive(Debug)]
struct CachedReceipts {
    block_hash: B256,
    receipts: Vec<Receipt>,
}

/// Caches only the last origin's successful receipt read, keyed by its exact hash.
/// This contains authenticated L1 data, not migration status. Reorgs use different keys.
#[derive(Debug, Clone)]
pub struct ReceiptCacheProvider<P> {
    pub(super) inner: P,
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
