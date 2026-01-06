//! Protocol constants for the executor.
//! Identical to kona-executor, but had to duplicate it because the original is private.
use alloy_primitives::{B256, b256};

/// Empty SHA-256 hash.
pub(crate) const SHA256_EMPTY: B256 =
    b256!("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");

/// The Celo EIP-1559 base fee floor (in wei).
pub(crate) const CELO_EIP_1559_BASE_FEE_FLOOR: u64 = 25_000_000_000;
