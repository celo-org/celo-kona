//! Shared storage for CIP-64 transaction execution results.
//!
//! This module provides a thread-safe storage mechanism for sharing CIP-64 transaction
//! execution results between the EVM handler and the receipt builder.

use alloc::{sync::Arc, vec::Vec};
use alloy_consensus::Eip658Value;
use alloy_evm::{Evm, eth::receipt_builder::ReceiptBuilderCtx};
use alloy_primitives::Address;
use celo_alloy_consensus::{CeloCip64Receipt, CeloTxType};
use celo_revm::Cip64Info;
use spin::Mutex;

/// Data needed by the receipt builder for a CIP-64 transaction.
#[derive(Debug, Clone)]
pub struct Cip64ReceiptData {
    /// The fee currency used (None means native CELO)
    pub fee_currency: Option<Address>,
    /// CIP-64 execution info (pre/post logs, reverted flag, etc.)
    pub cip64_info: Cip64Info,
}

/// Shared storage for CIP-64 transaction execution results.
///
/// `pending` is a single slot consumed by the receipt builder after each CIP-64
/// transaction. The block executor is strictly serial — every `transact_raw`
/// for a CIP-64 tx is followed by `build_receipt` for that same tx before the
/// next `transact_raw` — so one slot is sufficient. Storing more than one
/// entry would mean `transact_raw` ran twice without an intervening
/// `build_receipt` (e.g. an executor that skips commit on a per-tx condition,
/// retries, or parallelises), which would silently swap receipt logs / base
/// fee between txs and corrupt the receipts root. `store_cip64_info` panics
/// when the slot is already occupied to turn that silent corruption into a
/// loud, immediate failure.
///
/// The optional `all_entries` Vec (gated behind the `test-utils` feature)
/// accumulates every entry without consuming them, and exists purely so test
/// harnesses can inspect post-execution CIP-64 gas accounting. It must stay
/// off in production: `store_cip64_info` runs on every CIP-64 tx, and an
/// unbounded Vec on a long-running node would leak memory linearly with
/// CIP-64 tx volume.
#[derive(Debug, Clone, Default)]
pub struct Cip64Storage {
    /// Single-slot pending receipt data (consumed by receipt builder).
    pending: Arc<Mutex<Option<Cip64ReceiptData>>>,
    /// Accumulated entries for post-execution inspection (test-only).
    #[cfg(any(test, feature = "test-utils"))]
    all_entries: Arc<Mutex<Vec<Cip64ReceiptData>>>,
}

impl Cip64Storage {
    /// Stores CIP-64 execution info for a transaction, to be consumed by the next
    /// `build_receipt` call.
    ///
    /// Panics if the slot is already occupied: that means the executor invoked
    /// `transact_raw` for two CIP-64 txs without `build_receipt` running between
    /// them, which would corrupt the second tx's receipt. Failing loud here
    /// catches the bug at its source instead of at receipts-root divergence.
    pub fn store_cip64_info(&self, fee_currency: Option<Address>, info: Cip64Info) {
        let data = Cip64ReceiptData { fee_currency, cip64_info: info };
        #[cfg(any(test, feature = "test-utils"))]
        self.all_entries.lock().push(data.clone());
        let prev = self.pending.lock().replace(data);
        assert!(
            prev.is_none(),
            "Cip64Storage: store_cip64_info called with slot occupied — \
             executor invariant violated (transact_raw without intervening build_receipt)"
        );
    }

    /// Takes the pending CIP-64 receipt data for receipt building.
    pub fn pop_cip64_receipt_data(&self) -> Option<Cip64ReceiptData> {
        self.pending.lock().take()
    }

    /// Returns all stored CIP-64 receipt data entries (not consumed by this call).
    ///
    /// Only available when the `test-utils` feature is enabled — this is a
    /// diagnostic used by the kona executor's CIP-64 gas-accounting tests.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn all_entries(&self) -> Vec<Cip64ReceiptData> {
        self.all_entries.lock().clone()
    }

    /// Builds the inner [`CeloCip64Receipt`] for a CIP-64 transaction, popping the pending
    /// entry and merging its pre/post logs into the main execution logs.
    ///
    /// Callers wrap the result in their receipt envelope variant — bloomed for canonical
    /// types ([`CeloReceiptEnvelope`](celo_alloy_consensus::CeloReceiptEnvelope)) or
    /// bloomless for reth's on-disk format (`CeloReceipt`).
    pub fn build_cip64_receipt<E: Evm>(
        &self,
        ctx: ReceiptBuilderCtx<'_, CeloTxType, E>,
    ) -> CeloCip64Receipt {
        let success = ctx.result.is_success();
        let main_logs = ctx.result.into_logs();

        // Pop the CIP-64 receipt data stored during transact_raw
        let cip64_data = self.pop_cip64_receipt_data();
        assert!(
            cip64_data.is_some() || !success,
            "CIP-64 tx succeeded but no receipt data was stored — transact_raw invariant violated"
        );

        let base_fee_in_erc20 = cip64_data.as_ref().and_then(|d| d.cip64_info.base_fee_in_erc20);

        // Merge CIP-64 pre/post logs with the main execution logs if available.
        // `cip64_data` was just popped, so the log vectors are moved, not cloned.
        let logs = if let Some(data) = cip64_data {
            let info = data.cip64_info;
            let capacity = info.logs_pre.len() + main_logs.len() + info.logs_post.len();
            let mut merged = Vec::with_capacity(capacity);
            merged.extend(info.logs_pre);
            merged.extend(main_logs);
            merged.extend(info.logs_post);
            merged
        } else {
            main_logs
        };
        CeloCip64Receipt {
            inner: alloy_consensus::Receipt {
                status: Eip658Value::Eip658(success),
                cumulative_gas_used: ctx.cumulative_gas_used,
                logs,
            },
            base_fee: base_fee_in_erc20,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_drained_by_pop() {
        let storage = Cip64Storage::default();
        storage.store_cip64_info(None, Cip64Info::default());
        assert!(storage.pop_cip64_receipt_data().is_some());
        assert!(storage.pop_cip64_receipt_data().is_none());
    }

    #[test]
    #[should_panic(expected = "store_cip64_info called with slot occupied")]
    fn double_store_without_pop_panics() {
        let storage = Cip64Storage::default();
        storage.store_cip64_info(None, Cip64Info::default());
        storage.store_cip64_info(None, Cip64Info::default());
    }
}
