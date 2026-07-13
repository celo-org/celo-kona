//! Utilities for the preimage server backend.
//! Identical to kona-host, but had to duplicate it because the original is private.

use alloy_consensus::EMPTY_ROOT_HASH;
use alloy_primitives::{B256, keccak256};
use alloy_rlp::EMPTY_STRING_CODE;
use alloy_rpc_client::ClientRef;
use alloy_rpc_types::debug::ExecutionWitness;
use anyhow::Result;
use kona_host::KeyValueStore;
use kona_preimage::{PreimageKey, PreimageKeyType};
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use tokio::sync::RwLock;

/// Fetches a block's execution witness via `debug_executePayload`.
///
/// Any failure, including "method not found", is returned as an error rather than swallowed. The
/// underlying RPC error remains downcastable so Celo's hint handler can apply its short transient
/// retry before kona-host falls back to a fine-grained hint.
pub(crate) async fn fetch_execution_witness(
    client: ClientRef<'_>,
    parent_block_hash: B256,
    payload_attributes: OpPayloadAttributes,
) -> anyhow::Result<ExecutionWitness> {
    client
        .request::<(B256, OpPayloadAttributes), ExecutionWitness>(
            "debug_executePayload",
            (parent_block_hash, payload_attributes),
        )
        .await
        .map_err(|error| {
            let context = format!("debug_executePayload failed: {error}");
            anyhow::Error::new(error).context(context)
        })
}

/// Constructs a merkle patricia trie from the ordered list passed and stores all encoded
/// intermediate nodes of the trie in the [KeyValueStore].
#[allow(dead_code)]
pub(crate) async fn store_ordered_trie<KV: KeyValueStore + ?Sized, T: AsRef<[u8]>>(
    kv: &RwLock<KV>,
    values: &[T],
) -> Result<()> {
    let mut kv_write_lock = kv.write().await;

    // If the list of nodes is empty, store the empty root hash and exit early.
    // The `HashBuilder` will not push the preimage of the empty root hash to the
    // `ProofRetainer` in the event that there are no leaves inserted.
    if values.is_empty() {
        let empty_key = PreimageKey::new(*EMPTY_ROOT_HASH, PreimageKeyType::Keccak256);
        kv_write_lock.set(empty_key.into(), [EMPTY_STRING_CODE].into())?;
        return Ok(());
    }

    let mut hb = kona_mpt::ordered_trie_with_encoder(values, |node, buf| {
        buf.put_slice(node.as_ref());
    });
    hb.root();
    let intermediates = hb.take_proof_nodes().into_inner();

    for (_, value) in intermediates.into_iter() {
        let value_hash = keccak256(value.as_ref());
        let key = PreimageKey::new(*value_hash, PreimageKeyType::Keccak256);

        kv_write_lock.set(key.into(), value.into())?;
    }

    Ok(())
}
