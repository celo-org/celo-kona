//! Transaction fillers for the [`Celo`] network.
//!
//! [`Celo`]: crate::Celo

use crate::Celo;
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, U256};
use alloy_provider::{
    Provider, SendableTx,
    fillers::{
        ChainIdFiller, FillerControlFlow, GasFillable, GasFiller, JoinFill, NonceFiller,
        RecommendedFillers, TxFiller,
    },
};
use alloy_transport::{RpcError, TransportError, TransportErrorKind, TransportResult};
use celo_alloy_rpc_types::CeloTransactionRequest;

impl RecommendedFillers for Celo {
    type RecommendedFillers = JoinFill<CeloGasFiller, JoinFill<NonceFiller, ChainIdFiller>>;

    fn recommended_fillers() -> Self::RecommendedFillers {
        Default::default()
    }
}

/// Gas properties for a Celo transaction request, ready to be filled in.
#[derive(Clone, Copy, Debug)]
pub enum CeloGasFillable {
    /// A native-fee transaction, prepared by the stock [`GasFiller`].
    Native(GasFillable),
    /// A CIP-64 transaction; the fee values are denominated in the fee currency, or in
    /// native wei for a native-fee CIP-64 (no fee currency set).
    Cip64 {
        /// Gas limit including the CIP-64 intrinsic surcharge, which the node's estimate
        /// covers because the request carries `feeCurrency`.
        gas_limit: u64,
        /// Maximum fee per gas, in the denomination the variant doc names.
        max_fee_per_gas: u128,
        /// Maximum priority fee per gas, in the same denomination.
        max_priority_fee_per_gas: u128,
    },
}

/// A [`TxFiller`] that populates unset gas fields of Celo transaction requests.
///
/// Non-CIP-64 requests go to the stock [`GasFiller`]. A fee currency denominates the fee
/// fields in that currency, so native suggestions would be off by its exchange rate; the fees
/// come from the fee-currency-parameterized `eth_gasPrice` and `eth_maxPriorityFeePerGas`
/// instead. Native-fee CIP-64 (`0x7b`, no fee currency) shares that path, which unlike the
/// stock filler keeps an individually preset fee field.
///
/// The gas estimate carries the resolved fees, so the node must price a CIP-64 estimate in
/// the fee currency: celo-reth v1.0.5 or later, or Celo op-geth. An older celo-reth compares
/// the cap with the native base fee and fails the estimate for a currency worth more than
/// about two CELO.
///
/// The pool admits a CIP-64 transaction only if the sender holds `gas_limit × max_fee_per_gas`
/// in the fee currency, not just what the transaction ends up paying. With the filled cap of
/// `2·baseFee + tip` that is about twice the expected cost.
///
/// A CIP-64 request carrying `gasPrice` or an EIP-7702 `authorizationList` is refused here,
/// before any RPC, because a node that signs the request itself may drop the fee currency
/// rather than reject: op-geth builds a native transaction and charges the fee-currency price
/// in CELO. The refusal covers submission only. `eth_call` and `eth_estimateGas` go out as the
/// caller wrote them, since alloy's `FillProvider` discards the error from its call-path hook,
/// and answering them is the node's business.
#[derive(Clone, Debug, Default)]
pub struct CeloGasFiller {
    inner: GasFiller,
}

/// The refusal a conflicted CIP-64 request earns, shared by [`TxFiller::status`] and
/// [`TxFiller::prepare`] so the two cannot disagree about which requests are refused.
///
/// `status` must keep such a request unfinished and `prepare` must then fail it: a request
/// that stays unfinished while `prepare` succeeds spins until alloy's fill loop panics.
fn cip64_refusal(tx: &CeloTransactionRequest) -> Option<TransportError> {
    let conflicts = crate::cip64_conflicts(tx);
    (tx.is_cip64() && !conflicts.is_empty())
        .then(|| RpcError::local_usage_str(&conflicts.join(", ")))
}

impl CeloGasFiller {
    async fn prepare_cip64<P: Provider<Celo>>(
        &self,
        provider: &P,
        tx: &CeloTransactionRequest,
        fee_currency: Option<Address>,
    ) -> TransportResult<CeloGasFillable> {
        // The fees come first: the gas estimate carries them.
        let (max_fee_per_gas, max_priority_fee_per_gas) =
            self.cip64_fees(provider, tx, fee_currency).await?;

        let gas_limit = match tx.inner.as_ref().gas {
            Some(gas_limit) => gas_limit,
            None => {
                let request = estimation_request(tx, max_fee_per_gas, max_priority_fee_per_gas);
                provider.estimate_gas(request).await?
            }
        };

        Ok(CeloGasFillable::Cip64 { gas_limit, max_fee_per_gas, max_priority_fee_per_gas })
    }

    /// Resolves `(max_fee_per_gas, max_priority_fee_per_gas)`: the caller's presets where
    /// set, the node's suggestions for the rest. Both are in units of `fee_currency` when
    /// one is set, in native CELO wei otherwise.
    async fn cip64_fees<P: Provider<Celo>>(
        &self,
        provider: &P,
        tx: &CeloTransactionRequest,
        fee_currency: Option<Address>,
    ) -> TransportResult<(u128, u128)> {
        let preset_max_fee = tx.inner.as_ref().max_fee_per_gas;
        let preset_tip = tx.inner.as_ref().max_priority_fee_per_gas;
        if let (Some(max_fee_per_gas), Some(max_priority_fee_per_gas)) =
            (preset_max_fee, preset_tip)
        {
            return Ok((max_fee_per_gas, max_priority_fee_per_gas));
        }

        // Both methods take an optional feeCurrency parameter on celo-reth and Celo op-geth.
        // Only the missing fields are filled, mirroring the node's `fill_cip64_fee_defaults`.
        // The suggested tip is fetched even when the caller set one: `eth_gasPrice` returns
        // base fee + suggested tip, so the base fee only falls out of the node's own tip.
        let suggested_tip: U256 = match fee_currency {
            Some(fee_currency) => {
                provider.raw_request("eth_maxPriorityFeePerGas".into(), (fee_currency,)).await?
            }
            None => U256::from(provider.get_max_priority_fee_per_gas().await?),
        };

        // A missing tip takes the suggestion, clamped to the caller's max fee: a tip above
        // the cap is an invalid request.
        let tip = preset_tip.map_or_else(
            || {
                preset_max_fee
                    .map_or(suggested_tip, |max_fee| suggested_tip.min(U256::from(max_fee)))
            },
            U256::from,
        );

        let max_fee = match preset_max_fee {
            Some(max_fee) => U256::from(max_fee),
            None => {
                let price: U256 = match fee_currency {
                    Some(fee_currency) => {
                        provider.raw_request("eth_gasPrice".into(), (fee_currency,)).await?
                    }
                    None => U256::from(provider.get_gas_price().await?),
                };
                cip64_max_fee(price, suggested_tip, tip)
            }
        };

        let to_u128 = |value: U256, field: &'static str| -> TransportResult<u128> {
            u128::try_from(value).map_err(|_| {
                TransportErrorKind::custom_str(&format!("{field} {value} overflows u128"))
            })
        };

        Ok((to_u128(max_fee, "maxFeePerGas")?, to_u128(tip, "maxPriorityFeePerGas")?))
    }
}

/// Builds the request used to estimate a CIP-64 transaction's gas limit: `tx` with the fees
/// it will be sent with.
///
/// The node prices the simulation from them, in the fee currency when one is set. `GASPRICE`
/// then reads the price the tx would pay, and a sender who cannot pay `gas × maxFeePerGas`
/// gets an allowance error from the estimate rather than a rejection from the pool. Both
/// fields are always set, because the node rejects a tip without a cap, which is what a
/// caller who preset only the tip would otherwise send.
fn estimation_request(
    tx: &CeloTransactionRequest,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
) -> CeloTransactionRequest {
    let mut request = tx.clone();
    request.as_mut().max_fee_per_gas = Some(max_fee_per_gas);
    request.as_mut().max_priority_fee_per_gas = Some(max_priority_fee_per_gas);
    request
}

/// Computes the CIP-64 `max_fee_per_gas` from the node's gas-price and tip suggestions plus
/// the tip the transaction will use.
///
/// Mirrors the node's own default of `2·baseFee + tip`: `gasPrice ≈ baseFee + suggestedTip`,
/// so `maxFee = 2·(gasPrice − suggestedTip) + tip`. Falls back to `2·gasPrice` if the node
/// reports a tip above its gas price, and never drops below `tip` itself.
fn cip64_max_fee(price: U256, suggested_tip: U256, tip: U256) -> U256 {
    if price > suggested_tip {
        (price - suggested_tip).saturating_mul(U256::from(2)).saturating_add(tip)
    } else {
        price.saturating_mul(U256::from(2)).max(tip)
    }
}

impl TxFiller<Celo> for CeloGasFiller {
    type Fillable = CeloGasFillable;

    fn status(&self, tx: &CeloTransactionRequest) -> FillerControlFlow {
        // `Ready` rather than `Finished`, so `prepare` runs and reports the conflict. The
        // stock rules finish a `gasPrice` request as legacy without ever consulting the fee
        // currency, which would hand it to the node unexamined. Not `Missing` either: that
        // ends the fill loop, and `sign_transaction` forwards what is left.
        if cip64_refusal(tx).is_some() {
            return FillerControlFlow::Ready;
        }
        // Otherwise the stock rules work for CIP-64 too: finished once the gas limit and
        // both EIP-1559 fee fields are set.
        TxFiller::<Celo>::status(&self.inner, tx)
    }

    fn fill_sync(&self, _tx: &mut SendableTx<Celo>) {}

    async fn prepare<P>(
        &self,
        provider: &P,
        tx: &CeloTransactionRequest,
    ) -> TransportResult<Self::Fillable>
    where
        P: Provider<Celo>,
    {
        // Before the routing below and before any request goes out. This is the last refusal
        // on the path where the node signs: `prep_for_submission` returns `()`, and nothing
        // on the send path consults `can_submit`. A wallet-backed provider refuses later too,
        // in `build_unsigned`.
        if let Some(refusal) = cip64_refusal(tx) {
            return Err(refusal);
        }

        if tx.is_cip64() {
            // Native-fee CIP-64 takes this path too: the stock filler would discard an
            // individually preset fee field.
            self.prepare_cip64(provider, tx, tx.fee_currency).await
        } else {
            TxFiller::<Celo>::prepare(&self.inner, provider, tx).await.map(CeloGasFillable::Native)
        }
    }

    async fn fill(
        &self,
        fillable: Self::Fillable,
        mut tx: SendableTx<Celo>,
    ) -> TransportResult<SendableTx<Celo>> {
        match fillable {
            CeloGasFillable::Native(fillable) => {
                TxFiller::<Celo>::fill(&self.inner, fillable, tx).await
            }
            CeloGasFillable::Cip64 { gas_limit, max_fee_per_gas, max_priority_fee_per_gas } => {
                if let Some(builder) = tx.as_mut_builder() {
                    builder.set_gas_limit(gas_limit);
                    builder.set_max_fee_per_gas(max_fee_per_gas);
                    builder.set_max_priority_fee_per_gas(max_priority_fee_per_gas);
                }
                Ok(tx)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_provider::{Identity, ProviderBuilder};
    use alloy_transport::mock::Asserter;
    use celo_alloy_consensus::CeloTxType;

    fn mocked_provider(asserter: Asserter) -> impl Provider<Celo> {
        ProviderBuilder::<Identity, Identity, Celo>::default().connect_mocked_client(asserter)
    }

    #[test]
    fn max_fee_doubles_base_fee_and_adds_tip() {
        // gasPrice = baseFee (100) + suggested tip (7) => maxFee = 2*100 + 7.
        assert_eq!(cip64_max_fee(U256::from(107), U256::from(7), U256::from(7)), U256::from(207));
    }

    #[test]
    fn max_fee_uses_caller_tip_over_suggestion() {
        // Caller preset a tip of 3; base fee still derives from the suggested tip (7).
        assert_eq!(cip64_max_fee(U256::from(107), U256::from(7), U256::from(3)), U256::from(203));
    }

    #[test]
    fn max_fee_with_zero_tip_doubles_gas_price() {
        assert_eq!(cip64_max_fee(U256::from(100), U256::ZERO, U256::ZERO), U256::from(200));
    }

    #[test]
    fn max_fee_falls_back_when_tip_exceeds_gas_price() {
        // Nonsensical node response (tip > gasPrice): still produce a usable cap.
        assert_eq!(cip64_max_fee(U256::from(5), U256::from(9), U256::from(9)), U256::from(10));
        // tip == gasPrice hits the same fallback.
        assert_eq!(cip64_max_fee(U256::from(5), U256::from(5), U256::from(5)), U256::from(10));
    }

    #[test]
    fn max_fee_fallback_clamps_to_caller_tip() {
        // Degenerate response plus a caller tip above 2·gasPrice: the cap must not fall
        // below the tip.
        assert_eq!(cip64_max_fee(U256::from(5), U256::from(9), U256::from(30)), U256::from(30));
    }

    #[test]
    fn max_fee_saturates_instead_of_overflowing() {
        let max = U256::MAX;
        assert_eq!(cip64_max_fee(max, U256::ZERO, U256::ZERO), max);
        assert_eq!(cip64_max_fee(max, U256::from(1), U256::from(1)), max);
    }

    #[tokio::test]
    async fn native_fee_cip64_preserves_preset_tip() {
        let asserter = Asserter::new();
        // The native-fee CIP-64 path resolves the missing max fee from the node's native
        // suggestions: suggested tip first, then gas price.
        asserter.push_success(&U256::from(7u64));
        asserter.push_success(&U256::from(107u64));
        let provider = mocked_provider(asserter);

        let mut tx = CeloTransactionRequest::default()
            .to(Address::ZERO)
            .gas_limit(50_000)
            .max_priority_fee_per_gas(30);
        tx.as_mut().transaction_type = Some(CeloTxType::Cip64 as u8);

        let fillable = TxFiller::<Celo>::prepare(&CeloGasFiller::default(), &provider, &tx)
            .await
            .expect("prepare should succeed");
        let CeloGasFillable::Cip64 { gas_limit, max_fee_per_gas, max_priority_fee_per_gas } =
            fillable
        else {
            panic!("native-fee CIP-64 must take the CIP-64 path, got {fillable:?}");
        };
        assert_eq!(gas_limit, 50_000);
        // The preset tip survives (the stock filler would discard it); the cap derives
        // from it: 2·(107 − 7) + 30.
        assert_eq!(max_priority_fee_per_gas, 30);
        assert_eq!(max_fee_per_gas, 230);
    }

    fn conflicted_cip64(
        conflict: impl FnOnce(&mut alloy_rpc_types_eth::TransactionRequest),
    ) -> CeloTransactionRequest {
        let mut tx = CeloTransactionRequest::default()
            .to(Address::ZERO)
            .from(Address::repeat_byte(9))
            .nonce(1)
            .gas_limit(50_000)
            .fee_currency(Address::repeat_byte(1));
        tx.as_mut().chain_id = Some(1);
        conflict(tx.as_mut());
        tx
    }

    #[tokio::test]
    async fn send_transaction_refuses_conflicts_without_reaching_the_node() {
        // The whole stack: recommended fillers, no wallet, so the node would be the one to
        // sign. op-geth would sign both of these as native transactions with `feeCurrency`
        // dropped, so nothing may leave the client. An empty `Asserter` panics if consumed,
        // which is the assertion that no RPC went out.
        for (label, tx) in [
            ("gasPrice", conflicted_cip64(|req| req.gas_price = Some(25_000_000_000))),
            (
                "authorizationList",
                conflicted_cip64(|req| req.authorization_list = Some(Vec::new())),
            ),
        ] {
            let provider =
                ProviderBuilder::new_with_network::<Celo>().connect_mocked_client(Asserter::new());

            let err =
                provider.send_transaction(tx).await.expect_err("conflict must not reach the node");
            assert!(err.to_string().contains(label), "{label}: unhelpful error: {err}");
            assert!(err.is_local_usage_error(), "{label}: wrong error class: {err:?}");
        }
    }

    #[tokio::test]
    async fn fill_applies_the_resolved_values() {
        // Nothing else exercises `fill`: transposing the cap and the tip here would leave a
        // transaction whose tip exceeds its own cap, which the pool rejects.
        let tx = CeloTransactionRequest::default()
            .to(Address::ZERO)
            .fee_currency(Address::repeat_byte(1));
        let fillable = CeloGasFillable::Cip64 {
            gas_limit: 71_000,
            max_fee_per_gas: 207,
            max_priority_fee_per_gas: 7,
        };

        let filled =
            TxFiller::<Celo>::fill(&CeloGasFiller::default(), fillable, SendableTx::Builder(tx))
                .await
                .expect("fill should succeed");
        let SendableTx::Builder(filled) = filled else { panic!("fill must not build an envelope") };

        assert_eq!(filled.as_ref().gas, Some(71_000));
        assert_eq!(filled.as_ref().max_fee_per_gas, Some(207));
        assert_eq!(filled.as_ref().max_priority_fee_per_gas, Some(7));
        // What `fill` leaves behind must satisfy `status`, or the fill loop runs again and
        // eventually panics.
        assert!(TxFiller::<Celo>::status(&CeloGasFiller::default(), &filled).is_finished());
    }

    #[test]
    fn status_delegates_for_native_requests() {
        // Without the CIP-64 gate every legacy request would report `Ready` forever while
        // `prepare` succeeds, spinning the fill loop into alloy's panic.
        let mut tx = CeloTransactionRequest::default().to(Address::ZERO).gas_limit(21_000);
        tx.as_mut().gas_price = Some(25_000_000_000);

        assert!(TxFiller::<Celo>::status(&CeloGasFiller::default(), &tx).is_finished());
    }

    #[test]
    fn conflicted_cip64_is_never_finished() {
        // The stock rule finishes on `gasPrice` plus a gas limit, which would hand a
        // conflicted request straight to the node.
        let mut tx = CeloTransactionRequest::default()
            .to(Address::ZERO)
            .gas_limit(50_000)
            .fee_currency(Address::repeat_byte(1));
        tx.as_mut().gas_price = Some(25_000_000_000);

        let status = TxFiller::<Celo>::status(&CeloGasFiller::default(), &tx);
        assert!(!status.is_finished(), "conflicted CIP-64 reported {status:?}");
    }

    #[tokio::test]
    async fn prepare_reports_cip64_conflicts() {
        // No responses queued: the conflict must be caught before any request goes out.
        let provider = mocked_provider(Asserter::new());

        let mut tx = CeloTransactionRequest::default()
            .to(Address::ZERO)
            .gas_limit(50_000)
            .fee_currency(Address::repeat_byte(1));
        tx.as_mut().gas_price = Some(25_000_000_000);

        let err = TxFiller::<Celo>::prepare(&CeloGasFiller::default(), &provider, &tx)
            .await
            .expect_err("a gasPrice conflict must not fill");
        assert!(err.to_string().contains("gasPrice"), "unhelpful error: {err}");

        let mut tx = CeloTransactionRequest::default()
            .to(Address::ZERO)
            .gas_limit(50_000)
            .fee_currency(Address::repeat_byte(1));
        tx.as_mut().authorization_list = Some(Vec::new());

        let err = TxFiller::<Celo>::prepare(&CeloGasFiller::default(), &provider, &tx)
            .await
            .expect_err("an authorizationList conflict must not fill");
        assert!(err.to_string().contains("authorizationList"), "unhelpful error: {err}");
    }

    #[test]
    fn estimation_request_carries_the_resolved_fees() {
        // The caller preset only a tip. Sent as is, the node would reject the estimate: a tip
        // without a cap is a tip above a zero cap.
        let tx = CeloTransactionRequest::default()
            .to(Address::ZERO)
            .max_priority_fee_per_gas(30)
            .fee_currency(Address::repeat_byte(1));

        let request = estimation_request(&tx, 230, 30);

        assert_eq!(request.as_ref().max_fee_per_gas, Some(230));
        assert_eq!(request.as_ref().max_priority_fee_per_gas, Some(30));
        // The fee currency must survive, or the estimate misses the intrinsic surcharge and
        // the node prices the fees as native CELO.
        assert_eq!(request.fee_currency, Some(Address::repeat_byte(1)));
    }

    #[tokio::test]
    async fn cip64_fills_fee_currency_denominated_fees() {
        let asserter = Asserter::new();
        // In call order: tip suggestion, gas price, gas estimate. The estimate comes last
        // because it carries the fees. The positional `Asserter` sees neither method names
        // nor params, so this test covers the order and the arithmetic only; the
        // `cip64_reads_*` tests below pin which methods ran, and only
        // `examples/send_cip64.rs` exercises the `feeCurrency` parameter itself.
        asserter.push_success(&U256::from(7u64));
        asserter.push_success(&U256::from(107u64));
        asserter.push_success(&U256::from(71_000u64));
        let provider = mocked_provider(asserter);

        let tx = CeloTransactionRequest::default()
            .to(Address::ZERO)
            .fee_currency(Address::repeat_byte(1));

        let fillable = TxFiller::<Celo>::prepare(&CeloGasFiller::default(), &provider, &tx)
            .await
            .expect("prepare should succeed");
        let CeloGasFillable::Cip64 { gas_limit, max_fee_per_gas, max_priority_fee_per_gas } =
            fillable
        else {
            panic!("expected a CIP-64 fillable, got {fillable:?}");
        };
        assert_eq!(gas_limit, 71_000);
        assert_eq!(max_priority_fee_per_gas, 7);
        // 2·(107 − 7) + 7, every term in fee-currency units.
        assert_eq!(max_fee_per_gas, 207);
    }

    #[tokio::test]
    async fn cip64_clamps_filled_tip_to_caller_max_fee() {
        let asserter = Asserter::new();
        // Only the tip suggestion is fetched; the caller's max fee short-circuits eth_gasPrice.
        asserter.push_success(&U256::from(9u64));
        let provider = mocked_provider(asserter);

        let tx = CeloTransactionRequest::default()
            .to(Address::ZERO)
            .gas_limit(50_000)
            .max_fee_per_gas(5)
            .fee_currency(Address::repeat_byte(1));

        let fillable = TxFiller::<Celo>::prepare(&CeloGasFiller::default(), &provider, &tx)
            .await
            .expect("prepare should succeed");
        let CeloGasFillable::Cip64 { gas_limit, max_fee_per_gas, max_priority_fee_per_gas } =
            fillable
        else {
            panic!("expected a CIP-64 fillable, got {fillable:?}");
        };
        // Suggested tip 9 exceeds the caller's cap of 5, so it is clamped; left unclamped the
        // request would carry a tip above its own fee cap and be rejected by the pool.
        assert_eq!((gas_limit, max_fee_per_gas, max_priority_fee_per_gas), (50_000, 5, 5));
    }

    #[tokio::test]
    async fn cip64_reads_suggestions_in_fee_currency_units() {
        // The positional queue alone cannot pin which suggestion was fetched, but a value
        // above `u128::MAX` can: only the fee-currency request deserializes it into `U256`
        // and reaches the overflow guard, while the native one is `u128` and fails to parse.
        let asserter = Asserter::new();
        asserter.push_success(&U256::MAX);
        asserter.push_success(&U256::from(107u64));
        let provider = mocked_provider(asserter);

        let tx = CeloTransactionRequest::default()
            .to(Address::ZERO)
            .gas_limit(50_000)
            .fee_currency(Address::repeat_byte(1));

        let err = TxFiller::<Celo>::prepare(&CeloGasFiller::default(), &provider, &tx)
            .await
            .expect_err("a suggestion above u128::MAX must not be truncated");
        assert!(
            err.to_string().contains("maxFeePerGas") && err.to_string().contains("overflows u128"),
            "expected the overflow error only the fee-currency path can reach, got: {err}"
        );
    }

    #[tokio::test]
    async fn cip64_reads_gas_price_in_fee_currency_units() {
        // The same discriminator for the gas-price method: the tip parses either way, and
        // the oversized value in the gas-price slot survives only the fee-currency path.
        let asserter = Asserter::new();
        asserter.push_success(&U256::from(7u64));
        asserter.push_success(&U256::MAX);
        let provider = mocked_provider(asserter);

        let tx = CeloTransactionRequest::default()
            .to(Address::ZERO)
            .gas_limit(50_000)
            .fee_currency(Address::repeat_byte(1));

        let err = TxFiller::<Celo>::prepare(&CeloGasFiller::default(), &provider, &tx)
            .await
            .expect_err("a gas-price suggestion above u128::MAX must not be truncated");
        assert!(
            err.to_string().contains("maxFeePerGas") && err.to_string().contains("overflows u128"),
            "expected the overflow error only the fee-currency path can reach, got: {err}"
        );
    }

    #[tokio::test]
    async fn zero_fee_currency_reads_fee_currency_suggestions() {
        // The zero address gets no special treatment, so the tip comes from the fee-currency
        // (`U256`) method: it parses the oversized value and clamps it to the preset max fee,
        // where the native (`u128`) method would fail to deserialize it.
        let asserter = Asserter::new();
        asserter.push_success(&U256::MAX);
        let provider = mocked_provider(asserter);

        let tx = CeloTransactionRequest::default()
            .to(Address::ZERO)
            .gas_limit(50_000)
            .max_fee_per_gas(5)
            .fee_currency(Address::ZERO);

        let fillable = TxFiller::<Celo>::prepare(&CeloGasFiller::default(), &provider, &tx)
            .await
            .expect("the fee-currency tip fetch must parse a U256::MAX response");
        let CeloGasFillable::Cip64 { gas_limit, max_fee_per_gas, max_priority_fee_per_gas } =
            fillable
        else {
            panic!("expected a CIP-64 fillable, got {fillable:?}");
        };
        assert_eq!((gas_limit, max_fee_per_gas, max_priority_fee_per_gas), (50_000, 5, 5));
    }

    #[tokio::test]
    async fn cip64_preserves_fully_preset_fees_without_rpc() {
        // No queued responses: any RPC request would fail the test.
        let provider = mocked_provider(Asserter::new());

        let tx = CeloTransactionRequest::default()
            .to(Address::ZERO)
            .gas_limit(50_000)
            .max_fee_per_gas(200)
            .max_priority_fee_per_gas(30)
            .fee_currency(Address::repeat_byte(1));

        let fillable = TxFiller::<Celo>::prepare(&CeloGasFiller::default(), &provider, &tx)
            .await
            .expect("prepare should succeed");
        let CeloGasFillable::Cip64 { gas_limit, max_fee_per_gas, max_priority_fee_per_gas } =
            fillable
        else {
            panic!("expected a CIP-64 fillable, got {fillable:?}");
        };
        assert_eq!((gas_limit, max_fee_per_gas, max_priority_fee_per_gas), (50_000, 200, 30));
    }

    #[tokio::test]
    async fn cip64_estimates_a_tip_only_request_after_resolving_its_cap() {
        // In call order: tip suggestion, gas price, gas estimate. The estimate takes the
        // third response, so it ran after the cap it has to carry was resolved.
        let asserter = Asserter::new();
        asserter.push_success(&U256::from(7u64));
        asserter.push_success(&U256::from(107u64));
        asserter.push_success(&U256::from(71_000u64));
        let provider = mocked_provider(asserter);

        let tx = CeloTransactionRequest::default()
            .to(Address::ZERO)
            .max_priority_fee_per_gas(30)
            .fee_currency(Address::repeat_byte(1));

        let fillable = TxFiller::<Celo>::prepare(&CeloGasFiller::default(), &provider, &tx)
            .await
            .expect("prepare should succeed");
        let CeloGasFillable::Cip64 { gas_limit, max_fee_per_gas, max_priority_fee_per_gas } =
            fillable
        else {
            panic!("expected a CIP-64 fillable, got {fillable:?}");
        };
        // The preset tip survives and the cap derives from it: 2·(107 − 7) + 30.
        assert_eq!((gas_limit, max_fee_per_gas, max_priority_fee_per_gas), (71_000, 230, 30));
    }

    #[tokio::test]
    async fn cip64_estimates_gas_under_fully_preset_fees() {
        // Preset fees need no suggestion, but a missing gas limit is still estimated: the
        // one queued response is the estimate.
        let asserter = Asserter::new();
        asserter.push_success(&U256::from(71_000u64));
        let provider = mocked_provider(asserter);

        let tx = CeloTransactionRequest::default()
            .to(Address::ZERO)
            .max_fee_per_gas(200)
            .max_priority_fee_per_gas(30)
            .fee_currency(Address::repeat_byte(1));

        let fillable = TxFiller::<Celo>::prepare(&CeloGasFiller::default(), &provider, &tx)
            .await
            .expect("prepare should succeed");
        let CeloGasFillable::Cip64 { gas_limit, max_fee_per_gas, max_priority_fee_per_gas } =
            fillable
        else {
            panic!("expected a CIP-64 fillable, got {fillable:?}");
        };
        assert_eq!((gas_limit, max_fee_per_gas, max_priority_fee_per_gas), (71_000, 200, 30));
    }
}
