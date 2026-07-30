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
use alloy_transport::{TransportErrorKind, TransportResult};
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
        /// Gas limit including the CIP-64 intrinsic surcharge (the node's estimate covers
        /// it because the serialized request carries `feeCurrency`).
        gas_limit: u64,
        /// Maximum fee per gas, in units of the fee currency.
        max_fee_per_gas: u128,
        /// Maximum priority fee per gas, in units of the fee currency.
        max_priority_fee_per_gas: u128,
    },
}

/// A [`TxFiller`] that populates gas-related fields of Celo transaction requests if unset.
///
/// For non-CIP-64 requests this behaves exactly like the stock [`GasFiller`]. For CIP-64
/// requests with a fee currency the fee fields are denominated in that currency, so the
/// native fee estimates would be wrong by the currency's exchange rate; instead the fees are
/// fetched with the fee-currency-parameterized `eth_gasPrice` and `eth_maxPriorityFeePerGas`
/// methods that Celo nodes expose. Native-fee CIP-64 requests (type `0x7b` without a fee
/// currency) use the native suggestions on the same path, which — unlike the stock filler —
/// preserves an individually preset fee field.
#[derive(Clone, Debug, Default)]
pub struct CeloGasFiller {
    inner: GasFiller,
}

impl CeloGasFiller {
    async fn prepare_cip64<P: Provider<Celo>>(
        &self,
        provider: &P,
        tx: &CeloTransactionRequest,
        fee_currency: Option<Address>,
    ) -> TransportResult<CeloGasFillable> {
        let gas_limit = match tx.inner.as_ref().gas {
            Some(gas_limit) => gas_limit,
            None => provider.estimate_gas(tx.clone()).await?,
        };

        let preset_max_fee = tx.inner.as_ref().max_fee_per_gas;
        let preset_tip = tx.inner.as_ref().max_priority_fee_per_gas;
        if let (Some(max_fee_per_gas), Some(max_priority_fee_per_gas)) =
            (preset_max_fee, preset_tip)
        {
            return Ok(CeloGasFillable::Cip64 {
                gas_limit,
                max_fee_per_gas,
                max_priority_fee_per_gas,
            });
        }

        // Fee suggestions from the node, denominated in the fee currency when one is set
        // (both methods take an optional feeCurrency parameter on celo-reth and Celo
        // op-geth). A caller-provided field is preserved; only the missing ones are filled —
        // mirroring the node's own `fill_cip64_fee_defaults`.
        //
        // The suggested tip is needed even when the caller set one: `eth_gasPrice
        // [feeCurrency]` returns base fee + suggested tip, so deriving the base fee for the
        // max-fee default requires the node's own tip suggestion, not the caller's.
        let suggested_tip: U256 = match fee_currency {
            Some(fee_currency) => {
                provider.raw_request("eth_maxPriorityFeePerGas".into(), (fee_currency,)).await?
            }
            None => U256::from(provider.get_max_priority_fee_per_gas().await?),
        };

        // A missing tip takes the suggestion, clamped to a caller-provided max fee so it
        // never invalidates the request (tip > max fee).
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
                TransportErrorKind::custom_str(&format!(
                    "fee-currency {field} suggestion {value} overflows u128"
                ))
            })
        };

        Ok(CeloGasFillable::Cip64 {
            gas_limit,
            max_fee_per_gas: to_u128(max_fee, "maxFeePerGas")?,
            max_priority_fee_per_gas: to_u128(tip, "maxPriorityFeePerGas")?,
        })
    }
}

/// Computes the CIP-64 `max_fee_per_gas` suggestion from the node's fee-currency
/// denominated `eth_gasPrice` and `eth_maxPriorityFeePerGas` suggestions plus the tip the
/// transaction will actually use (caller-provided or suggested).
///
/// Mirrors the node's own fee defaults (`2·baseFee + tip`, in fee-currency units):
/// `gasPrice ≈ baseFee + suggestedTip`, so `maxFee = 2·(gasPrice − suggestedTip) + tip`.
/// Falls back to `2·gasPrice` if the node ever reports a tip above its gas price; either
/// way the result never drops below `tip` (a cap under the tip is an invalid transaction).
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
        // The stock rules work for CIP-64 too: finished once gas limit and both EIP-1559
        // fee fields are set.
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
        if tx.is_cip64() {
            // Native-fee CIP-64 (type 0x7b, no fee currency) takes this path too: the
            // stock filler would discard an individually preset fee field.
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
        // Degenerate node response (gasPrice <= suggested tip) plus a caller tip above
        // 2·gasPrice: the cap must not fall below the tip.
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
}
