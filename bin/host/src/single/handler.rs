//! [HintHandler] for the [CeloSingleChainHost].

use crate::{backend::util::store_ordered_trie, single::CeloSingleChainHost};
use alloy_consensus::Header;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_provider::Provider;
use alloy_rlp::Decodable;
use alloy_rpc_types::{Block, debug::ExecutionWitness};
use alloy_transport::{RpcError, TransportErrorKind};
use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use backon::{ExponentialBuilder, Retryable};
#[cfg(feature = "eigenda")]
use hokulea_host_bin::handler::fetch_eigenda_hint;
#[cfg(feature = "eigenda")]
use hokulea_proof::hint::ExtendedHintType;
use kona_host::{HintHandler, OnlineHostBackendCfg, SharedKeyValueStore};
use kona_preimage::{PreimageKey, PreimageKeyType};
use kona_proof::{Hint, HintType};
use kona_protocol::{OutputRoot, Predeploys};
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use std::time::Duration;
use tracing::{info, warn};

/// Returns `true` if the RPC error indicates the node does not support the requested method
/// (JSON-RPC error code -32601: Method not found).
const fn is_rpc_method_not_found(e: &RpcError<TransportErrorKind>) -> bool {
    matches!(e, RpcError::ErrorResp(p) if p.code == -32601)
}

/// Backoff schedule for retrying a hint fetch when its underlying RPC call fails transiently.
///
/// kona's preimage server (`OnlineHostBackend::get_preimage`) retries a failed `fetch_hint`
/// immediately with no delay, so a flaky RPC endpoint makes it busy-spin on the endpoint and
/// flood the logs with `Failed to prefetch hint`. Retrying here with exponential backoff turns
/// that into a bounded, quiet retry that recovers on its own: 100 ms, doubling each attempt,
/// capped at 2 minutes, up to 20 times (~21 min of tolerance for a sustained outage).
fn hint_retry_policy() -> ExponentialBuilder {
    ExponentialBuilder::default()
        .with_min_delay(Duration::from_millis(100))
        .with_factor(2.0)
        .with_max_delay(Duration::from_secs(120))
        .with_max_times(20)
}

/// Retry only genuinely transient failures on the L1/L2 RPC path, classified by error kind
/// rather than the outer variant:
/// - transport infra failures — dropped connection/timeout (`Custom`), `BackendGone`, missing batch
///   response — are retried;
/// - HTTP responses are retried only for `429` and `5xx`; deterministic `4xx` (auth, not-found,
///   bad-request) are terminal;
/// - a JSON-RPC `ErrorResp` is retried only when it is itself a rate-limit (some providers return
///   `429`/request-limit that way), via alloy's own `is_retry_err`;
/// - everything else (method-not-found, (de)serialize, unsupported-feature, `NonRetryable`) is
///   terminal, so it surfaces immediately instead of burning the whole backoff budget.
fn is_retryable_rpc(rpc_err: &RpcError<TransportErrorKind>) -> bool {
    match rpc_err {
        RpcError::Transport(kind) => match kind {
            TransportErrorKind::MissingBatchResponse(_) |
            TransportErrorKind::BackendGone |
            TransportErrorKind::Custom(_) => true,
            TransportErrorKind::HttpError(http) => http.is_rate_limit_err() || http.status >= 500,
            _ => false,
        },
        RpcError::ErrorResp(payload) => payload.is_retry_err(),
        _ => false,
    }
}

fn is_retryable_transport_err(err: &anyhow::Error) -> bool {
    err.downcast_ref::<RpcError<TransportErrorKind>>().is_some_and(is_retryable_rpc)
}

/// Retry only the transient EigenDA proxy-request failures. hokulea surfaces these as opaque
/// `anyhow` strings (no typed error to match on), so match the transient ones by message:
/// - the request send error and non-teapot `5xx` ("failed to fetch eigenda encoded payload…");
/// - a reset/timeout while reading the success-response body ("should be able to get encoded
///   payload from http response…").
///
/// Deterministic errors (commitment parse, 418-body decode, bad status handling, KV write)
/// carry other messages and stay terminal, so they don't re-hit the proxy for the whole backoff
/// budget. Coupled to hokulea's wording (pinned revision); tracked for a typed-error fix in
/// celo-blockchain-planning#1430.
#[cfg(feature = "eigenda")]
fn is_retryable_eigenda_err(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    msg.contains("failed to fetch eigenda encoded payload") ||
        msg.contains("should be able to get encoded payload from http response")
}

/// Logs a hint-fetch retry with the delay before the next attempt.
fn notify_hint_retry(err: &anyhow::Error, backoff: Duration) {
    warn!(target: "single_hint_handler", %err, ?backoff, "transient failure fetching hint; retrying");
}

/// The [HintHandler] for the [CeloSingleChainHost].
#[derive(Debug, Clone, Copy)]
pub struct CeloSingleChainHintHandler;

#[cfg(feature = "eigenda")]
#[async_trait]
impl HintHandler for CeloSingleChainHintHandler {
    type Cfg = CeloSingleChainHost;

    /// fetch_hint fetches and processes a hint based on its type.
    async fn fetch_hint(
        hint: Hint<<Self::Cfg as OnlineHostBackendCfg>::HintType>,
        cfg: &Self::Cfg,
        providers: &<Self::Cfg as OnlineHostBackendCfg>::Providers,
        kv: SharedKeyValueStore,
    ) -> Result<()> {
        match &hint.ty {
            ExtendedHintType::Original(ty) => {
                let ty = *ty;
                // L1/L2 RPC path: retry only genuine transport failures, not deterministic
                // JSON-RPC errors.
                (|| {
                    Self::fetch_original_hint(
                        Hint { ty, data: hint.data.clone() },
                        cfg,
                        providers,
                        kv.clone(),
                    )
                })
                .retry(hint_retry_policy())
                .when(is_retryable_transport_err)
                .notify(notify_hint_retry)
                .await
            }
            ExtendedHintType::EigenDACert => {
                let eigenda_provider = providers
                    .eigenda_preimage_provider
                    .as_ref()
                    .ok_or(anyhow!("Eigen DA blob provider must be set"))?;
                // Back off transient EigenDA proxy-request failures (see is_retryable_eigenda_err)
                // rather than falling through to kona's immediate spin loop; deterministic errors
                // surface immediately.
                (|| fetch_eigenda_hint(hint.data.clone(), eigenda_provider, kv.clone()))
                    .retry(hint_retry_policy())
                    .when(is_retryable_eigenda_err)
                    .notify(notify_hint_retry)
                    .await
            }
        }
    }
}

#[cfg(not(feature = "eigenda"))]
#[async_trait]
impl HintHandler for CeloSingleChainHintHandler {
    type Cfg = CeloSingleChainHost;

    async fn fetch_hint(
        hint: Hint<<Self::Cfg as OnlineHostBackendCfg>::HintType>,
        cfg: &Self::Cfg,
        providers: &<Self::Cfg as OnlineHostBackendCfg>::Providers,
        kv: SharedKeyValueStore,
    ) -> Result<()> {
        (|| {
            Self::fetch_original_hint(
                Hint { ty: hint.ty, data: hint.data.clone() },
                cfg,
                providers,
                kv.clone(),
            )
        })
        .retry(hint_retry_policy())
        .when(is_retryable_transport_err)
        .notify(notify_hint_retry)
        .await
    }
}

/// Implements the fetchers for each hint type.
impl CeloSingleChainHintHandler {
    /// fetch_original_hint fetches and processes an original hint.
    async fn fetch_original_hint(
        hint: Hint<HintType>,
        cfg: &<Self as HintHandler>::Cfg,
        providers: &<<Self as HintHandler>::Cfg as OnlineHostBackendCfg>::Providers,
        kv: SharedKeyValueStore,
    ) -> Result<()> {
        match hint.ty {
            HintType::L1BlockHeader => {
                ensure!(hint.data.len() == 32, "Invalid hint data length");

                let hash: B256 = hint.data.as_ref().try_into()?;
                let raw_header: Bytes =
                    match providers.l1.client().request("debug_getRawHeader", [hash]).await {
                        Ok(raw_header) => raw_header,
                        // Not every L1 endpoint serves geth's raw-debug API (anvil does not);
                        // re-encode the header from `eth_getBlockByHash` instead. The hash check
                        // below rejects a bad re-encoding.
                        Err(e) if is_rpc_method_not_found(&e) => {
                            let block = providers
                                .l1
                                .get_block_by_hash(hash)
                                .await?
                                .ok_or(anyhow!("Block not found"))?;
                            alloy_rlp::encode(block.header.into_consensus()).into()
                        }
                        Err(e) => return Err(e.into()),
                    };
                ensure!(
                    keccak256(&raw_header) == hash,
                    "L1 header preimage does not match its hash"
                );

                let mut kv_lock = kv.write().await;
                kv_lock.set(PreimageKey::new_keccak256(*hash).into(), raw_header.into())?;
            }
            HintType::L1Transactions => {
                ensure!(hint.data.len() == 32, "Invalid hint data length");

                let hash: B256 = hint.data.as_ref().try_into()?;
                let Block { transactions, .. } = providers
                    .l1
                    .get_block_by_hash(hash)
                    .full()
                    .await?
                    .ok_or(anyhow!("Block not found"))?;
                let encoded_transactions = transactions
                    .into_transactions()
                    .map(|tx| tx.inner.encoded_2718())
                    .collect::<Vec<_>>();

                store_ordered_trie(kv.as_ref(), encoded_transactions.as_slice()).await?;
            }
            HintType::L1Receipts => {
                ensure!(hint.data.len() == 32, "Invalid hint data length");

                let hash: B256 = hint.data.as_ref().try_into()?;
                let raw_receipts: Vec<Bytes> =
                    match providers.l1.client().request("debug_getRawReceipts", [hash]).await {
                        Ok(raw_receipts) => raw_receipts,
                        // Same fallback as `L1BlockHeader` above; a bad re-encoding surfaces in
                        // the client as a receipts-root mismatch against the verified header.
                        Err(e) if is_rpc_method_not_found(&e) => providers
                            .l1
                            .get_block_receipts(hash.into())
                            .await?
                            .ok_or(anyhow!("Block receipts not found"))?
                            .into_iter()
                            .map(|receipt| {
                                receipt.into_primitives_receipt().inner.encoded_2718().into()
                            })
                            .collect(),
                        Err(e) => return Err(e.into()),
                    };

                store_ordered_trie(kv.as_ref(), raw_receipts.as_slice()).await?;
            }
            HintType::L1Blob => {
                // Celo uses EigenDA (or calldata) for data availability, not EIP-4844
                // blob transactions, so this hint type should never be sent.
                anyhow::bail!("Celo does not use EIP-4844 blob transactions");
            }
            HintType::L1Precompile => {
                ensure!(hint.data.len() >= 28, "Invalid hint data length");

                let address = Address::from_slice(&hint.data.as_ref()[..20]);
                let gas = u64::from_be_bytes(hint.data.as_ref()[20..28].try_into()?);
                let input = hint.data[28..].to_vec();
                let input_hash = keccak256(hint.data.as_ref());

                let result = crate::eth::execute(address, input, gas).map_or_else(
                    |_| vec![0u8; 1],
                    |raw_res| {
                        let mut res = Vec::with_capacity(1 + raw_res.len());
                        res.push(0x01);
                        res.extend_from_slice(&raw_res);
                        res
                    },
                );

                let mut kv_lock = kv.write().await;
                kv_lock.set(PreimageKey::new_keccak256(*input_hash).into(), hint.data.into())?;
                kv_lock.set(
                    PreimageKey::new(*input_hash, PreimageKeyType::Precompile).into(),
                    result,
                )?;
            }
            HintType::L2BlockHeader => {
                ensure!(hint.data.len() == 32, "Invalid hint data length");

                // Fetch the raw header from the L2 chain provider.
                let hash: B256 = hint.data.as_ref().try_into()?;
                let raw_header: Bytes =
                    providers.l2.client().request("debug_getRawHeader", [hash]).await?;

                // Acquire a lock on the key-value store and set the preimage.
                let mut kv_lock = kv.write().await;
                kv_lock.set(PreimageKey::new_keccak256(*hash).into(), raw_header.into())?;
            }
            HintType::L2Transactions => {
                ensure!(hint.data.len() == 32, "Invalid hint data length");

                let hash: B256 = hint.data.as_ref().try_into()?;
                let Block { transactions, .. } = providers
                    .l2
                    .get_block_by_hash(hash)
                    .full()
                    .await?
                    .ok_or(anyhow!("Block not found."))?;

                let encoded_transactions = transactions
                    .into_transactions()
                    .map(|tx| tx.inner.inner.encoded_2718())
                    .collect::<Vec<_>>();
                store_ordered_trie(kv.as_ref(), encoded_transactions.as_slice()).await?;
            }
            HintType::StartingL2Output => {
                ensure!(hint.data.len() == 32, "Invalid hint data length");

                // Fetch the header for the L2 head block.
                let raw_header: Bytes = providers
                    .l2
                    .client()
                    .request("debug_getRawHeader", &[cfg.kona_cfg.agreed_l2_head_hash])
                    .await?;
                let header = Header::decode(&mut raw_header.as_ref())?;

                // Fetch the storage root for the L2 head block.
                let l2_to_l1_message_passer = providers
                    .l2
                    .get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, Default::default())
                    .block_id(cfg.kona_cfg.agreed_l2_head_hash.into())
                    .await?;

                let output_root = OutputRoot::from_parts(
                    header.state_root,
                    l2_to_l1_message_passer.storage_hash,
                    cfg.kona_cfg.agreed_l2_head_hash,
                );
                let output_root_hash = output_root.hash();

                ensure!(
                    output_root_hash == cfg.kona_cfg.agreed_l2_output_root,
                    "Output root does not match L2 head."
                );

                let mut kv_write_lock = kv.write().await;
                kv_write_lock.set(
                    PreimageKey::new_keccak256(*output_root_hash).into(),
                    output_root.encode().into(),
                )?;
            }
            HintType::L2Code => {
                // geth hashdb scheme code hash key prefix
                const CODE_PREFIX: u8 = b'c';

                ensure!(hint.data.len() == 32, "Invalid hint data length");

                let hash: B256 = hint.data.as_ref().try_into()?;

                // Attempt to fetch the code from the L2 chain provider.
                let code_key = [&[CODE_PREFIX], hash.as_slice()].concat();
                let code = providers
                    .l2
                    .client()
                    .request::<&[Bytes; 1], Bytes>("debug_dbGet", &[code_key.into()])
                    .await;

                // Check if the first attempt to fetch the code failed. If it did, try fetching the
                // code hash preimage without the geth hashdb scheme prefix.
                let code = match code {
                    Ok(code) => code,
                    // A transient failure on the primary lookup must back off — propagate it
                    // (still downcastable) instead of masking it with the fallback, whose
                    // deterministic miss would otherwise surface as terminal and spin in kona. A
                    // normal "wrong hashdb scheme" miss is a non-retryable `ErrorResp`, so it still
                    // falls through to the fallback.
                    Err(e) if is_retryable_rpc(&e) => return Err(anyhow::Error::new(e)),
                    // Preserve the fallback's transport error (via `context`, not a stringified
                    // `anyhow!`) so a transient failure there stays downcastable and is retried.
                    Err(_) => providers
                        .l2
                        .client()
                        .request::<&[B256; 1], Bytes>("debug_dbGet", &[hash])
                        .await
                        .context("Error fetching code hash preimage")?,
                };

                let mut kv_lock = kv.write().await;
                kv_lock.set(PreimageKey::new_keccak256(*hash).into(), code.into())?;
            }
            HintType::L2StateNode => {
                ensure!(hint.data.len() == 32, "Invalid hint data length");

                let hash: B256 = hint.data.as_ref().try_into()?;

                warn!(target: "single_hint_handler", "L2StateNode hint was sent for node hash: {}", hash);
                warn!(
                    target: "single_hint_handler",
                    "`debug_executePayload` failed to return a complete witness."
                );

                // Fetch the preimage from the L2 chain provider.
                let preimage: Bytes = providers.l2.client().request("debug_dbGet", &[hash]).await?;

                let mut kv_write_lock = kv.write().await;
                kv_write_lock.set(PreimageKey::new_keccak256(*hash).into(), preimage.into())?;
            }
            HintType::L2AccountProof => {
                // Backwards compatibility: old prestates send an 8-byte block number; new
                // prestates send a 32-byte block hash. Mirrors kona-host v1.5.0.
                const BLOCK_NUMBER_HINT_LEN: usize = 8 + 20;
                const BLOCK_HASH_HINT_LEN: usize = 32 + 20;
                let (block_id, address) = match hint.data.len() {
                    BLOCK_NUMBER_HINT_LEN => {
                        let block_number = u64::from_be_bytes(hint.data.as_ref()[..8].try_into()?);
                        let address = Address::from_slice(&hint.data.as_ref()[8..28]);
                        (block_number.into(), address)
                    }
                    BLOCK_HASH_HINT_LEN => {
                        let block_hash = B256::from_slice(&hint.data.as_ref()[..32]);
                        let address = Address::from_slice(&hint.data.as_ref()[32..52]);
                        (block_hash.into(), address)
                    }
                    other => anyhow::bail!(
                        "Invalid L2AccountProof hint length: expected {BLOCK_NUMBER_HINT_LEN} or {BLOCK_HASH_HINT_LEN}, got {other}"
                    ),
                };

                let proof_response =
                    providers.l2.get_proof(address, Default::default()).block_id(block_id).await?;

                // Write the account proof nodes to the key-value store.
                let mut kv_lock = kv.write().await;
                proof_response.account_proof.into_iter().try_for_each(|node| {
                    let node_hash = keccak256(node.as_ref());
                    let key = PreimageKey::new_keccak256(*node_hash);
                    kv_lock.set(key.into(), node.into())?;
                    Ok::<(), anyhow::Error>(())
                })?;
            }
            HintType::L2AccountStorageProof => {
                // Backwards compatibility: old prestates send an 8-byte block number; new
                // prestates send a 32-byte block hash. Mirrors kona-host v1.5.0.
                const BLOCK_NUMBER_HINT_LEN: usize = 8 + 20 + 32;
                const BLOCK_HASH_HINT_LEN: usize = 32 + 20 + 32;
                let (block_id, address, slot) = match hint.data.len() {
                    BLOCK_NUMBER_HINT_LEN => {
                        let block_number = u64::from_be_bytes(hint.data.as_ref()[..8].try_into()?);
                        let address = Address::from_slice(&hint.data.as_ref()[8..28]);
                        let slot = B256::from_slice(&hint.data.as_ref()[28..60]);
                        (block_number.into(), address, slot)
                    }
                    BLOCK_HASH_HINT_LEN => {
                        let block_hash = B256::from_slice(&hint.data.as_ref()[..32]);
                        let address = Address::from_slice(&hint.data.as_ref()[32..52]);
                        let slot = B256::from_slice(&hint.data.as_ref()[52..84]);
                        (block_hash.into(), address, slot)
                    }
                    other => anyhow::bail!(
                        "Invalid L2AccountStorageProof hint length: expected {BLOCK_NUMBER_HINT_LEN} or {BLOCK_HASH_HINT_LEN}, got {other}"
                    ),
                };

                let mut proof_response =
                    providers.l2.get_proof(address, vec![slot]).block_id(block_id).await?;

                let mut kv_lock = kv.write().await;

                // Write the account proof nodes to the key-value store.
                proof_response.account_proof.into_iter().try_for_each(|node| {
                    let node_hash = keccak256(node.as_ref());
                    let key = PreimageKey::new_keccak256(*node_hash);
                    kv_lock.set(key.into(), node.into())?;
                    Ok::<(), anyhow::Error>(())
                })?;

                // Write the storage proof nodes to the key-value store.
                let storage_proof = proof_response.storage_proof.remove(0);
                storage_proof.proof.into_iter().try_for_each(|node| {
                    let node_hash = keccak256(node.as_ref());
                    let key = PreimageKey::new_keccak256(*node_hash);
                    kv_lock.set(key.into(), node.into())?;
                    Ok::<(), anyhow::Error>(())
                })?;
            }
            HintType::L2PayloadWitness => {
                ensure!(hint.data.len() >= 32, "Invalid hint data length");

                let parent_block_hash = B256::from_slice(&hint.data.as_ref()[..32]);
                let payload_attributes: OpPayloadAttributes =
                    serde_json::from_slice(&hint.data[32..])?;

                let execute_payload_response = match providers
                    .l2
                    .client()
                    .request::<(B256, OpPayloadAttributes), ExecutionWitness>(
                        "debug_executePayload",
                        (parent_block_hash, payload_attributes),
                    )
                    .await
                {
                    Ok(response) => response,
                    Err(e) => {
                        info!(
                            target: "single_hint_handler",
                            err = %e,
                            method_not_found = is_rpc_method_not_found(&e),
                            "debug_executePayload unavailable, skipping witness preimage collection"
                        );
                        return Ok(());
                    }
                };

                let preimages = execute_payload_response
                    .state
                    .into_iter()
                    .chain(execute_payload_response.codes)
                    .chain(execute_payload_response.keys);

                let mut kv_lock = kv.write().await;
                for preimage in preimages {
                    let computed_hash = keccak256(preimage.as_ref());

                    let key = PreimageKey::new_keccak256(*computed_hash);
                    kv_lock.set(key.into(), preimage.into())?;
                }
            }
        }

        Ok(())
    }
}
