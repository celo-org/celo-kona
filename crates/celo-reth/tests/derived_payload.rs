//! Derived (`noTxPool`) attributes are built when `engine_forkchoiceUpdated` hands them to the
//! payload service. Transactions that make the payload invalid are rejected there with `-38003`,
//! so the rollup node can replace the block with a deposits-only one, as the fault proof program
//! does.
//!
//! Adapted from op-reth's `tests/it/derived_attributes.rs` for [`CeloNode`].

use alloy_consensus::{Sealable, SignableTransaction, TxEip1559};
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind};
use alloy_rpc_types_engine::{ForkchoiceState, ForkchoiceUpdated, PayloadAttributes};
use celo_reth::node::{CeloNode, RollupArgs};
use jsonrpsee::{
    core::{ClientError, client::ClientT},
    rpc_params,
};
use op_alloy_consensus::{OpTxEnvelope, TxDeposit};
use op_alloy_rpc_types_engine::{OpExecutionPayloadEnvelopeV3, OpPayloadAttributes};
use reth_chainspec::EthChainSpec;
use reth_node_builder::{NodeBuilder, NodeConfig};
use reth_optimism_chainspec::OpChainSpecBuilder;
use reth_tasks::Runtime;
use std::sync::Arc;

const INVALID_PAYLOAD_ATTRIBUTES: i32 = -38003;

#[tokio::test(flavor = "multi_thread")]
async fn invalid_derived_transactions_are_rejected_at_fcu() -> eyre::Result<()> {
    let chain_spec = Arc::new(OpChainSpecBuilder::optimism_sepolia().ecotone_activated().build());
    let chain_id = chain_spec.chain_id();

    // A deposit funding the sender of a 21k-gas transfer.
    let transfer = |nonce| {
        TxEip1559 {
            chain_id,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: 1_000_000_000_000,
            to: TxKind::Call(Address::with_last_byte(1)),
            ..Default::default()
        }
        .into_signed(Signature::test_signature())
    };
    let from = transfer(1).recover_signer()?;
    let encode = |tx: OpTxEnvelope| -> Bytes { tx.encoded_2718().into() };
    let deposit = encode(OpTxEnvelope::Deposit(
        TxDeposit {
            source_hash: B256::with_last_byte(1),
            from,
            to: TxKind::Call(Address::with_last_byte(1)),
            mint: 1_000_000_000_000_000_000,
            gas_limit: 21_000,
            ..Default::default()
        }
        .seal_slow(),
    ));
    // Valid after `deposit`, which raises the sender's nonce to 1.
    let valid_transfer = encode(OpTxEnvelope::Eip1559(transfer(1)));
    let nonce_too_high = encode(OpTxEnvelope::Eip1559(transfer(5)));

    let mut config = NodeConfig::test().map_chain(chain_spec.clone()).with_unused_ports();
    config.network.discovery.discv5_port = Some(0);
    config.network.discovery.discv5_port_ipv6 = Some(0);
    let node = NodeBuilder::new(config)
        .testing_node(Runtime::test())
        .node(CeloNode::new(RollupArgs::default()))
        .launch()
        .await?
        .node;
    let client = node.auth_server_handle().http_client();

    let genesis_head = ForkchoiceState::same_hash(chain_spec.genesis_hash());
    let derived = |transactions: Vec<Bytes>| OpPayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp: chain_spec.genesis_header().timestamp + 2,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
            slot_number: None,
            target_gas_limit: None,
        },
        transactions: Some(transactions),
        no_tx_pool: Some(true),
        gas_limit: Some(30_000_000),
        eip_1559_params: None,
        min_base_fee: None,
    };
    let fcu = |attrs: OpPayloadAttributes| {
        let client = &client;
        async move {
            client
                .request::<ForkchoiceUpdated, _>(
                    "engine_forkchoiceUpdatedV3",
                    rpc_params![genesis_head, attrs],
                )
                .await
        }
    };

    for (case, transactions) in [
        ("nonce too high", vec![deposit.clone(), nonce_too_high]),
        ("sender cannot pay", vec![valid_transfer.clone()]),
    ] {
        match fcu(derived(transactions)).await {
            Err(ClientError::Call(err)) => {
                assert_eq!(err.code(), INVALID_PAYLOAD_ATTRIBUTES, "{case}: {err:?}");
                assert!(
                    err.data()
                        .is_some_and(|data| data.get().contains("invalid payload attributes")),
                    "{case}: not rejected by the payload builder: {err:?}"
                );
            }
            other => panic!("{case}: expected INVALID_PAYLOAD_ATTRIBUTES, got {other:?}"),
        }
    }

    let valid = vec![deposit, valid_transfer];
    let updated = fcu(derived(valid.clone())).await?;
    assert!(updated.payload_status.is_valid(), "{updated:?}");
    let id = updated.payload_id.expect("valid derived attributes return a payload id");
    let payload: OpExecutionPayloadEnvelopeV3 =
        client.request("engine_getPayloadV3", rpc_params![id]).await?;
    assert_eq!(
        payload.execution_payload.payload_inner.payload_inner.transactions, valid,
        "the block keeps every derived transaction"
    );
    Ok(())
}
