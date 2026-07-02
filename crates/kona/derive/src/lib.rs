//! Celo-specific derivation-pipeline extensions.
//!
//! This crate provides the Celo analogue of kona's `EthereumDataSource`
//! ([`CeloEthereumDataSource`]) with Espresso event-based batch authentication folded in. It
//! wraps/duplicates kona's data sources rather than patching upstream kona, mirroring how the rest
//! of celo-kona consumes the OP Stack rust crates.
//!
//! The batch-authentication logic is the Rust companion to the op-node changes in
//! celo-org/optimism#445, originally drafted against vendored kona in celo-org/optimism#449 and
//! relocated here.

#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod batch_auth;
pub use batch_auth::{
    BATCH_INFO_AUTHENTICATED_TOPIC, BatchAuthCache, BatchAuthConfig,
    collect_auth_events_from_receipts, collect_authenticated_batches, compute_blob_batch_hash,
    compute_calldata_batch_hash, is_batch_authorized,
};

mod blob_data;
pub use blob_data::CeloBlobData;

mod blobs;
pub use blobs::CeloBlobSource;

mod ethereum;
pub use ethereum::CeloEthereumDataSource;
