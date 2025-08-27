//! The [`AttributesBuilder`] and it's default implementation.

use alloc::{boxed::Box, fmt::Debug, string::ToString, sync::Arc, vec, vec::Vec};
use alloy_consensus::{Eip658Value, Receipt};
use alloy_eips::{BlockNumHash, eip2718::Encodable2718};
use alloy_primitives::{Address, B256, Bytes};
use alloy_rlp::Encodable;
use alloy_rpc_types_engine::PayloadAttributes;
use async_trait::async_trait;
use celo_protocol::CeloL2ChainProvider;
use kona_derive::{
    errors::{BuilderError, PipelineEncodingError, PipelineError, PipelineErrorKind},
    traits::{AttributesBuilder, ChainProvider},
    types::PipelineResult,
};
use kona_genesis::RollupConfig;
use kona_hardforks::{Hardfork, Hardforks};
use kona_protocol::{
    DEPOSIT_EVENT_ABI_HASH, L1BlockInfoTx, L2BlockInfo, Predeploys, decode_deposit,
};
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use tracing::info;

/// A stateful implementation of the [AttributesBuilder].
#[derive(Debug)]
pub struct CeloStatefulAttributesBuilder<L1P, L2P>
where
    L1P: ChainProvider + Debug,
    L2P: CeloL2ChainProvider + Debug,
{
    /// The rollup config.
    rollup_cfg: Arc<RollupConfig>,
    /// The system config fetcher.
    config_fetcher: L2P,
    /// The L1 receipts fetcher.
    receipts_fetcher: L1P,
}

impl<L1P, L2P> CeloStatefulAttributesBuilder<L1P, L2P>
where
    L1P: ChainProvider + Debug,
    L2P: CeloL2ChainProvider + Debug,
{
    /// Create a new [StatefulAttributesBuilder] with the given epoch.
    pub const fn new(rcfg: Arc<RollupConfig>, sys_cfg_fetcher: L2P, receipts: L1P) -> Self {
        Self { rollup_cfg: rcfg, config_fetcher: sys_cfg_fetcher, receipts_fetcher: receipts }
    }
}

#[async_trait]
impl<L1P, L2P> AttributesBuilder for CeloStatefulAttributesBuilder<L1P, L2P>
where
    L1P: ChainProvider + Debug + Send,
    L2P: CeloL2ChainProvider + Debug + Send,
{
    async fn prepare_payload_attributes(
        &mut self,
        l2_parent: L2BlockInfo,
        epoch: BlockNumHash,
    ) -> PipelineResult<OpPayloadAttributes> {
        info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 1");
        let l1_header;
        let deposit_transactions: Vec<Bytes>;

        info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 2");
        // ERROR below
        let mut sys_config = self
            .config_fetcher
            .system_config_by_number(l2_parent.block_info.number, self.rollup_cfg.clone())
            .await
            .inspect_err(|e| info!("!!!! system_config_by_number return error: {}", e))
            .map_err(Into::into)?;

        info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 3");
        // If the L1 origin changed in this block, then we are in the first block of the epoch.
        // In this case we need to fetch all transaction receipts from the L1 origin block so
        // we can scan for user deposits.
        let sequence_number = if l2_parent.l1_origin.number != epoch.number {
            info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 4-1");
            let header =
                self.receipts_fetcher.header_by_hash(epoch.hash).await.map_err(Into::into)?;
            info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 4-2");
            if l2_parent.l1_origin.hash != header.parent_hash {
                return Err(PipelineErrorKind::Reset(
                    BuilderError::BlockMismatchEpochReset(
                        epoch,
                        l2_parent.l1_origin,
                        header.parent_hash,
                    )
                    .into(),
                ));
            }
            info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 4-3");
            let receipts =
                self.receipts_fetcher.receipts_by_hash(epoch.hash).await.map_err(Into::into)?;
            info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 4-4");
            let deposits =
                derive_deposits(epoch.hash, &receipts, self.rollup_cfg.deposit_contract_address)
                    .await
                    .map_err(|e| PipelineError::BadEncoding(e).crit())?;
            info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 4-5");
            sys_config
                .update_with_receipts(
                    &receipts,
                    self.rollup_cfg.l1_system_config_address,
                    self.rollup_cfg.is_ecotone_active(header.timestamp),
                )
                .map_err(|e| PipelineError::SystemConfigUpdate(e).crit())?;
            l1_header = header;
            deposit_transactions = deposits;
            info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 4-6");
            0
        } else {
            info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 5-1");
            #[allow(clippy::collapsible_else_if)]
            if l2_parent.l1_origin.hash != epoch.hash {
                return Err(PipelineErrorKind::Reset(
                    BuilderError::BlockMismatch(epoch, l2_parent.l1_origin).into(),
                ));
            }
            info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 5-2");
            let header =
                self.receipts_fetcher.header_by_hash(epoch.hash).await.map_err(Into::into)?;
            info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 5-3");
            l1_header = header;
            deposit_transactions = vec![];
            l2_parent.seq_num + 1
        };

        info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 6");
        // Sanity check the L1 origin was correctly selected to maintain the time invariant
        // between L1 and L2.
        let next_l2_time = l2_parent.block_info.timestamp + self.rollup_cfg.block_time;
        info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 7");
        if next_l2_time < l1_header.timestamp {
            return Err(PipelineErrorKind::Reset(
                BuilderError::BrokenTimeInvariant(
                    l2_parent.l1_origin,
                    next_l2_time,
                    BlockNumHash { hash: l1_header.hash_slow(), number: l1_header.number },
                    l1_header.timestamp,
                )
                .into(),
            ));
        }
        info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 8");

        let mut upgrade_transactions: Vec<Bytes> = vec![];
        if self.rollup_cfg.is_ecotone_active(next_l2_time) &&
            !self.rollup_cfg.is_ecotone_active(l2_parent.block_info.timestamp)
        {
            upgrade_transactions = Hardforks::ECOTONE.txs().collect();
        }
        info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 9");
        if self.rollup_cfg.is_fjord_active(next_l2_time) &&
            !self.rollup_cfg.is_fjord_active(l2_parent.block_info.timestamp)
        {
            upgrade_transactions.append(&mut Hardforks::FJORD.txs().collect());
        }
        info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 10");
        if self.rollup_cfg.is_isthmus_active(next_l2_time) &&
            !self.rollup_cfg.is_isthmus_active(l2_parent.block_info.timestamp)
        {
            upgrade_transactions.append(&mut Hardforks::ISTHMUS.txs().collect());
        }
        info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 11");
        if self.rollup_cfg.is_interop_active(next_l2_time) &&
            !self.rollup_cfg.is_interop_active(l2_parent.block_info.timestamp)
        {
            upgrade_transactions.append(&mut Hardforks::INTEROP.txs().collect());
        }

        info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 12");
        // Build and encode the L1 info transaction for the current payload.
        let (_, l1_info_tx_envelope) = L1BlockInfoTx::try_new_with_deposit_tx(
            &self.rollup_cfg,
            &sys_config,
            sequence_number,
            &l1_header,
            next_l2_time,
        )
        .map_err(|e| {
            PipelineError::AttributesBuilder(BuilderError::Custom(e.to_string())).crit()
        })?;
        info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 13");
        let mut encoded_l1_info_tx = Vec::with_capacity(l1_info_tx_envelope.length());
        info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 14");
        l1_info_tx_envelope.encode_2718(&mut encoded_l1_info_tx);

        let mut txs =
            Vec::with_capacity(1 + deposit_transactions.len() + upgrade_transactions.len());
        txs.push(encoded_l1_info_tx.into());
        txs.extend(deposit_transactions);
        txs.extend(upgrade_transactions);

        info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 15");

        let mut withdrawals = None;
        if self.rollup_cfg.is_canyon_active(next_l2_time) {
            withdrawals = Some(Vec::default());
        }

        info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 16");
        let mut parent_beacon_root = None;
        if self.rollup_cfg.is_ecotone_active(next_l2_time) {
            // if the parent beacon root is not available, default to zero hash
            parent_beacon_root = Some(l1_header.parent_beacon_block_root.unwrap_or_default());
        }
        info!("CeloStatefulAttributesBuilder::prepare_payload_attributes 17");
        Ok(OpPayloadAttributes {
            payload_attributes: PayloadAttributes {
                timestamp: next_l2_time,
                prev_randao: l1_header.mix_hash,
                suggested_fee_recipient: Predeploys::SEQUENCER_FEE_VAULT,
                parent_beacon_block_root: parent_beacon_root,
                withdrawals,
            },
            transactions: Some(txs),
            no_tx_pool: Some(true),
            gas_limit: Some(u64::from_be_bytes(
                alloy_primitives::U64::from(sys_config.gas_limit).to_be_bytes(),
            )),
            eip_1559_params: sys_config.eip_1559_params(
                &self.rollup_cfg,
                l2_parent.block_info.timestamp,
                next_l2_time,
            ),
        })
    }
}

/// Derive deposits as `Vec<Bytes>` for transaction receipts.
///
/// Successful deposits must be emitted by the deposit contract and have the correct event
/// signature. So the receipt address must equal the specified deposit contract and the first topic
/// must be the [DEPOSIT_EVENT_ABI_HASH].
async fn derive_deposits(
    block_hash: B256,
    receipts: &[Receipt],
    deposit_contract: Address,
) -> Result<Vec<Bytes>, PipelineEncodingError> {
    let mut global_index = 0;
    let mut res = Vec::new();
    for r in receipts.iter() {
        if Eip658Value::Eip658(false) == r.status {
            continue;
        }
        for l in r.logs.iter() {
            let curr_index = global_index;
            global_index += 1;
            if l.data.topics().first().is_none_or(|i| *i != DEPOSIT_EVENT_ABI_HASH) {
                continue;
            }
            if l.address != deposit_contract {
                continue;
            }
            let decoded = decode_deposit(block_hash, curr_index, l)?;
            res.push(decoded);
        }
    }
    Ok(res)
}
