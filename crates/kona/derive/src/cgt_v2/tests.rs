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
    assert_eq!(guard.next_batch(parent).await.unwrap_err(), PipelineError::NotEnoughData.temp());
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
    assert_eq!(next.next_batch(L2BlockInfo { l1_origin: epoch, ..parent }).await.unwrap(), batch);
}

#[tokio::test]
async fn cgt_v2_receipt_cache_is_hash_keyed_and_shared() {
    use kona_derive::ChainProvider;
    let (_, provider, _, epoch) = setup();
    let mut cache = ReceiptCacheProvider::new(provider);
    let receipts = cache.receipts_by_hash(epoch.hash).await.unwrap();
    let mut clone = cache.clone();
    // Prove the cloned provider can use the cached authenticated result, while another hash fails.
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
    let old =
        BlockInfo { hash: parent.l1_origin.hash, number: 10, timestamp: 100, ..Default::default() };
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
    // Hydrated output of a span: a valid prefix, the invalid activation block, and an unread tail.
    stream.buffer.extend([
        SingleBatch { epoch_num: 10, epoch_hash: old.hash, timestamp: 102, ..Default::default() },
        SingleBatch {
            epoch_num: 11,
            epoch_hash: epoch.hash,
            timestamp: 104,
            transactions: vec![Bytes::from_static(&[0x02])],
            ..Default::default()
        },
        SingleBatch { epoch_num: 11, epoch_hash: epoch.hash, timestamp: 106, ..Default::default() },
    ]);
    let mut validator = BatchValidator::new(cfg.clone(), stream);
    validator.origin = Some(inclusion);
    validator.l1_blocks = vec![old, activation, inclusion];
    let mut mux = BatchProvider::new(cfg.clone(), validator.prev, TestL2ChainProvider::default());
    let stream = mux.prev.take().unwrap();
    let mut validator = BatchValidator::new(cfg.clone(), stream);
    validator.origin = Some(inclusion);
    validator.l1_blocks = vec![old, activation, inclusion];
    mux.batch_validator = Some(validator);
    let mut guard = CgtV2BatchProvider::new(cfg, mux, provider);
    let prefix = guard.next_batch(parent).await.unwrap();
    assert_eq!(prefix.timestamp, 102);
    parent.block_info.timestamp = prefix.timestamp;
    assert_eq!(guard.next_batch(parent).await.unwrap_err(), PipelineError::NotEnoughData.temp());
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
        provider.receipts[0].1[0].logs[0].address = cgt_v2_params(id).unwrap().celo_gas_bridge_l1;
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
