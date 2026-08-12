//! Typed wrappers over the OP engine API methods the replay loop drives.
//!
//! Params are hand-built rather than going through a generated `#[rpc(client)]` trait: the trait
//! is generic over `EngineTypes`, which would drag `reth-optimism-node` into a crate that only
//! needs to speak JSON. The wire shapes here are pinned against op-reth's `OpEngineApi`
//! declarations (`rust/op-reth/crates/rpc/src/engine.rs`).

use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_engine::{
    ExecutionPayloadV1, ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadId,
    PayloadStatus,
};
use anyhow::{Context, bail};
use jsonrpsee::{core::client::ClientT, rpc_params};
use op_alloy_rpc_types_engine::{
    OpExecutionPayloadEnvelopeV3, OpExecutionPayloadEnvelopeV4, OpExecutionPayloadV4,
    OpPayloadAttributes,
};

use crate::attrs::EngineVersion;

/// A payload the node just built, in the shape `engine_newPayload` wants it back.
#[derive(Debug, Clone)]
pub(crate) enum BuiltPayload {
    /// Ecotone through Holocene.
    V3 {
        /// The payload as returned by `engine_getPayloadV3`.
        payload: Box<ExecutionPayloadV3>,
        /// Echoed back to `engine_newPayloadV3` as its third parameter.
        parent_beacon_block_root: B256,
    },
    /// Isthmus onwards, carrying the L2 withdrawals root.
    V4 {
        /// The payload as returned by `engine_getPayloadV4`.
        payload: Box<OpExecutionPayloadV4>,
        /// Echoed back to `engine_newPayloadV4` as its third parameter.
        parent_beacon_block_root: B256,
    },
}

impl BuiltPayload {
    /// The innermost V1 payload, where the sealed header fields live.
    const fn v1(&self) -> &ExecutionPayloadV1 {
        match self {
            Self::V3 { payload, .. } => &payload.payload_inner.payload_inner,
            Self::V4 { payload, .. } => &payload.payload_inner.payload_inner.payload_inner,
        }
    }

    /// Hash of the block the node sealed.
    pub(crate) const fn block_hash(&self) -> B256 {
        self.v1().block_hash
    }

    /// State root the node computed. The quantity this whole rig exists to time.
    pub(crate) const fn state_root(&self) -> B256 {
        self.v1().state_root
    }

    /// Receipts root the node computed.
    pub(crate) const fn receipts_root(&self) -> B256 {
        self.v1().receipts_root
    }

    /// Gas the node's block consumed.
    pub(crate) const fn gas_used(&self) -> u64 {
        self.v1().gas_used
    }

    /// Number of transactions the node actually included.
    ///
    /// Lower than the supplied count means the builder skipped a sequencer transaction that
    /// failed validation — a harness problem, not a state-root bug.
    pub(crate) const fn tx_count(&self) -> usize {
        self.v1().transactions.len()
    }
}

/// `engine_forkchoiceUpdatedV3`.
///
/// With `attributes` set this is what *starts* the build: op-reth kicks the job off inside the
/// FCU handler, so the returned time is the enqueue cost, not the build cost.
pub(crate) async fn forkchoice_updated_v3<C: ClientT + Sync>(
    client: &C,
    state: ForkchoiceState,
    attributes: Option<OpPayloadAttributes>,
) -> anyhow::Result<ForkchoiceUpdated> {
    let updated: ForkchoiceUpdated = client
        .request("engine_forkchoiceUpdatedV3", rpc_params![state, attributes])
        .await
        .context("engine_forkchoiceUpdatedV3")?;
    if !updated.payload_status.is_valid() {
        bail!(
            "engine_forkchoiceUpdatedV3 for head {} returned {:?}",
            state.head_block_hash,
            updated.payload_status.status,
        );
    }
    Ok(updated)
}

/// `engine_getPayloadV3` / `engine_getPayloadV4`, selected per block.
///
/// This is where the build latency shows up: the call awaits the job started by the preceding
/// forkchoice update rather than polling, so no sleep belongs between the two (a sleep would
/// subtract directly from the measurement).
pub(crate) async fn get_payload<C: ClientT + Sync>(
    client: &C,
    version: EngineVersion,
    id: PayloadId,
) -> anyhow::Result<BuiltPayload> {
    match version {
        EngineVersion::V3 => {
            let envelope: OpExecutionPayloadEnvelopeV3 = client
                .request("engine_getPayloadV3", rpc_params![id])
                .await
                .context("engine_getPayloadV3")?;
            Ok(BuiltPayload::V3 {
                payload: Box::new(envelope.execution_payload),
                parent_beacon_block_root: envelope.parent_beacon_block_root,
            })
        }
        EngineVersion::V4 => {
            let envelope: OpExecutionPayloadEnvelopeV4 = client
                .request("engine_getPayloadV4", rpc_params![id])
                .await
                .context("engine_getPayloadV4")?;
            Ok(BuiltPayload::V4 {
                payload: Box::new(envelope.execution_payload),
                parent_beacon_block_root: envelope.parent_beacon_block_root,
            })
        }
    }
}

/// `engine_newPayloadV3` / `engine_newPayloadV4`.
///
/// For a locally built block this is cheap — the block is already in the engine's tree, so the
/// call short-circuits instead of re-executing. It is issued anyway because it is what makes the
/// block a forkchoice-eligible candidate, and because it is the shape a real sequencer's peers
/// take.
pub(crate) async fn new_payload<C: ClientT + Sync>(
    client: &C,
    payload: &BuiltPayload,
) -> anyhow::Result<PayloadStatus> {
    // OP requires both of these to be empty; they are parameters only because the Ethereum
    // method shapes have them.
    let versioned_hashes: Vec<B256> = Vec::new();
    let execution_requests: Vec<Bytes> = Vec::new();

    let status: PayloadStatus = match payload {
        BuiltPayload::V3 { payload, parent_beacon_block_root } => client
            .request(
                "engine_newPayloadV3",
                rpc_params![payload, versioned_hashes, parent_beacon_block_root],
            )
            .await
            .context("engine_newPayloadV3")?,
        BuiltPayload::V4 { payload, parent_beacon_block_root } => client
            .request(
                "engine_newPayloadV4",
                rpc_params![
                    payload,
                    versioned_hashes,
                    parent_beacon_block_root,
                    execution_requests
                ],
            )
            .await
            .context("engine_newPayloadV4")?,
    };
    if !status.is_valid() {
        bail!("engine_newPayload for {} returned {:?}", payload.block_hash(), status.status);
    }
    Ok(status)
}
