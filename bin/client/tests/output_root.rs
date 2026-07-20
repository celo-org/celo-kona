//! Tests for output-root preimage validation in the single-chain client.
//!
//! The helper uses `get_exact` with a `[u8; 128]` buffer. These tests cover
//! both the version-word check and that implicit length check.

use alloy_primitives::B256;
use celo_client::single;
use kona_preimage::{PreimageKey, errors::PreimageOracleError};
use kona_proof::errors::OracleProviderError;

mod common;
use common::MockOracle;

fn b256(fill: u8) -> B256 {
    B256::from([fill; 32])
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_safe_head_hash_rejects_unknown_output_version() {
    let agreed_root = b256(0xAA);
    let mut bad_preimage = [0u8; 128];
    bad_preimage[0] = 0x01;
    let oracle =
        MockOracle::single(PreimageKey::new_keccak256(*agreed_root), bad_preimage.to_vec());

    let err = single::fetch_safe_head_hash(&oracle, agreed_root).await.unwrap_err();
    match err {
        OracleProviderError::UnknownOutputVersion(version) => {
            assert_eq!(version[0], 0x01);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_safe_head_hash_rejects_wrong_preimage_length() {
    // `get_exact` with a `[u8; 128]` buffer enforces the length here. This
    // guards against a future refactor swapping it for `get`.
    let agreed_root = b256(0xAA);
    let oracle = MockOracle::single(PreimageKey::new_keccak256(*agreed_root), vec![0u8; 127]);

    let err = single::fetch_safe_head_hash(&oracle, agreed_root).await.unwrap_err();
    match err {
        OracleProviderError::Preimage(PreimageOracleError::BufferLengthMismatch(
            expected,
            actual,
        )) => {
            assert_eq!(expected, 128);
            assert_eq!(actual, 127);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}
