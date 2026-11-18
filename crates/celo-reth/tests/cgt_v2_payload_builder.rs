//! CGTv2 fallback coverage through the real OP payload builder.

use alloy_consensus::{Header, TxReceipt};
use alloy_eips::Encodable2718;
use alloy_genesis::GenesisAccount;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, keccak256};
use alloy_rpc_types_engine::{PayloadAttributes, PayloadId};
use celo_alloy_consensus::cgt_v2_group::{
    CGT_V2_CONTROLLER, CGT_V2_DEPOSITOR, CGT_V2_GROUP_GAS, CGT_V2_L1_BLOCK, CgtV2UpgradeGroup,
};
use celo_reth::{
    CeloEvmConfig, cgt_v2::CgtV2PayloadBuilder, pool::CeloPoolTx, primitives::CeloPrimitives,
};
use op_alloy_consensus::TxDeposit;
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use reth_basic_payload_builder::{BuildArguments, BuildOutcome, PayloadBuilder, PayloadConfig};
use reth_chainspec::Chain;
use reth_node_api::BuiltPayload;
use reth_optimism_chainspec::OpChainSpecBuilder;
use reth_optimism_payload_builder::{OpPayloadAttrs, OpPayloadBuilder};
use reth_primitives_traits::SealedHeader;
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
use reth_transaction_pool::noop::NoopTransactionPool;
use std::{collections::BTreeMap, sync::Arc};

#[test]
fn cgt_v2_fallback_block_preserves_l1_info_and_origin_deposits() {
    let chain_spec = Arc::new(
        OpChainSpecBuilder::default()
            .chain(Chain::from_id(1337))
            .genesis(Default::default())
            .granite_activated()
            .build(),
    );
    let provider =
        MockEthProvider::<CeloPrimitives>::new().with_chain_spec(chain_spec.as_ref().clone());
    let mut accounts: BTreeMap<Address, GenesisAccount> = serde_json::from_str(include_str!(
        "../../celo-alloy/consensus/testdata/cgt_v2/cgtv2-fixture-prestate.json"
    ))
    .unwrap();
    // An initialized controller makes the real upgrade initializer revert.
    accounts
        .get_mut(&CGT_V2_CONTROLLER)
        .unwrap()
        .storage
        .get_or_insert_default()
        .insert(B256::ZERO, B256::with_last_byte(1));
    for (address, account) in accounts {
        provider.add_account(
            address,
            ExtendedAccount::new(account.nonce.unwrap_or_default(), account.balance)
                .with_bytecode(account.code.unwrap_or_default())
                .extend_storage(
                    account
                        .storage
                        .into_iter()
                        .flatten()
                        .map(|(slot, value)| (slot, U256::from_be_bytes(value.0))),
                ),
        );
    }
    let inner = OpPayloadBuilder::new(
        NoopTransactionPool::<CeloPoolTx>::new(),
        provider.clone(),
        CeloEvmConfig::celo(chain_spec),
    );
    let builder = CgtV2PayloadBuilder::new(inner, provider, 1337);

    let mut l1_info_data = keccak256(
        b"setL1BlockValues(uint64,uint64,uint256,bytes32,uint64,bytes32,uint256,uint256)",
    )[..4]
        .to_vec();
    l1_info_data.extend_from_slice(&[0; 256]);
    let l1_info = TxDeposit {
        from: CGT_V2_DEPOSITOR,
        to: TxKind::Call(CGT_V2_L1_BLOCK),
        gas_limit: 200_000,
        source_hash: B256::with_last_byte(1),
        input: l1_info_data.into(),
        ..Default::default()
    };
    let origin_deposit = |index, amount| TxDeposit {
        from: Address::with_last_byte(0xd1),
        to: TxKind::Call(Address::with_last_byte(0x70 + index)),
        gas_limit: 100_000,
        source_hash: B256::with_last_byte(index),
        mint: amount,
        value: U256::from(amount),
        ..Default::default()
    };
    let prefix: Vec<Bytes> = [l1_info, origin_deposit(2, 5), origin_deposit(3, 7)]
        .iter()
        .map(|tx| tx.encoded_2718().into())
        .collect();
    let mut transactions = prefix.clone();
    transactions.extend(
        CgtV2UpgradeGroup::deposits(1337, U256::from(600))
            .unwrap()
            .iter()
            .map(|tx| Bytes::from(tx.encoded_2718())),
    );
    let parent_header = SealedHeader::seal_slow(Header {
        base_fee_per_gas: Some(25_000_000_000),
        gas_limit: 30_000_000,
        gas_used: 15_000_000,
        excess_blob_gas: Some(0),
        blob_gas_used: Some(0),
        parent_beacon_block_root: Some(B256::ZERO),
        ..Default::default()
    });
    let parent_hash = parent_header.hash();
    let id = PayloadId::new([1; 8]);
    let attrs = OpPayloadAttrs(OpPayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp: 1,
            parent_beacon_block_root: Some(B256::ZERO),
            ..Default::default()
        },
        transactions: Some(transactions),
        gas_limit: Some(30_000_000 + CGT_V2_GROUP_GAS),
        ..Default::default()
    });
    let config = PayloadConfig::new(Arc::new(parent_header), attrs, id);

    let BuildOutcome::Freeze(payload) = builder
        .try_build(BuildArguments::new(
            Default::default(),
            None,
            None,
            config,
            Default::default(),
            None,
        ))
        .expect("fallback must build from the original parent")
    else {
        panic!("fallback must return a final payload");
    };
    assert_eq!(payload.id(), id);
    assert_eq!(payload.block().header().parent_hash, parent_hash);
    assert_eq!(payload.block().header().gas_limit, 30_000_000);
    let block_transactions: Vec<Bytes> =
        payload.block().body().transactions.iter().map(|tx| tx.encoded_2718().into()).collect();
    assert_eq!(block_transactions, prefix);
    let executed = payload.executed_block().unwrap();
    let receipts = &executed.execution_output.result.receipts;
    assert_eq!(receipts.len(), prefix.len());
    assert!(receipts.iter().all(TxReceipt::status));
}
