//! Deriving replay inputs from a sealed canonical header.
//!
//! The driver reads **no chain configuration at all**. Every input the engine API needs is
//! recoverable from the canonical header itself:
//!
//! * `extraData` *is* the canonical encoding of the Holocene/Jovian EIP-1559 parameters, and its
//!   length plus version byte identify which fork produced it.
//! * `requestsHash` is set exactly from Isthmus onwards, which is what selects
//!   `engine_getPayloadV4` over `engine_getPayloadV3`.
//! * `withdrawalsRoot` is set exactly from Canyon onwards, which is what decides whether the
//!   attributes carry an (always empty) withdrawals list.
//!
//! That keeps one binary usable against a dev chain, celo-sepolia and Celo mainnet with no
//! `--chain` argument to get wrong, and makes a range that crosses a fork boundary replay
//! correctly block by block instead of failing with `-38005 Unsupported fork`.

use alloy_consensus::Header;
use alloy_primitives::{B64, Bytes};
use alloy_rpc_types_engine::PayloadAttributes;
use anyhow::{Context, bail};
use op_alloy_rpc_types_engine::OpPayloadAttributes;

/// `extraData` length for the Holocene encoding: one version byte plus the 8-byte EIP-1559
/// parameter pair (`denominator || elasticity`, both big-endian `u32`).
const HOLOCENE_EXTRA_DATA_LEN: usize = 9;

/// `extraData` length for the Jovian encoding: Holocene's 9 bytes plus a big-endian `u64`
/// minimum base fee.
const JOVIAN_EXTRA_DATA_LEN: usize = 17;

/// `extraData` version byte written by the Holocene encoder.
const HOLOCENE_VERSION: u8 = 0;

/// `extraData` version byte written by the Jovian encoder.
const JOVIAN_VERSION: u8 = 1;

/// The engine API method version a given block must be built and submitted with.
///
/// Only the two versions a live Celo chain can produce are modelled. `forkchoiceUpdated` is
/// always V3 for both — the OP engine API has no V4 of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EngineVersion {
    /// Ecotone through Holocene: `getPayloadV3` / `newPayloadV3`.
    V3,
    /// Isthmus onwards: `getPayloadV4` / `newPayloadV4`, carrying the L2 withdrawals root.
    V4,
}

/// The EIP-1559 parameters a block's `extraData` encodes, if any.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Eip1559Encoding {
    /// Holocene's `denominator || elasticity` pair, verbatim from `extraData[1..9]`.
    params: Option<B64>,
    /// Jovian's minimum base fee, from `extraData[9..17]`.
    min_base_fee: Option<u64>,
}

/// Decode the EIP-1559 attribute fields back out of a sealed header's `extraData`.
///
/// This is the inverse of op-alloy's `encode_holocene_extra_data` /
/// `encode_jovian_extra_data`. An unrecognised layout is a hard error rather than a silent
/// `None`: replaying such a block would produce a *different* `extraData` and so a hash
/// mismatch that looks exactly like a consensus bug.
fn eip1559_encoding(header: &Header) -> anyhow::Result<Eip1559Encoding> {
    let extra = header.extra_data.as_ref();
    match (extra.len(), extra.first().copied()) {
        // Pre-Holocene: the block assembler forces `extraData` empty, and the attributes must
        // omit both fields.
        (0, _) => Ok(Eip1559Encoding::default()),
        (HOLOCENE_EXTRA_DATA_LEN, Some(HOLOCENE_VERSION)) => {
            Ok(Eip1559Encoding { params: Some(B64::from_slice(&extra[1..9])), min_base_fee: None })
        }
        (JOVIAN_EXTRA_DATA_LEN, Some(JOVIAN_VERSION)) => Ok(Eip1559Encoding {
            params: Some(B64::from_slice(&extra[1..9])),
            min_base_fee: Some(u64::from_be_bytes(
                extra[9..17].try_into().expect("slice of exactly 8 bytes"),
            )),
        }),
        _ => bail!(
            "block {} has extraData of {} bytes starting with {:?}, which is neither the empty \
             pre-Holocene form nor the 9-byte Holocene (version 0) nor the 17-byte Jovian \
             (version 1) encoding; this block cannot be reproduced through the build path",
            header.number,
            extra.len(),
            extra.first(),
        ),
    }
}

/// Which engine API payload version reproduces this block.
///
/// `requestsHash` is populated from Isthmus onwards and never before, which makes it an exact
/// discriminator without consulting fork timestamps. A block older than Ecotone (no
/// `parentBeaconBlockRoot`) cannot be driven by `forkchoiceUpdatedV3` at all and is rejected.
pub(crate) fn engine_version(header: &Header) -> anyhow::Result<EngineVersion> {
    if header.requests_hash.is_some() {
        Ok(EngineVersion::V4)
    } else if header.parent_beacon_block_root.is_some() {
        Ok(EngineVersion::V3)
    } else {
        bail!(
            "block {} predates Ecotone (no parentBeaconBlockRoot), so it cannot be driven with \
             engine_forkchoiceUpdatedV3; pick a range above the Ecotone activation",
            header.number,
        )
    }
}

/// Rebuild the payload attributes that reproduce `header` byte for byte.
///
/// `transactions` must be the block's full EIP-2718-encoded transaction list, in order,
/// including the leading L1-attributes deposit — the OP builder never synthesises it, it
/// executes exactly `attributes.transactions`.
///
/// `noTxPool` is always set: the pool is empty on a replay node and letting the builder consult
/// it would make the produced block depend on wall-clock arrival order.
pub(crate) fn payload_attributes(
    header: &Header,
    transactions: Vec<Bytes>,
) -> anyhow::Result<OpPayloadAttributes> {
    let eip1559 = eip1559_encoding(header)
        .with_context(|| format!("cannot derive attributes for block {}", header.number))?;

    Ok(OpPayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp: header.timestamp,
            prev_randao: header.mix_hash,
            suggested_fee_recipient: header.beneficiary,
            // Canyon onwards requires a present-but-empty list; before it, the field must be
            // absent. `withdrawalsRoot` is set exactly from Canyon onwards.
            withdrawals: header.withdrawals_root.is_some().then(Vec::new),
            parent_beacon_block_root: header.parent_beacon_block_root,
            slot_number: None,
        },
        transactions: Some(transactions),
        no_tx_pool: Some(true),
        // op-reth requires this on every version; the builder uses it verbatim.
        gas_limit: Some(header.gas_limit),
        eip_1559_params: eip1559.params,
        min_base_fee: eip1559.min_base_fee,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, b256};

    /// An Isthmus-shaped header with a Jovian `extraData`, i.e. what both live Celo chains
    /// produce today.
    fn jovian_header() -> Header {
        let mut extra = vec![JOVIAN_VERSION];
        extra.extend_from_slice(&[0, 0, 0, 250]); // denominator
        extra.extend_from_slice(&[0, 0, 0, 6]); // elasticity
        extra.extend_from_slice(&25_000_000_000u64.to_be_bytes()); // min base fee
        Header {
            number: 100,
            timestamp: 1_800_000_000,
            gas_limit: 30_000_000,
            mix_hash: b256!("0x1111111111111111111111111111111111111111111111111111111111111111"),
            extra_data: extra.into(),
            withdrawals_root: Some(B256::ZERO),
            parent_beacon_block_root: Some(B256::ZERO),
            requests_hash: Some(B256::ZERO),
            ..Default::default()
        }
    }

    #[test]
    fn test_jovian_extra_data_round_trips_into_attributes() {
        let header = jovian_header();
        let attrs = payload_attributes(&header, vec![]).unwrap();

        assert_eq!(attrs.eip_1559_params, Some(B64::from_slice(&header.extra_data[1..9])));
        assert_eq!(attrs.min_base_fee, Some(25_000_000_000));
        assert_eq!(attrs.gas_limit, Some(30_000_000));
        assert_eq!(attrs.no_tx_pool, Some(true));
        assert_eq!(attrs.payload_attributes.prev_randao, header.mix_hash);
        assert_eq!(attrs.payload_attributes.timestamp, header.timestamp);
        assert_eq!(attrs.payload_attributes.withdrawals, Some(vec![]));
        assert_eq!(engine_version(&header).unwrap(), EngineVersion::V4);
    }

    #[test]
    fn test_holocene_extra_data_omits_min_base_fee() {
        let mut header = jovian_header();
        header.extra_data = header.extra_data[..HOLOCENE_EXTRA_DATA_LEN].to_vec().into();
        // Undo the Jovian version byte.
        let mut extra = header.extra_data.to_vec();
        extra[0] = HOLOCENE_VERSION;
        header.extra_data = extra.into();

        let attrs = payload_attributes(&header, vec![]).unwrap();
        assert!(attrs.eip_1559_params.is_some());
        assert_eq!(attrs.min_base_fee, None);
    }

    #[test]
    fn test_pre_holocene_empty_extra_data_omits_both_fields() {
        let mut header = jovian_header();
        header.extra_data = Bytes::new();
        header.requests_hash = None;

        let attrs = payload_attributes(&header, vec![]).unwrap();
        assert_eq!(attrs.eip_1559_params, None);
        assert_eq!(attrs.min_base_fee, None);
        // No `requestsHash` but a `parentBeaconBlockRoot` means Ecotone..Isthmus, i.e. V3.
        assert_eq!(engine_version(&header).unwrap(), EngineVersion::V3);
    }

    /// A pre-Holocene block whose `extraData` carries a client version string (the shape
    /// non-OP chains use) can never be reproduced, because the assembler forces `extraData`
    /// empty. It must fail loudly at archive time rather than surface later as a hash mismatch.
    #[test]
    fn test_unrecognised_extra_data_is_rejected() {
        let mut header = jovian_header();
        header.extra_data = Bytes::from_static(b"celo-reth/v1.0.0");

        let err = payload_attributes(&header, vec![]).unwrap_err().to_string();
        assert!(err.contains("cannot derive attributes for block 100"), "{err}");
    }

    #[test]
    fn test_pre_ecotone_block_is_rejected() {
        let mut header = jovian_header();
        header.requests_hash = None;
        header.parent_beacon_block_root = None;

        let err = engine_version(&header).unwrap_err().to_string();
        assert!(err.contains("predates Ecotone"), "{err}");
    }

    /// Truncated or over-long variants of a recognised length must not be silently accepted.
    #[test]
    fn test_extra_data_length_and_version_must_agree() {
        let mut header = jovian_header();
        // Right Jovian length, wrong version byte.
        let mut extra = header.extra_data.to_vec();
        extra[0] = 7;
        header.extra_data = extra.into();
        assert!(payload_attributes(&header, vec![]).is_err());

        // Holocene version byte on a Jovian-length payload.
        let mut extra = header.extra_data.to_vec();
        extra[0] = HOLOCENE_VERSION;
        header.extra_data = extra.into();
        assert!(payload_attributes(&header, vec![]).is_err());
    }
}
