//! Implementations of `reth-rpc-traits` for Celo types.
//!
//! Mirrors `op-alloy-rpc-types/src/reth_compat.rs`: the trait is foreign and so is the
//! consensus tx type (`CeloTxEnvelope`), so the impl has to live here, where the RPC
//! response type [`CeloTransaction`] is local.

use crate::{CeloTransaction, CeloTransactionInfo, transaction::cip64_effective_gas_price};
use alloy_primitives::Address;
use celo_alloy_consensus::CeloTxEnvelope;
use core::convert::Infallible;
use reth_rpc_traits::FromConsensusTx;

/// Build a [`CeloTransaction`] from a recovered consensus tx + the metadata in
/// [`CeloTransactionInfo`].
///
/// Non-CIP-64 paths delegate to [`CeloTransaction::from_transaction`], which mirrors
/// `op_alloy_rpc_types::Transaction::from_transaction` (deposits report `gasPrice = 0`;
/// other types report `effective_tip + base_fee`, falling back to `max_fee_per_gas`).
///
/// CIP-64 needs an override: `gasPrice` must be in fee-currency units, computed via
/// [`cip64_effective_gas_price`] against the FC base fee from the receipt — the same
/// formula the receipt path uses, so `eth_getTransactionByHash` and
/// `eth_getTransactionReceipt` report the same number. If the receipt isn't reachable
/// we fall back to `max_fee_per_gas` (still FC-denominated — never mixed with native wei).
impl FromConsensusTx<CeloTxEnvelope> for CeloTransaction {
    type TxInfo = CeloTransactionInfo;
    type Err = Infallible;

    fn from_consensus_tx(
        tx: CeloTxEnvelope,
        signer: Address,
        tx_info: Self::TxInfo,
    ) -> Result<Self, Self::Err> {
        use alloy_consensus::Transaction as _;

        let recovered = alloy_consensus::transaction::Recovered::new_unchecked(tx, signer);
        let mut out = Self::from_transaction(recovered, tx_info.inner);

        if matches!(out.inner.inner.inner(), CeloTxEnvelope::Cip64(_)) {
            let fc_max_fee = out.inner.inner.max_fee_per_gas();
            let fc_prio = out.inner.inner.max_priority_fee_per_gas().unwrap_or(0);
            let effective = tx_info
                .cip64_fc_base_fee
                .map_or(fc_max_fee, |fc_bf| cip64_effective_gas_price(fc_max_fee, fc_prio, fc_bf));
            out.inner.effective_gas_price = Some(effective);
        }

        Ok(out)
    }
}
