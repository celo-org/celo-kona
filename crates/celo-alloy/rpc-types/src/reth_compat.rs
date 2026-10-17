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

// ---------------------------------------------------------------------------
// Request → simulated / signed transaction (used by reth's RpcConverter)
// ---------------------------------------------------------------------------

// Boxing the Err variant keeps `Result<CeloTxEnvelope, _>` small even though `OpTxEnvelope` grew
// past clippy's result_large_err threshold after the kona-node v1.5.0 bump added the post-exec
// variant.
fn op_tx_to_celo(
    op_tx: op_alloy_consensus::OpTxEnvelope,
) -> Result<CeloTxEnvelope, alloc::boxed::Box<op_alloy_consensus::OpTxEnvelope>> {
    use op_alloy_consensus::OpTxEnvelope as Op;
    match op_tx {
        Op::Legacy(tx) => Ok(CeloTxEnvelope::Legacy(tx)),
        Op::Eip2930(tx) => Ok(CeloTxEnvelope::Eip2930(tx)),
        Op::Eip1559(tx) => Ok(CeloTxEnvelope::Eip1559(tx)),
        Op::Eip7702(tx) => Ok(CeloTxEnvelope::Eip7702(tx)),
        Op::Deposit(tx) => Ok(CeloTxEnvelope::Deposit(tx)),
        // Celo doesn't ship the OP-stack PostExec tx type. Return the envelope
        // so the RPC caller can surface a typed error instead of crashing.
        post @ Op::PostExec(_) => Err(alloc::boxed::Box::new(post)),
    }
}

impl reth_rpc_traits::TryIntoSimTx<CeloTxEnvelope> for crate::CeloTransactionRequest {
    fn try_into_sim_tx(self) -> Result<CeloTxEnvelope, alloy_consensus::error::ValueError<Self>> {
        use alloy_consensus::error::ValueError;

        let fee_currency = self.fee_currency;

        if let Err(conflict) = crate::check_cip64_compatibility(self.inner.as_ref(), fee_currency) {
            return Err(ValueError::new_static(self, conflict.message()));
        }

        self.inner
            .try_into_sim_tx()
            .map_err(|e| e.map(|inner| Self { inner, fee_currency }))
            .and_then(|op_tx| match op_tx_to_celo(op_tx) {
                Ok(mut celo_tx) => {
                    // If fee_currency is set, wrap the inner EIP-1559 tx into a CIP-64 variant
                    if let Some(fc) = fee_currency
                        && let CeloTxEnvelope::Eip1559(signed) = celo_tx
                    {
                        let (eip1559, sig, _hash) = signed.into_parts();
                        let cip64 = celo_alloy_consensus::TxCip64 {
                            chain_id: eip1559.chain_id,
                            nonce: eip1559.nonce,
                            gas_limit: eip1559.gas_limit,
                            max_fee_per_gas: eip1559.max_fee_per_gas,
                            max_priority_fee_per_gas: eip1559.max_priority_fee_per_gas,
                            to: eip1559.to,
                            value: eip1559.value,
                            access_list: eip1559.access_list,
                            input: eip1559.input,
                            fee_currency: Some(fc),
                        };
                        celo_tx = CeloTxEnvelope::Cip64(alloy_consensus::Signed::new_unhashed(
                            cip64, sig,
                        ));
                    }
                    Ok(celo_tx)
                }
                Err(rejected) => Err(ValueError::new_static(
                    Self { inner: (*rejected).into(), fee_currency },
                    "PostExec transactions are not supported on Celo",
                )),
            })
    }
}

impl reth_rpc_traits::SignableTxRequest<CeloTxEnvelope> for crate::CeloTransactionRequest {
    async fn try_build_and_sign(
        self,
        signer: impl alloy_network::TxSigner<alloy_primitives::Signature> + Send,
    ) -> Result<CeloTxEnvelope, reth_rpc_traits::SignTxRequestError> {
        use reth_rpc_traits::{SignTxRequestError, SignableTxRequest};

        if let Some(fc) = self.fee_currency {
            // Build a CIP-64 tx directly so fee_currency is preserved.
            let req = self.inner.as_ref();

            if let Err(conflict) = crate::check_cip64_compatibility(req, Some(fc)) {
                tracing::warn!(target: "celo::rpc", ?fc, "{}", conflict.message());
                return Err(SignTxRequestError::InvalidTransactionRequest);
            }

            // Validate required fields — defaulting to 0 would produce a
            // seemingly valid but nonsensical CIP-64 transaction.
            let chain_id = req.chain_id.ok_or(SignTxRequestError::InvalidTransactionRequest)?;
            let nonce = req.nonce.ok_or(SignTxRequestError::InvalidTransactionRequest)?;
            let gas_limit = req.gas.ok_or(SignTxRequestError::InvalidTransactionRequest)?;

            // CIP-64 is an EIP-1559-style dynamic-fee tx; both fee fields must
            // be present. Silently defaulting to 0 would produce a signed tx
            // that looks valid but is unusable once base fee > 0 — and
            // `gasPrice` (legacy) won't be mapped, so a caller that only sets
            // `gasPrice` would get a zero-fee CIP-64 tx without any warning.
            let max_fee_per_gas =
                req.max_fee_per_gas.ok_or(SignTxRequestError::InvalidTransactionRequest)?;
            let max_priority_fee_per_gas = req
                .max_priority_fee_per_gas
                .ok_or(SignTxRequestError::InvalidTransactionRequest)?;

            let mut cip64 = celo_alloy_consensus::TxCip64 {
                chain_id,
                nonce,
                gas_limit,
                max_fee_per_gas,
                max_priority_fee_per_gas,
                to: req.to.unwrap_or_default(),
                value: req.value.unwrap_or_default(),
                access_list: req.access_list.clone().unwrap_or_default(),
                input: req.input.clone().into_input().unwrap_or_default(),
                fee_currency: Some(fc),
            };
            let sig = signer.sign_transaction(&mut cip64).await?;
            Ok(CeloTxEnvelope::Cip64(alloy_consensus::Signed::new_unhashed(cip64, sig)))
        } else {
            SignableTxRequest::<op_alloy_consensus::OpTxEnvelope>::try_build_and_sign(
                self.inner, signer,
            )
            .await
            .and_then(|op_tx| {
                op_tx_to_celo(op_tx).map_err(|_| SignTxRequestError::InvalidTransactionRequest)
            })
        }
    }
}

#[cfg(test)]
mod request_tests {
    use crate::CeloTransactionRequest;
    use alloy_primitives::{Address, U256};
    use celo_alloy_consensus::CeloTxEnvelope;
    use op_alloy_rpc_types::OpTransactionRequest;
    use reth_rpc_traits::TryIntoSimTx;

    #[test]
    fn try_into_sim_tx_rejects_gas_price_with_fee_currency() {
        use alloy_network::TransactionBuilder;

        let fc = Address::with_last_byte(0xCC);
        // Build a legacy-style request (gas_price set, no max_fee_per_gas)
        let req = CeloTransactionRequest {
            inner: OpTransactionRequest::default()
                .to(Address::ZERO)
                .with_gas_price(1_000_000_000)
                .with_nonce(0)
                .with_chain_id(42220),
            fee_currency: Some(fc),
        };

        let result = req.try_into_sim_tx();
        assert!(result.is_err(), "gasPrice + feeCurrency must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("CIP-64") || err_msg.contains("maxFeePerGas"),
            "Error should mention CIP-64 or EIP-1559 fields, got: {err_msg}"
        );
    }

    /// CIP-64 has no `authorizationList` field. A request that pairs `feeCurrency`
    /// with EIP-7702 authorizations would silently drop the auth list on
    /// conversion to CIP-64, signing a tx that no longer matches caller intent.
    fn auth_list_request_with_fee_currency() -> CeloTransactionRequest {
        use alloy_eips::eip7702::{Authorization, SignedAuthorization};
        use alloy_network_primitives::TransactionBuilder7702;

        let auth = SignedAuthorization::new_unchecked(
            Authorization { chain_id: U256::ZERO, address: Address::ZERO, nonce: 0 },
            0,
            U256::ZERO,
            U256::ZERO,
        );
        let mut inner = OpTransactionRequest::default();
        TransactionBuilder7702::set_authorization_list(&mut inner, vec![auth]);
        CeloTransactionRequest { inner, fee_currency: Some(Address::with_last_byte(0xCC)) }
    }

    #[test]
    fn try_into_sim_tx_rejects_authorization_list_with_fee_currency() {
        let result = auth_list_request_with_fee_currency().try_into_sim_tx();
        assert!(result.is_err(), "authorizationList + feeCurrency must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("authorizationList"),
            "Error should mention authorizationList, got: {err_msg}"
        );
    }

    #[test]
    fn try_into_sim_tx_fee_currency_wraps_eip1559() {
        use alloy_eips::Typed2718;
        use alloy_network::TransactionBuilder;

        let fc = Address::with_last_byte(0xDD);
        let sender = Address::with_last_byte(1);
        let req = CeloTransactionRequest {
            inner: OpTransactionRequest::default()
                .to(Address::ZERO)
                .max_fee_per_gas(1_000_000_000)
                .max_priority_fee_per_gas(100)
                .gas_limit(21_000)
                .with_nonce(0)
                .with_chain_id(42220)
                .with_from(sender),
            fee_currency: Some(fc),
        };

        let tx = req.try_into_sim_tx().expect("EIP-1559 with fee_currency should succeed");
        match &tx {
            CeloTxEnvelope::Cip64(signed) => {
                assert_eq!(signed.tx().fee_currency, Some(fc));
            }
            other => panic!("Expected CIP-64 (0x7b), got type 0x{:02x}", other.ty()),
        }
    }
}
