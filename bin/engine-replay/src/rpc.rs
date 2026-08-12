//! JSON-RPC transports and the handful of `eth_*` / `debug_*` calls the rig makes.
//!
//! Two endpoints are involved and they are deliberately kept separate:
//!
//! * the **public** endpoint (`--rpc-url`, needs `--http.api eth,debug`) is used only by `archive`,
//!   to read canonical blocks out of a synced node;
//! * the **authenticated engine** endpoint (`--engine-url` + `--jwt`) is used by `replay`. reth
//!   serves a small `eth_*` subset there too, so the replay path needs no public RPC at all — one
//!   fewer flag to get wrong, and no reason to expose HTTP on a shaped node.

use alloy_primitives::{B256, Bytes};
use anyhow::{Context, bail};
use jsonrpsee::{core::client::ClientT, http_client::HttpClientBuilder, rpc_params};
use reth_rpc_layer::{AuthClientLayer, JwtSecret};
use serde::Deserialize;
use std::{path::Path, time::Duration};

/// The subset of `eth_getBlockByNumber`'s reply the rig reads.
///
/// Deserialised structurally rather than via `alloy-rpc-types-eth` so the driver does not have
/// to model Celo's RPC block shape (CIP-64 receipts, fee-currency fields) at all.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockHead {
    /// Sealed block hash.
    hash: B256,
    /// Block height, as a hex quantity.
    #[serde(with = "hex_u64")]
    number: u64,
    /// Parent's sealed block hash.
    parent_hash: B256,
}

/// Serde helper for JSON-RPC hex quantities, so the driver does not need `alloy-serde`.
mod hex_u64 {
    use serde::{Deserialize, Deserializer};

    /// Deserialize a `0x`-prefixed hex quantity into a `u64`.
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        let raw = String::deserialize(d)?;
        let digits = raw.strip_prefix("0x").unwrap_or(&raw);
        u64::from_str_radix(digits, 16).map_err(serde::de::Error::custom)
    }
}

/// Build a plain (unauthenticated) HTTP JSON-RPC client.
pub(crate) fn plain_client(
    url: &str,
    timeout: Duration,
) -> anyhow::Result<impl ClientT + Send + Sync + 'static> {
    HttpClientBuilder::default()
        .request_timeout(timeout)
        .build(url)
        .with_context(|| format!("failed to build an RPC client for {url}"))
}

/// Build an HTTP JSON-RPC client that signs every request with the engine API's JWT secret.
///
/// The token carries an `iat` claim and is only valid for about a minute, so it is re-minted per
/// request by the layer rather than pinned into a default header. This is the same construction
/// reth's own `AuthServerHandle::http_client` uses.
pub(crate) fn engine_client(
    url: &str,
    jwt_path: &Path,
    timeout: Duration,
) -> anyhow::Result<impl ClientT + Send + Sync + 'static> {
    let secret = JwtSecret::from_file(jwt_path).with_context(|| {
        format!(
            "failed to read the engine JWT secret from {}; reth writes it to <datadir>/jwt.hex \
             unless --authrpc.jwtsecret says otherwise",
            jwt_path.display(),
        )
    })?;
    let middleware = tower::ServiceBuilder::default().layer(AuthClientLayer::new(secret));
    HttpClientBuilder::default()
        .set_http_middleware(middleware)
        .request_timeout(timeout)
        .build(url)
        .with_context(|| format!("failed to build an engine API client for {url}"))
}

/// `eth_chainId`.
///
/// reth types the reply as `Option<U64>` on both the public and the auth endpoint, so a `null`
/// is a wire-legal answer that has to be rejected explicitly.
pub(crate) async fn chain_id<C: ClientT + Sync>(client: &C) -> anyhow::Result<u64> {
    let raw: Option<String> =
        client.request("eth_chainId", rpc_params![]).await.context("eth_chainId")?;
    let raw = raw.context("node answered eth_chainId with null")?;
    let digits = raw.strip_prefix("0x").unwrap_or(&raw);
    u64::from_str_radix(digits, 16).with_context(|| format!("malformed eth_chainId reply {raw:?}"))
}

/// The node's current canonical head: `(number, hash, parent_hash)`.
pub(crate) async fn canonical_head<C: ClientT + Sync>(
    client: &C,
) -> anyhow::Result<(u64, B256, B256)> {
    let head: Option<BlockHead> = client
        .request("eth_getBlockByNumber", rpc_params!["latest", false])
        .await
        .context("eth_getBlockByNumber(latest)")?;
    let head = head.context("node reported no latest block")?;
    Ok((head.number, head.hash, head.parent_hash))
}

/// The sealed hash of canonical block `number`.
pub(crate) async fn block_hash<C: ClientT + Sync>(
    client: &C,
    number: u64,
) -> anyhow::Result<(B256, B256)> {
    let block: Option<BlockHead> = client
        .request("eth_getBlockByNumber", rpc_params![quantity(number), false])
        .await
        .with_context(|| format!("eth_getBlockByNumber({number})"))?;
    let block = block.with_context(|| format!("node does not have canonical block {number}"))?;
    if block.number != number {
        bail!("asked for block {number} but the node answered with block {}", block.number);
    }
    Ok((block.hash, block.parent_hash))
}

/// `debug_getRawHeader`: the RLP-encoded consensus header of canonical block `number`.
pub(crate) async fn raw_header<C: ClientT + Sync>(
    client: &C,
    number: u64,
) -> anyhow::Result<Bytes> {
    client
        .request("debug_getRawHeader", rpc_params![quantity(number)])
        .await
        .with_context(|| format!("debug_getRawHeader({number}); is --http.api debug enabled?"))
}

/// `debug_getRawTransactions`: the block's EIP-2718-encoded transactions, in order.
///
/// Taking them from the node in encoded form is what makes CIP-64 (type `0x7b`) transactions
/// replay correctly: the driver never re-encodes, so it cannot introduce a non-canonical
/// encoding that would surface later as a `transactionsRoot` mismatch.
pub(crate) async fn raw_transactions<C: ClientT + Sync>(
    client: &C,
    number: u64,
) -> anyhow::Result<Vec<Bytes>> {
    client.request("debug_getRawTransactions", rpc_params![quantity(number)]).await.with_context(
        || format!("debug_getRawTransactions({number}); is --http.api debug enabled?"),
    )
}

/// Render a block number as the JSON-RPC hex quantity a `BlockId` parameter expects.
fn quantity(number: u64) -> String {
    format!("{number:#x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quantity_is_a_hex_block_id() {
        assert_eq!(quantity(0), "0x0");
        assert_eq!(quantity(4_660), "0x1234");
    }

    #[test]
    fn test_block_head_deserialises_from_a_full_rpc_block() {
        // Extra fields (Celo's fee-currency additions included) must be ignored, not rejected.
        let json = r#"{
            "hash": "0x1111111111111111111111111111111111111111111111111111111111111111",
            "number": "0x1f4",
            "parentHash": "0x2222222222222222222222222222222222222222222222222222222222222222",
            "gasUsed": "0x5208",
            "someFutureField": true
        }"#;
        let head: BlockHead = serde_json::from_str(json).unwrap();
        assert_eq!(head.number, 500);
        assert_eq!(head.hash.0[0], 0x11);
        assert_eq!(head.parent_hash.0[0], 0x22);
    }
}
