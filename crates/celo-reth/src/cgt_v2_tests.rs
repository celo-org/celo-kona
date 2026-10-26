use super::*;
use alloy_consensus::{Block, BlockBody, Sealable, SignableTransaction, TxLegacy};
use alloy_primitives::Signature;
use celo_alloy_consensus::{CeloTxEnvelope, cgt_v2_group::CgtV2UpgradeGroup};
use reth_node_api::payload::PayloadTypes;
use reth_optimism_payload_builder::OpPayloadTypes;

fn block(activation: bool, user_first: bool) -> SealedBlock<Block<CeloTxEnvelope>> {
    let user = CeloTxEnvelope::Legacy(
        TxLegacy { gas_limit: 21_000, ..Default::default() }.into_signed(Signature::new(
            U256::ZERO,
            U256::ZERO,
            false,
        )),
    );
    let mut transactions = vec![user];
    if activation {
        let signal = CgtV2UpgradeGroup::deposits(1337, U256::from(600)).unwrap().remove(0);
        transactions.insert(usize::from(user_first), CeloTxEnvelope::Deposit(signal.seal_slow()));
    }
    SealedBlock::seal_slow(Block {
        header: Header::default(),
        body: BlockBody { transactions, ..Default::default() },
    })
}

#[test]
fn cgt_v2_import_marks_activation_blocks_deposits_only() {
    let config = CeloEvmConfig::celo(Arc::new(
        reth_optimism_chainspec::OpChainSpecBuilder::default()
            .chain(reth_chainspec::Chain::from_id(42220))
            .genesis(Default::default())
            .granite_activated()
            .build(),
    ));
    for user_first in [false, true] {
        let block = block(true, user_first);
        assert!(
            config.context_for_block(&block).unwrap().no_user_tx_activation_block,
            "import must reject user txs on either side of the signal",
        );
    }
    assert!(!config.context_for_block(&block(false, false)).unwrap().no_user_tx_activation_block);
}

#[test]
fn cgt_v2_engine_payload_marks_activation_blocks_deposits_only() {
    let config = CeloEvmConfig::celo(Arc::new(
        reth_optimism_chainspec::OpChainSpecBuilder::default()
            .chain(reth_chainspec::Chain::from_id(42220))
            .genesis(Default::default())
            .granite_activated()
            .build(),
    ));
    for activation in [false, true] {
        let payload = <OpPayloadTypes<CeloPrimitives> as PayloadTypes>::block_to_payload(
            block(activation, true),
            None,
        );
        assert_eq!(
            config.context_for_payload(&payload).unwrap().no_user_tx_activation_block,
            activation,
            "unsafe payload execution must enforce the same rule as block import",
        );
    }
}

#[test]
fn cgt_v2_import_rejects_user_execution_on_either_side_of_signal() {
    use alloy_consensus::transaction::Recovered;
    use alloy_primitives::Address;
    use revm::database::{CacheDB, EmptyDB, State};

    let chain_spec = Arc::new(
        reth_optimism_chainspec::OpChainSpecBuilder::default()
            .chain(reth_chainspec::Chain::from_id(42220))
            .genesis(Default::default())
            .granite_activated()
            .build(),
    );
    let config = CeloEvmConfig::celo(chain_spec);
    for user_first in [false, true] {
        let block = block(true, user_first);
        let mut db = State::builder().with_database(CacheDB::<EmptyDB>::default()).build();
        let mut executor = config.executor_for_block(&mut db, &block).unwrap();
        let user = &block.body().transactions[usize::from(!user_first)];
        let error = executor
            .execute_transaction(Recovered::new_unchecked(user, Address::ZERO))
            .unwrap_err();
        assert!(error.to_string().contains("unexpected non-deposit transactions"), "{error}");
    }
}
