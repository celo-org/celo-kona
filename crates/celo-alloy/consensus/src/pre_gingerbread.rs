//! Pre-Gingerbread Celo block header encoding.
//!
//! Mirrors op-geth's `core/types/celo_block.go::BeforeGingerbreadHeader`. The
//! original Celo L1 used IBFT consensus with a header that omits all the
//! Ethereum PoW-era fields (`ommers_hash`, `difficulty`, `gas_limit`,
//! `mix_hash`, `nonce`) and EIP-1559+ extensions. Encoding a post-Gingerbread
//! [`alloy_consensus::Header`] with these fields stripped reproduces the
//! original block hash — required to match the genesis hash op-geth peers
//! announce on celo-mainnet handshake (`0x19ea3339…1bd11c3`).
//!
//! Only encoding is implemented: reth as an OP Stack node never syncs
//! pre-bedrock blocks (those go through `--rollup.historicalrpc`), so we never
//! need to RLP-decode this format. The single use site is recomputing the
//! sealed genesis-block hash for celo-mainnet so peers don't disconnect on
//! `mismatched genesis in status message`.

use alloy_consensus::Header;
use alloy_primitives::{Address, B256, Bloom, U256, keccak256};
use alloy_rlp::{Encodable, RlpEncodable};

/// RLP form of a pre-Gingerbread Celo block header — exactly the 10 fields op-geth's
/// `BeforeGingerbreadHeader` serialises, in order.
#[derive(RlpEncodable, Debug)]
struct BeforeGingerbreadHeader<'a> {
    parent_hash: &'a B256,
    coinbase: &'a Address,
    state_root: &'a B256,
    transactions_root: &'a B256,
    receipts_root: &'a B256,
    logs_bloom: &'a Bloom,
    number: U256,
    gas_used: u64,
    timestamp: u64,
    extra_data: &'a [u8],
}

/// Recomputes the sealed block hash of a header using the pre-Gingerbread
/// encoding. Pass an [`alloy_consensus::Header`] populated from a Celo L1
/// genesis (or any pre-Gingerbread block); fields not present in the legacy
/// 10-field layout are ignored.
pub fn pre_gingerbread_header_hash(header: &Header) -> B256 {
    let pre = BeforeGingerbreadHeader {
        parent_hash: &header.parent_hash,
        coinbase: &header.beneficiary,
        state_root: &header.state_root,
        transactions_root: &header.transactions_root,
        receipts_root: &header.receipts_root,
        logs_bloom: &header.logs_bloom,
        number: U256::from(header.number),
        gas_used: header.gas_used,
        timestamp: header.timestamp,
        extra_data: header.extra_data.as_ref(),
    };
    let mut buf = std::vec::Vec::with_capacity(pre.length());
    pre.encode(&mut buf);
    keccak256(&buf)
}
