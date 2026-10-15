//! Derives [`L2BlockInfo`] from Celo blocks through upstream kona's generic constructor, which
//! reads the L1-info deposit via `OpTransaction` on [`CeloTxEnvelope`].

use alloy_consensus::{BlockBody, Header, Signed};
use alloy_eips::BlockNumHash;
use alloy_primitives::{B256, Bytes, Sealed, Signature, b256};
use celo_alloy_consensus::{CeloBlock, CeloTxEnvelope, TxCip64};
use kona_genesis::ChainGenesis;
use kona_protocol::{BlockInfo, FromBlockError, L2BlockInfo, test_utils::RAW_BEDROCK_INFO_TX};
use op_alloy_consensus::TxDeposit;

fn genesis() -> ChainGenesis {
    ChainGenesis {
        l1: BlockNumHash { hash: B256::from([4; 32]), number: 2 },
        l2: BlockNumHash { hash: B256::from([5; 32]), number: 1 },
        ..Default::default()
    }
}

fn block_with(transactions: Vec<CeloTxEnvelope>) -> CeloBlock {
    CeloBlock {
        header: Header {
            number: 3,
            parent_hash: b256!("0202020202020202020202020202020202020202020202020202020202020202"),
            timestamp: 1,
            ..Default::default()
        },
        body: BlockBody { transactions, ommers: Vec::new(), withdrawals: None },
    }
}

#[test]
fn celo_block_with_l1_info_deposit() {
    let deposit = CeloTxEnvelope::Deposit(Sealed::new(TxDeposit {
        input: Bytes::from(&RAW_BEDROCK_INFO_TX),
        ..Default::default()
    }));

    let info = L2BlockInfo::from_block_and_genesis(&block_with(vec![deposit]), &genesis()).unwrap();

    assert_eq!(
        info,
        L2BlockInfo {
            block_info: BlockInfo {
                hash: b256!("e65ecd961cee8e4d2d6e1d424116f6fe9a794df0244578b6d5860a3d2dfcd97e"),
                number: 3,
                parent_hash: b256!(
                    "0202020202020202020202020202020202020202020202020202020202020202"
                ),
                timestamp: 1,
            },
            l1_origin: BlockNumHash {
                hash: b256!("392012032675be9f94aae5ab442de73c5f4fb1bf30fa7dd0d2442239899a40fc"),
                number: 18334955,
            },
            seq_num: 4,
        }
    );
}

#[test]
fn celo_block_led_by_cip64_tx_is_rejected() {
    let cip64 = CeloTxEnvelope::Cip64(Signed::new_unchecked(
        TxCip64::default(),
        Signature::test_signature(),
        B256::ZERO,
    ));

    let err =
        L2BlockInfo::from_block_and_genesis(&block_with(vec![cip64]), &genesis()).unwrap_err();

    assert_eq!(err, FromBlockError::FirstTxNonDeposit(0x7b));
}

#[test]
fn celo_genesis_block_with_wrong_hash_is_rejected() {
    let block = CeloBlock {
        header: Header {
            number: 1,
            parent_hash: B256::from([2; 32]),
            timestamp: 1,
            ..Default::default()
        },
        body: Default::default(),
    };

    let err = L2BlockInfo::from_block_and_genesis(&block, &genesis()).unwrap_err();

    assert_eq!(err, FromBlockError::InvalidGenesisHash);
}
