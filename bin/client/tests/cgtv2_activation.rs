//! Synthetic receipt-to-output-root coverage through the real single-chain client.
//! Production addresses and deployed legacy state remain release inputs.

mod common;
use alloy_consensus::{Eip658Value, Header, Receipt, TrieAccount};
use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, B256, Bytes, Log, LogData, Sealable, U256, keccak256};
use alloy_rlp::Encodable;
use alloy_trie::{HashBuilder, Nibbles, proof::ProofRetainer};
use celo_derive::{CgtV2AttributesBuilder, ReceiptCacheProvider};
use celo_executor::CeloStatelessL2Builder;
use celo_genesis::CeloRollupConfig;
use celo_proof::CeloOracleL2ChainProvider;
use common::MockOracle;
use kona_derive::{AttributesBuilder, StatefulAttributesBuilder};
use kona_genesis::{ChainGenesis, HardForkConfig, RollupConfig, SystemConfig};
use kona_mpt::NoopTrieHinter;
use kona_preimage::PreimageKey;
use kona_proof::{boot::*, l1::OracleL1ChainProvider};
use kona_protocol::L2BlockInfo;
use std::{collections::HashMap, sync::Arc};

type Preimages = HashMap<PreimageKey, Vec<u8>>;
fn put(preimages: &mut Preimages, data: Vec<u8>) -> B256 {
    let hash = keccak256(&data);
    preimages.insert(PreimageKey::new_keccak256(*hash), data);
    hash
}
fn trie(preimages: &mut Preimages, mut leaves: Vec<(Nibbles, Vec<u8>)>) -> B256 {
    leaves.sort_by_key(|(key, _)| *key);
    let mut builder = HashBuilder::default()
        .with_proof_retainer(ProofRetainer::new(leaves.iter().map(|(key, _)| *key).collect()));
    for (key, value) in leaves {
        builder.add_leaf(key, &value);
    }
    let root = builder.root();
    for (_, node) in builder.take_proof_nodes().iter() {
        put(preimages, node.to_vec());
    }
    root
}
fn list(preimages: &mut Preimages, values: Vec<Vec<u8>>) -> B256 {
    trie(
        preimages,
        values
            .into_iter()
            .enumerate()
            .map(|(i, value)| (Nibbles::unpack(alloy_rlp::encode(i)), value))
            .collect(),
    )
}
fn legacy_state(preimages: &mut Preimages) -> (B256, B256) {
    let accounts: serde_json::Value = serde_json::from_str(include_str!(
        "../../../crates/celo-alloy/consensus/testdata/cgt_v2/cgtv2-fixture-prestate.json"
    ))
    .unwrap();
    let mut leaves = Vec::new();
    let mut message_root = B256::ZERO;
    for (address, account) in accounts.as_object().unwrap() {
        let address: Address = address.parse().unwrap();
        let code: Bytes = account["code"].as_str().unwrap().parse().unwrap();
        let code_hash = put(preimages, code.to_vec());
        let slots = account["storage"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(key, value)| {
                let key: B256 = key.parse().unwrap();
                let value: U256 = value.as_str().unwrap().parse().unwrap();
                (Nibbles::unpack(keccak256(key)), alloy_rlp::encode(value))
            })
            .collect();
        let storage_root = trie(preimages, slots);
        if address == alloy_primitives::address!("4200000000000000000000000000000000000016") {
            message_root = storage_root;
        }
        let account = TrieAccount {
            nonce: 0,
            balance: account["balance"].as_str().unwrap().parse().unwrap(),
            storage_root,
            code_hash,
        };
        leaves.push((Nibbles::unpack(keccak256(address)), alloy_rlp::encode(account)));
    }
    (trie(preimages, leaves), message_root)
}
fn output(state: B256, message: B256, block: B256) -> Vec<u8> {
    [B256::ZERO.as_slice(), state.as_slice(), message.as_slice(), block.as_slice()].concat()
}

fn portal_deposit(portal: Address, mint: u128, controller_call: bool) -> Log {
    let from = Address::with_last_byte(0x71);
    let to = if controller_call {
        alloy_primitives::address!("420000000000000000000000000000000000002a")
    } else {
        Address::with_last_byte(0x72)
    };
    let call = if controller_call { keccak256(b"owner()")[..4].to_vec() } else { Vec::new() };
    let length = 73 + call.len();
    let mut data = vec![0; 64 + length.div_ceil(32) * 32];
    data[31] = 32;
    data[63] = length as u8;
    data[80..96].copy_from_slice(&mint.to_be_bytes());
    data[96..128].copy_from_slice(&U256::from(mint).to_be_bytes::<32>());
    data[128..136].copy_from_slice(&100_000u64.to_be_bytes());
    data[137..137 + call.len()].copy_from_slice(&call);
    Log {
        address: portal,
        data: LogData::new_unchecked(
            vec![
                kona_protocol::DEPOSIT_EVENT_ABI_HASH,
                from.into_word(),
                to.into_word(),
                B256::ZERO,
            ],
            data.into(),
        ),
    }
}

fn reference_execution(
    cfg: &CeloRollupConfig,
    attrs: &op_alloy_rpc_types_engine::OpPayloadAttributes,
) -> (B256, Vec<celo_alloy_consensus::CeloReceiptEnvelope>) {
    use alloy_consensus::transaction::Recovered;
    use alloy_eips::Decodable2718;
    use alloy_evm::{
        EvmFactory,
        block::{BlockExecutor, BlockExecutorFactory},
    };
    use revm::{
        database::{EmptyDB, State},
        state::{AccountInfo, Bytecode},
    };
    let accounts: serde_json::Value = serde_json::from_str(include_str!(
        "../../../crates/celo-alloy/consensus/testdata/cgt_v2/cgtv2-fixture-prestate.json"
    ))
    .unwrap();
    let mut state = State::builder().with_database(EmptyDB::default()).build();
    for (address, account) in accounts.as_object().unwrap() {
        let code: Bytes = account["code"].as_str().unwrap().parse().unwrap();
        state.cache.insert_account_with_storage(
            address.parse().unwrap(),
            AccountInfo {
                balance: account["balance"].as_str().unwrap().parse().unwrap(),
                code_hash: keccak256(&code),
                code: Some(Bytecode::new_raw(code)),
                ..Default::default()
            },
            account["storage"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(slot, value)| {
                    (slot.parse().unwrap(), value.as_str().unwrap().parse().unwrap())
                })
                .collect(),
        );
    }
    let factory = alloy_celo_evm::block::CeloBlockExecutorFactory::<
        alloy_celo_evm::block::CeloAlloyReceiptBuilder,
        _,
    >::new(cfg.clone(), alloy_celo_evm::CeloEvmFactory::default());
    let mut env = alloy_evm::EvmEnv::<op_revm::OpSpecId>::default();
    env.cfg_env.chain_id = 1337;
    env.cfg_env.spec = op_revm::OpSpecId::GRANITE;
    env.block_env.number = U256::from(1);
    env.block_env.timestamp = U256::from(102);
    env.block_env.gas_limit = attrs.gas_limit.unwrap();
    env.block_env.basefee = 25_000_000_000;
    let mut executor = factory.create_executor(
        factory.evm_factory().create_evm(&mut state, env),
        alloy_op_evm::OpBlockExecutionCtx {
            parent_beacon_block_root: Some(
                attrs.payload_attributes.parent_beacon_block_root.unwrap_or_default(),
            ),
            ..Default::default()
        },
    );
    executor.apply_pre_execution_changes().unwrap();
    for encoded in attrs.transactions.as_ref().unwrap() {
        let tx = celo_alloy_consensus::CeloTxEnvelope::decode_2718_exact(encoded).unwrap();
        let from = tx.as_deposit().unwrap().from;
        executor.execute_transaction(&Recovered::new_unchecked(tx, from)).unwrap();
    }
    let (_, result) = executor.finish().unwrap();
    let mut images = HashMap::new();
    let accounts = state
        .cache
        .trie_account()
        .into_iter()
        .filter(|(_, account)| !account.info.is_empty())
        .map(|(address, account)| {
            let storage = account
                .storage
                .iter()
                .filter(|(_, value)| !value.is_zero())
                .map(|(slot, value)| {
                    (Nibbles::unpack(keccak256(slot.to_be_bytes::<32>())), alloy_rlp::encode(value))
                })
                .collect();
            let storage_root = trie(&mut images, storage);
            (
                Nibbles::unpack(keccak256(address)),
                alloy_rlp::encode(TrieAccount {
                    nonce: account.info.nonce,
                    balance: account.info.balance,
                    storage_root,
                    code_hash: account.info.code_hash,
                }),
            )
        })
        .collect();
    (trie(&mut images, accounts), result.receipts)
}

async fn fixture() -> (Preimages, B256) {
    let mut images = HashMap::new();
    let (state_root, message_root) = legacy_state(&mut images);
    let parent = Header {
        number: 0,
        timestamp: 100,
        gas_limit: 30_000_000,
        base_fee_per_gas: Some(25_000_000_000),
        state_root,
        ..Default::default()
    };
    let parent_hash = put(&mut images, alloy_rlp::encode(&parent));
    let l1_parent =
        Header { number: 10, timestamp: 100, base_fee_per_gas: Some(1), ..Default::default() };
    let l1_parent_hash = put(&mut images, alloy_rlp::encode(&l1_parent));
    let portal = Address::with_last_byte(0x44);
    let bridge = Address::with_last_byte(0xcc);
    let seeded = Log {
        address: bridge,
        data: LogData::new_unchecked(
            vec![keccak256(b"EscrowSeeded(address,uint256)"), portal.into_word()],
            U256::from(600).to_be_bytes::<32>().to_vec().into(),
        ),
    };
    let receipt = Receipt {
        status: Eip658Value::Eip658(true),
        logs: vec![portal_deposit(portal, 5, false), seeded, portal_deposit(portal, 0, true)],
        ..Default::default()
    };
    let mut receipt_bytes = Vec::new();
    receipt.clone().with_bloom().encode(&mut receipt_bytes);
    let root = list(&mut images, vec![receipt_bytes]);
    let activation = Header {
        parent_hash: l1_parent_hash,
        number: 11,
        timestamp: 102,
        base_fee_per_gas: Some(1),
        receipts_root: root,
        ..Default::default()
    };
    let origin_hash = put(&mut images, alloy_rlp::encode(&activation));
    // A later L1 head closes the sequencing window, so the normal pipeline derives an empty batch.
    let l1_head = Header {
        parent_hash: origin_hash,
        number: 12,
        timestamp: 104,
        base_fee_per_gas: Some(1),
        ..Default::default()
    };
    let l1_head_hash = put(&mut images, alloy_rlp::encode(&l1_head));
    let cfg = CeloRollupConfig {
        op_rollup_config: RollupConfig {
            l1_chain_id: 1,
            l2_chain_id: 1337.into(),
            block_time: 2,
            seq_window_size: 1,
            channel_timeout: 1,
            granite_channel_timeout: 1,
            deposit_contract_address: portal,
            genesis: ChainGenesis {
                l1: BlockNumHash::new(10, l1_parent_hash),
                l2: BlockNumHash::new(0, parent_hash),
                l2_time: 100,
                system_config: Some(SystemConfig { gas_limit: 30_000_000, ..Default::default() }),
            },
            hardforks: HardForkConfig {
                regolith_time: Some(0),
                canyon_time: Some(0),
                delta_time: Some(0),
                ecotone_time: Some(0),
                fjord_time: Some(0),
                granite_time: Some(0),
                ..Default::default()
            },
            ..Default::default()
        },
        espresso: Default::default(),
    };
    // NUT-based CGTv2 activation must work before the later Karst upgrade.
    assert!(cfg.op_rollup_config.hardforks.karst_time.is_none());
    let agreed_preimage = output(state_root, message_root, parent_hash);
    let agreed = put(&mut images, agreed_preimage);
    let oracle = Arc::new(MockOracle::from_preimages(images.clone()));
    let l1 = ReceiptCacheProvider::new(OracleL1ChainProvider::new(l1_head_hash, oracle.clone()));
    let l2 = CeloOracleL2ChainProvider::new(
        parent_hash,
        Arc::new(cfg.op_rollup_config.clone()),
        oracle.clone(),
    );
    let inner = StatefulAttributesBuilder::new(
        Arc::new(cfg.op_rollup_config.clone()),
        Arc::new(Default::default()),
        l2.clone(),
        l1.clone(),
        None,
    );
    let mut attributes =
        CgtV2AttributesBuilder::new(Arc::new(cfg.op_rollup_config.clone()), inner, l1);
    let parent_info = L2BlockInfo {
        block_info: kona_protocol::BlockInfo {
            hash: parent_hash,
            number: 0,
            timestamp: 100,
            ..Default::default()
        },
        l1_origin: BlockNumHash::new(10, l1_parent_hash),
        ..Default::default()
    };
    let attrs = attributes
        .prepare_payload_attributes(parent_info, BlockNumHash::new(11, origin_hash))
        .await
        .unwrap();
    assert_eq!(attrs.transactions.as_ref().unwrap().len(), 16);
    let shared: serde_json::Value =
        serde_json::from_str(include_str!("testdata/cgtv2-origin.json")).unwrap();
    let expected: Vec<Bytes> = serde_json::from_value(shared["transactions"].clone()).unwrap();
    assert_eq!(attrs.transactions.as_ref().unwrap(), &expected);
    assert_eq!(attrs.gas_limit.unwrap(), shared["gasLimit"].as_u64().unwrap());
    let reference_attrs = attrs.clone();
    let mut builder = CeloStatelessL2Builder::new(
        &cfg,
        Default::default(),
        l2,
        NoopTrieHinter,
        parent.seal_slow(),
    );
    let result = builder.build_block(attrs).unwrap();
    assert_eq!(result.execution_result.receipts.len(), 16);
    assert!(
        !result.execution_result.receipts[2].status(),
        "post-event zero-mint call still sees the uninitialized controller"
    );
    let (reference_root, reference_receipts) = reference_execution(&cfg, &reference_attrs);
    assert_eq!(
        result.header.state_root, reference_root,
        "stateful executor and trie-backed prover agree"
    );
    assert_eq!(result.execution_result.receipts, reference_receipts);
    let claimed = builder.compute_output_root().unwrap();
    assert_eq!(
        claimed,
        alloy_primitives::b256!("1abb58e433c95a3fba2fb4acf4bd90acd533de595de1699c7ee27a843ff6c688")
    );
    let tx_root = list(
        &mut images,
        reference_attrs.transactions.unwrap().iter().map(|tx| tx.to_vec()).collect(),
    );
    assert_eq!(tx_root, result.header.transactions_root);
    let post_hash = put(&mut images, alloy_rlp::encode(result.header.inner()));
    let post_oracle = Arc::new(MockOracle::from_preimages(images.clone()));
    let mut post_provider = CeloOracleL2ChainProvider::new(
        post_hash,
        Arc::new(cfg.op_rollup_config.clone()),
        post_oracle,
    );
    let recovered = kona_derive::L2ChainProvider::system_config_by_l2_hash(
        &mut post_provider,
        post_hash,
        Arc::new(cfg.op_rollup_config.clone()),
    )
    .await
    .unwrap();
    assert_eq!(recovered.gas_limit, 30_000_000);
    for (key, value) in [
        (L1_HEAD_KEY.to(), l1_head_hash.to_vec()),
        (L2_OUTPUT_ROOT_KEY.to(), agreed.to_vec()),
        (L2_CLAIM_KEY.to(), claimed.to_vec()),
        (L2_CLAIM_BLOCK_NUMBER_KEY.to(), 1u64.to_be_bytes().to_vec()),
        (L2_CHAIN_ID_KEY.to(), 1337u64.to_be_bytes().to_vec()),
        (L2_ROLLUP_CONFIG_KEY.to(), serde_json::to_vec(&cfg).unwrap()),
    ] {
        images.insert(PreimageKey::new_local(key), value);
    }
    (images, claimed)
}

#[tokio::test(flavor = "multi_thread")]
async fn cgt_v2_real_client_proves_activation_and_rejects_wrong_claim() {
    let (images, claimed) = fixture().await;
    let oracle = MockOracle::from_preimages(images.clone());
    celo_client::single::run(oracle.clone(), oracle).await.unwrap();
    let mut missing_receipt = images.clone();
    let topic = keccak256(b"EscrowSeeded(address,uint256)");
    let receipt_key = *missing_receipt
        .iter()
        .find(|(_, value)| value.windows(32).any(|word| word == topic.as_slice()))
        .unwrap()
        .0;
    missing_receipt.remove(&receipt_key);
    let oracle = MockOracle::from_preimages(missing_receipt);
    let l1_head = B256::from_slice(images.get(&PreimageKey::new_local(L1_HEAD_KEY.to())).unwrap());
    let mut provider = OracleL1ChainProvider::new(l1_head, Arc::new(oracle));
    // A missing preimage is a provider error. The driver intentionally retries temporary
    // provider failures, so test this boundary without starting its retry loop.
    let l1_header =
        kona_derive::ChainProvider::header_by_hash(&mut provider, l1_head).await.unwrap();
    assert!(
        kona_derive::ChainProvider::receipts_by_hash(&mut provider, l1_header.parent_hash)
            .await
            .is_err()
    );
    let mut wrong = images;
    wrong.insert(PreimageKey::new_local(L2_CLAIM_KEY.to()), B256::ZERO.to_vec());
    let oracle = MockOracle::from_preimages(wrong);
    assert!(celo_client::single::run(oracle.clone(), oracle).await.is_err());
    assert_ne!(claimed, B256::ZERO);
}
