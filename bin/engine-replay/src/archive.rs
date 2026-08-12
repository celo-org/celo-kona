//! The block archive: a self-contained, offline record of a canonical block range.
//!
//! The archive exists because of an ordering constraint that is easy to get wrong: `celo-reth
//! stage unwind to-block N` deletes the headers and bodies above `N`, which are the only local
//! source of the attributes and expected hashes a replay needs. So the range must be archived
//! **before** the datadir is rewound, and after that the replay is fully offline — no second
//! node, no public RPC, no network.
//!
//! One JSON object per line, one line per block, `chain_id` repeated on every line so a partial
//! file is still self-describing and appendable.

use crate::{
    NotReplayable,
    attrs::{engine_version, payload_attributes},
    rpc,
};
use alloy_consensus::{Header, proofs::ordered_trie_root_encoded};
use alloy_primitives::{B256, Bytes};
use alloy_rlp::Decodable;
use anyhow::{Context, bail};
use jsonrpsee::core::client::ClientT;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{BufRead, BufReader, BufWriter, Write},
    path::Path,
};
use tracing::info;

/// One canonical block, in the minimal form a replay needs.
///
/// The RLP header is stored rather than a field-by-field decomposition: it is authoritative
/// (its keccak *is* the block hash), compact, and immune to the driver's own view of which
/// header fields matter drifting away from consensus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ArchivedBlock {
    /// Chain the block was read from. Checked against the replay node's `eth_chainId`.
    pub(crate) chain_id: u64,
    /// Block height.
    pub(crate) number: u64,
    /// Sealed block hash — the value the replay asserts against.
    pub(crate) hash: B256,
    /// Parent's sealed block hash — the forkchoice head the build runs on top of.
    pub(crate) parent_hash: B256,
    /// RLP-encoded consensus header, from `debug_getRawHeader`.
    pub(crate) header: Bytes,
    /// EIP-2718-encoded transactions in block order, from `debug_getRawTransactions`.
    pub(crate) transactions: Vec<Bytes>,
}

impl ArchivedBlock {
    /// Decode the stored consensus header.
    pub(crate) fn decode_header(&self) -> anyhow::Result<Header> {
        Header::decode(&mut self.header.as_ref())
            .with_context(|| format!("failed to RLP-decode the header of block {}", self.number))
    }
}

/// Check that the stored transaction list is exactly the block's body.
///
/// This one assertion subsumes a whole class of harness-caused hash mismatches. A truncated or
/// silently empty body, a reordered list, and a non-canonical EIP-2718 encoding all change this
/// root — and every one of them would otherwise surface mid-run as a block-hash mismatch that
/// reads like a consensus bug. Failing here instead makes it unambiguous.
fn check_transactions_bind_to_header(
    header: &Header,
    transactions: &[Bytes],
) -> anyhow::Result<()> {
    let root = ordered_trie_root_encoded(transactions);
    if root != header.transactions_root {
        bail!(
            "block {}: the {} stored transactions hash to transactionsRoot {root}, but the header \
             says {}; the body is incomplete, reordered, or not canonically EIP-2718 encoded",
            header.number,
            transactions.len(),
            header.transactions_root,
        );
    }
    Ok(())
}

/// Read a canonical range off a synced node and write it to `out` as JSONL.
///
/// Every block is checked for replayability as it is archived — header decodes, keccak matches
/// the node's reported hash, the range is contiguous, and the attributes derive cleanly. A block
/// that cannot be reproduced through the build path (an unrecognised `extraData` layout, a
/// pre-Ecotone block) fails here, where it reads as "this range is not replayable", rather than
/// hours later as a hash mismatch that looks like a consensus bug.
pub(crate) async fn archive<C: ClientT + Sync>(
    client: &C,
    from: u64,
    to: u64,
    out: &Path,
) -> anyhow::Result<()> {
    if from == 0 {
        bail!("--from must be at least 1: block 0 has no parent to build on top of");
    }
    if from > to {
        bail!("--from {from} is above --to {to}");
    }

    let chain_id = rpc::chain_id(client).await?;
    let (head_number, ..) = rpc::canonical_head(client).await?;
    if to > head_number {
        bail!("--to {to} is above the node's head {head_number}");
    }
    info!(chain_id, from, to, head_number, "Archiving canonical range");

    let file =
        File::create(out).with_context(|| format!("failed to create archive {}", out.display()))?;
    let mut writer = BufWriter::new(file);
    let mut previous_hash: Option<B256> = None;

    for number in from..=to {
        let (hash, parent_hash) = rpc::block_hash(client, number).await?;
        let header_rlp = rpc::raw_header(client, number).await?;
        let transactions = rpc::raw_transactions(client, number).await?;

        let block =
            ArchivedBlock { chain_id, number, hash, parent_hash, header: header_rlp, transactions };
        let header = block.decode_header()?;

        // The archive is only useful if it is exactly what the node has. Check that here, once,
        // rather than trusting it during a measurement run.
        if header.number != number {
            bail!("debug_getRawHeader({number}) decoded to block {}", header.number);
        }
        if header.hash_slow() != hash {
            bail!(
                "block {number}: keccak of the raw header is {} but the node reports {hash}",
                header.hash_slow(),
            );
        }
        if header.parent_hash != parent_hash {
            bail!(
                "block {number}: raw header's parentHash {} disagrees with the node's {parent_hash}",
                header.parent_hash,
            );
        }
        if let Some(previous) = previous_hash &&
            previous != parent_hash
        {
            bail!(
                "block {number} does not follow block {}: parentHash {parent_hash} != {previous}",
                number - 1,
            );
        }
        check_transactions_bind_to_header(&header, &block.transactions)?;

        // Fail here, not during the run, if this block can never be reproduced.
        engine_version(&header).map_err(|e| anyhow::Error::new(NotReplayable(e.to_string())))?;
        payload_attributes(&header, block.transactions.clone())
            .map_err(|e| anyhow::Error::new(NotReplayable(e.to_string())))?;

        serde_json::to_writer(&mut writer, &block)
            .with_context(|| format!("failed to serialise block {number}"))?;
        writer.write_all(b"\n")?;

        previous_hash = Some(hash);
        if number % 1_000 == 0 || number == to {
            info!(number, "Archived");
        }
    }

    writer.flush().context("failed to flush the archive")?;
    info!(blocks = to - from + 1, path = %out.display(), "Archive written");
    Ok(())
}

/// Load a JSONL archive back into memory.
///
/// Archives are ranges of blocks a laptop can hold, so this reads eagerly: the replay loop must
/// not be doing file I/O between the calls it is timing.
pub(crate) fn load(path: &Path) -> anyhow::Result<Vec<ArchivedBlock>> {
    let file =
        File::open(path).with_context(|| format!("failed to open archive {}", path.display()))?;
    let mut blocks = Vec::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line.with_context(|| format!("failed to read {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let block: ArchivedBlock = serde_json::from_str(&line).with_context(|| {
            format!("{}:{}: malformed archive record", path.display(), index + 1)
        })?;
        blocks.push(block);
    }
    if blocks.is_empty() {
        bail!("archive {} is empty", path.display());
    }
    // Re-check the two invariants the replay loop relies on, in case the archive was edited or
    // concatenated by hand. Both would otherwise show up as a confusing arithmetic panic or as a
    // forkchoice rejection far from its cause.
    if blocks[0].number == 0 {
        bail!("archive {} starts at block 0, which has no parent to build on", path.display());
    }
    for pair in blocks.windows(2) {
        let (previous, next) = (&pair[0], &pair[1]);
        if next.number != previous.number + 1 || next.parent_hash != previous.hash {
            bail!(
                "archive {} is not a contiguous chain: block {} does not follow block {}",
                path.display(),
                next.number,
                previous.number,
            );
        }
    }
    // Re-bind every body to its header. `archive` already did this, but re-checking here is what
    // makes a hand-edited or concatenated archive impossible to mistake for a node bug. It runs
    // before any timed call, so it cannot distort a measurement.
    for block in &blocks {
        check_transactions_bind_to_header(&block.decode_header()?, &block.transactions)
            .with_context(|| format!("archive {}", path.display()))?;
    }
    Ok(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::b256;

    #[test]
    fn test_archived_block_round_trips_through_jsonl() {
        let header = Header { number: 7, gas_limit: 30_000_000, ..Default::default() };
        let mut encoded = Vec::new();
        alloy_rlp::Encodable::encode(&header, &mut encoded);

        let block = ArchivedBlock {
            chain_id: 1337,
            number: 7,
            hash: header.hash_slow(),
            parent_hash: b256!(
                "0x2222222222222222222222222222222222222222222222222222222222222222"
            ),
            header: encoded.into(),
            transactions: vec![Bytes::from_static(&[0x7b, 0x01, 0x02])],
        };

        let line = serde_json::to_string(&block).unwrap();
        assert!(!line.contains('\n'), "a record must fit on one JSONL line");

        let decoded: ArchivedBlock = serde_json::from_str(&line).unwrap();
        assert_eq!(decoded.number, 7);
        assert_eq!(decoded.chain_id, 1337);
        assert_eq!(decoded.decode_header().unwrap().gas_limit, 30_000_000);
        assert_eq!(decoded.decode_header().unwrap().hash_slow(), block.hash);
        assert_eq!(decoded.transactions[0][0], 0x7b);
    }

    /// The body must hash to the header's `transactionsRoot`, and any perturbation of the list
    /// must be rejected. The encodings here are opaque bytes on purpose: the trie root is over
    /// the EIP-2718 encodings themselves, so this is exactly the check a real body gets.
    #[test]
    fn test_transaction_list_is_bound_to_the_header() {
        let txs: Vec<Bytes> = vec![
            Bytes::from_static(&[0x7e, 0xaa, 0xbb]),
            Bytes::from_static(&[0x02, 0xcc]),
            Bytes::from_static(&[0x7b, 0xdd, 0xee, 0xff]),
        ];
        let header = Header {
            number: 42,
            transactions_root: ordered_trie_root_encoded(&txs),
            ..default_header()
        };
        check_transactions_bind_to_header(&header, &txs).expect("the real body must be accepted");

        // Truncated body — the shape a silently empty or partial `debug_getRawTransactions` takes.
        let err = check_transactions_bind_to_header(&header, &txs[..2])
            .expect_err("a truncated body must be rejected")
            .to_string();
        assert!(err.contains("block 42"), "{err}");
        assert!(err.contains("2 stored transactions"), "{err}");

        // Reordered body: same transactions, different root.
        let swapped = vec![txs[1].clone(), txs[0].clone(), txs[2].clone()];
        assert!(check_transactions_bind_to_header(&header, &swapped).is_err());

        // A single flipped byte in one encoding.
        let mut mangled = txs;
        mangled[2] = Bytes::from_static(&[0x7b, 0xdd, 0xee, 0x00]);
        assert!(check_transactions_bind_to_header(&header, &mangled).is_err());
    }

    /// An empty block body must hash to the empty trie root, not be waved through.
    #[test]
    fn test_empty_body_must_match_the_empty_root() {
        let empty_root = ordered_trie_root_encoded::<Bytes>(&[]);
        let header = Header { number: 1, transactions_root: empty_root, ..default_header() };
        check_transactions_bind_to_header(&header, &[]).expect("an empty body is legal");

        let non_empty = Header {
            number: 1,
            transactions_root: b256!(
                "0x3333333333333333333333333333333333333333333333333333333333333333"
            ),
            ..default_header()
        };
        assert!(check_transactions_bind_to_header(&non_empty, &[]).is_err());
    }

    /// A header with nothing set, for tests that only care about a couple of fields.
    fn default_header() -> Header {
        Header::default()
    }
}
