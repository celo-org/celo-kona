//! Block Types for Celo.

use alloy_consensus::{Block, Transaction, Typed2718};
use alloy_eips::BlockNumHash;
use celo_alloy_consensus::CeloTxEnvelope;
use derive_more::Display;
use kona_genesis::ChainGenesis;
use kona_protocol::{BlockInfo, FromBlockError, L1BlockInfoTx, L2BlockInfo};

/// L2 Block Header Info
#[derive(Debug, Display, Clone, Copy, Hash, Eq, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
#[display("CeloL2BlockInfo {{ op_l2_block_info: {op_l2_block_info} }}")]
pub struct CeloL2BlockInfo {
    /// OP L2 block header info
    pub op_l2_block_info: L2BlockInfo,
}

#[cfg(feature = "arbitrary")]
impl arbitrary::Arbitrary<'_> for CeloL2BlockInfo {
    fn arbitrary(g: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<Self> {
        Ok(Self {
            op_l2_block_info: L2BlockInfo {
                block_info: g.arbitrary()?,
                l1_origin: BlockNumHash {
                    number: g.arbitrary()?,
                    hash: g.arbitrary()?,
                },
                seq_num: g.arbitrary()?,
            },
        })
    }
}

impl CeloL2BlockInfo {
    /// Instantiates a new [CeloL2BlockInfo].
    pub const fn new(block_info: BlockInfo, l1_origin: BlockNumHash, seq_num: u64) -> Self {
        Self {
            op_l2_block_info: L2BlockInfo::new(block_info, l1_origin, seq_num),
        }
    }

    /// Constructs an [`CeloL2BlockInfo`] from a given [`alloy_rpc_types_eth::Block`] and
    /// [`ChainGenesis`].
    pub fn from_rpc_block_and_genesis(
        block: alloy_rpc_types_eth::Block<celo_alloy_rpc_types::CeloTransaction>,
        genesis: &ChainGenesis,
    ) -> Result<Self, FromBlockError> {
        let block_info = BlockInfo::new(
            block.header.hash,
            block.header.inner.number,
            block.header.inner.parent_hash,
            block.header.inner.timestamp,
        );
        if block_info.number == genesis.l2.number {
            if block_info.hash != genesis.l2.hash {
                return Err(FromBlockError::InvalidGenesisHash);
            }
            return Ok(Self {
                op_l2_block_info: L2BlockInfo::new(block_info, genesis.l1, 0),
            });
        }
        Self::from_block_and_genesis(&block.into_consensus(), genesis)
    }

    /// Constructs a [CeloL2BlockInfo] from a given Celo [Block] and [ChainGenesis].
    pub fn from_block_and_genesis<T: Typed2718 + AsRef<CeloTxEnvelope>>(
        block: &Block<T>,
        genesis: &ChainGenesis,
    ) -> Result<Self, FromBlockError> {
        let block_info = BlockInfo::from(block);

        let (l1_origin, sequence_number) = if block_info.number == genesis.l2.number {
            if block_info.hash != genesis.l2.hash {
                return Err(FromBlockError::InvalidGenesisHash);
            }
            (genesis.l1, 0)
        } else {
            if block.body.transactions.is_empty() {
                return Err(FromBlockError::MissingL1InfoDeposit(block_info.hash));
            }

            let tx = block.body.transactions[0].as_ref();
            let Some(tx) = tx.as_deposit() else {
                return Err(FromBlockError::FirstTxNonDeposit(tx.ty()));
            };

            let l1_info = L1BlockInfoTx::decode_calldata(tx.input().as_ref())
                .map_err(FromBlockError::BlockInfoDecodeError)?;
            (l1_info.id(), l1_info.sequence_number())
        };

        Ok(Self {
            op_l2_block_info: L2BlockInfo::new(block_info, l1_origin, sequence_number),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_primitives::{B256, b256};
    use celo_alloy_consensus::CeloBlock;

    #[test]
    fn test_rpc_block_into_info() {
        let block: alloy_rpc_types_eth::Block<CeloTxEnvelope> = alloy_rpc_types_eth::Block {
            header: alloy_rpc_types_eth::Header {
                hash: b256!("04d6fefc87466405ba0e5672dcf5c75325b33e5437da2a42423080aab8be889b"),
                inner: Header {
                    number: 1,
                    parent_hash: b256!(
                        "0202020202020202020202020202020202020202020202020202020202020202"
                    ),
                    timestamp: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let expected = BlockInfo {
            hash: b256!("04d6fefc87466405ba0e5672dcf5c75325b33e5437da2a42423080aab8be889b"),
            number: 1,
            parent_hash: b256!("0202020202020202020202020202020202020202020202020202020202020202"),
            timestamp: 1,
        };
        let block = block.into_consensus();
        assert_eq!(BlockInfo::from(block), expected);
    }

    // TODO: enable it once test_utils mod added
    // #[test]
    // fn test_from_block_and_genesis() {
    //     use crate::test_utils::RAW_BEDROCK_INFO_TX;
    //     let genesis = ChainGenesis {
    //         l1: BlockNumHash { hash: B256::from([4; 32]), number: 2 },
    //         l2: BlockNumHash { hash: B256::from([5; 32]), number: 1 },
    //         ..Default::default()
    //     };
    //     let tx_env = alloy_rpc_types_eth::Transaction {
    //         inner: alloy_consensus::transaction::Recovered::new_unchecked(
    //             op_alloy_consensus::OpTxEnvelope::Deposit(alloy_primitives::Sealed::new(
    //                 op_alloy_consensus::TxDeposit {
    //                     input: alloy_primitives::Bytes::from(&RAW_BEDROCK_INFO_TX),
    //                     ..Default::default()
    //                 },
    //             )),
    //             Default::default(),
    //         ),
    //         block_hash: None,
    //         block_number: Some(1),
    //         effective_gas_price: Some(1),
    //         transaction_index: Some(0),
    //     };
    //     let block: alloy_rpc_types_eth::Block<op_alloy_rpc_types::Transaction> =
    //         alloy_rpc_types_eth::Block {
    //             header: alloy_rpc_types_eth::Header {
    //                 hash: b256!("04d6fefc87466405ba0e5672dcf5c75325b33e5437da2a42423080aab8be889b"),
    //                 inner: alloy_consensus::Header {
    //                     number: 3,
    //                     parent_hash: b256!(
    //                         "0202020202020202020202020202020202020202020202020202020202020202"
    //                     ),
    //                     timestamp: 1,
    //                     ..Default::default()
    //                 },
    //                 ..Default::default()
    //             },
    //             transactions: alloy_rpc_types_eth::BlockTransactions::Full(vec![
    //                 op_alloy_rpc_types::Transaction {
    //                     inner: tx_env,
    //                     deposit_nonce: None,
    //                     deposit_receipt_version: None,
    //                 },
    //             ]),
    //             ..Default::default()
    //         };
    //     let expected = L2BlockInfo {
    //         block_info: BlockInfo {
    //             hash: b256!("e65ecd961cee8e4d2d6e1d424116f6fe9a794df0244578b6d5860a3d2dfcd97e"),
    //             number: 3,
    //             parent_hash: b256!(
    //                 "0202020202020202020202020202020202020202020202020202020202020202"
    //             ),
    //             timestamp: 1,
    //         },
    //         l1_origin: BlockNumHash {
    //             hash: b256!("392012032675be9f94aae5ab442de73c5f4fb1bf30fa7dd0d2442239899a40fc"),
    //             number: 18334955,
    //         },
    //         seq_num: 4,
    //     };
    //     let block = block.into_consensus();
    //     let derived = L2BlockInfo::from_block_and_genesis(&block, &genesis).unwrap();
    //     assert_eq!(derived, expected);
    // }

    #[test]
    fn test_l2_block_info_invalid_genesis_hash() {
        let genesis = ChainGenesis {
            l1: BlockNumHash {
                hash: B256::from([4; 32]),
                number: 2,
            },
            l2: BlockNumHash {
                hash: B256::from([5; 32]),
                number: 1,
            },
            ..Default::default()
        };
        let celo_block = CeloBlock {
            header: Header {
                number: 1,
                parent_hash: B256::from([2; 32]),
                timestamp: 1,
                ..Default::default()
            },
            body: Default::default(),
        };
        let err = CeloL2BlockInfo::from_block_and_genesis(&celo_block, &genesis).unwrap_err();
        assert_eq!(err, FromBlockError::InvalidGenesisHash);
    }

    #[test]
    #[cfg(feature = "arbitrary")]
    fn test_arbitrary_l2_block_info() {
        use arbitrary::Arbitrary;
        use rand::Rng;
        let mut bytes = [0u8; 1024];
        rand::rng().fill(bytes.as_mut_slice());
        CeloL2BlockInfo::arbitrary(&mut arbitrary::Unstructured::new(&bytes)).unwrap();
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_deserialize_l2_block_info() {
        let l2_block_info = CeloL2BlockInfo {
            op_l2_block_info: L2BlockInfo {
                block_info: BlockInfo {
                    hash: B256::from([1; 32]),
                    number: 1,
                    parent_hash: B256::from([2; 32]),
                    timestamp: 1,
                },
                l1_origin: BlockNumHash {
                    hash: B256::from([3; 32]),
                    number: 2,
                },
                seq_num: 3,
            },
        };

        let json = r#"{
            "opL2BlockInfo": {
                "hash": "0x0101010101010101010101010101010101010101010101010101010101010101",
                "number": 1,
                "parentHash": "0x0202020202020202020202020202020202020202020202020202020202020202",
                "timestamp": 1,
                "l1origin": {
                    "hash": "0x0303030303030303030303030303030303030303030303030303030303030303",
                    "number": 2
                },
                "sequenceNumber": 3
            }
        }"#;

        let deserialized: CeloL2BlockInfo = serde_json::from_str(json).unwrap();
        assert_eq!(deserialized, l2_block_info);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_deserialize_l2_block_info_hex() {
        let l2_block_info = CeloL2BlockInfo {
            op_l2_block_info: L2BlockInfo {
                block_info: BlockInfo {
                    hash: B256::from([1; 32]),
                    number: 1,
                    parent_hash: B256::from([2; 32]),
                    timestamp: 1,
                },
                l1_origin: BlockNumHash {
                    hash: B256::from([3; 32]),
                    number: 2,
                },
                seq_num: 3,
            },
        };

        let json = r#"{
            "opL2BlockInfo": {
                "hash": "0x0101010101010101010101010101010101010101010101010101010101010101",
                "number": 1,
                "parentHash": "0x0202020202020202020202020202020202020202020202020202020202020202",
                "timestamp": 1,
                "l1origin": {
                    "hash": "0x0303030303030303030303030303030303030303030303030303030303030303",
                    "number": 2
                },
                "sequenceNumber": 3
            }
        }"#;

        let deserialized: CeloL2BlockInfo = serde_json::from_str(json).unwrap();
        assert_eq!(deserialized, l2_block_info);
    }
}
