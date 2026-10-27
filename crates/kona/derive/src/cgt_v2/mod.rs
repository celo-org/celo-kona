//! Receipt-triggered CGTv2 derivation. Providers must authenticate receipts against the origin.

use alloc::{boxed::Box, string::String, sync::Arc};
use alloy_consensus::Receipt;
use alloy_eips::{BlockNumHash, Encodable2718};
use alloy_primitives::{Address, B256, Bytes, U256};
use async_trait::async_trait;
use celo_alloy_consensus::{
    cgt_v2_group::{CGT_V2_GROUP_GAS, CgtV2UpgradeGroup},
    cgt_v2_params::cgt_v2_params,
};
use kona_derive::{
    AttributesBuilder, BuilderError, ChainProvider, PipelineError, PipelineErrorKind,
    PipelineResult,
};
use kona_genesis::RollupConfig;
use kona_protocol::L2BlockInfo;
use op_alloy_rpc_types_engine::OpPayloadAttributes;

mod batches;
pub use batches::CgtV2BatchProvider;
mod receipts;
pub use receipts::ReceiptCacheProvider;

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

#[cfg(test)]
mod tests;
