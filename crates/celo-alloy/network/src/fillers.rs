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
    /// A CIP-64 transaction; the fee values are denominated in the fee currency.
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
/// For native-fee requests this behaves exactly like the stock [`GasFiller`]. For CIP-64
/// requests (`fee_currency` set) the fee fields are denominated in the fee currency, so the
/// native fee estimates would be wrong by the currency's exchange rate; instead the fees are
/// fetched with the fee-currency-parameterized `eth_gasPrice` and `eth_maxPriorityFeePerGas`
/// methods that Celo nodes expose.
#[derive(Clone, Debug, Default)]
pub struct CeloGasFiller {
    inner: GasFiller,
}

impl CeloGasFiller {
    async fn prepare_cip64<P: Provider<Celo>>(
        &self,
        provider: &P,
        tx: &CeloTransactionRequest,
        fee_currency: Address,
    ) -> TransportResult<CeloGasFillable> {
        let gas_limit = match tx.inner.as_ref().gas {
            Some(gas_limit) => gas_limit,
            None => provider.estimate_gas(tx.clone()).await?,
        };

        if let (Some(max_fee_per_gas), Some(max_priority_fee_per_gas)) =
            (tx.inner.as_ref().max_fee_per_gas, tx.inner.as_ref().max_priority_fee_per_gas)
        {
            return Ok(CeloGasFillable::Cip64 {
                gas_limit,
                max_fee_per_gas,
                max_priority_fee_per_gas,
            });
        }

        // Fee-currency-denominated suggestions from the node. `eth_gasPrice [feeCurrency]`
        // returns base fee + tip scaled to the fee currency, `eth_maxPriorityFeePerGas
        // [feeCurrency]` the scaled tip (both supported by celo-reth and Celo op-geth).
        let tip: U256 =
            provider.raw_request("eth_maxPriorityFeePerGas".into(), (fee_currency,)).await?;
        let price: U256 = provider.raw_request("eth_gasPrice".into(), (fee_currency,)).await?;

        let max_fee = cip64_max_fee(price, tip);

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
/// denominated `eth_gasPrice` and `eth_maxPriorityFeePerGas` suggestions.
///
/// Mirrors the node's own fee defaults (`2·baseFee + tip`, in fee-currency units):
/// `gasPrice ≈ baseFee + tip`, so `maxFee = 2·(gasPrice − tip) + tip`. Falls back to
/// `2·gasPrice` if the node ever reports a tip above its gas price.
fn cip64_max_fee(price: U256, tip: U256) -> U256 {
    if price > tip {
        (price - tip).saturating_mul(U256::from(2)).saturating_add(tip)
    } else {
        price.saturating_mul(U256::from(2))
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
        match tx.fee_currency {
            Some(fee_currency) => self.prepare_cip64(provider, tx, fee_currency).await,
            None => TxFiller::<Celo>::prepare(&self.inner, provider, tx)
                .await
                .map(CeloGasFillable::Native),
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

    #[test]
    fn max_fee_doubles_base_fee_and_adds_tip() {
        // gasPrice = baseFee (100) + tip (7) => maxFee = 2*100 + 7.
        assert_eq!(cip64_max_fee(U256::from(107), U256::from(7)), U256::from(207));
    }

    #[test]
    fn max_fee_with_zero_tip_doubles_gas_price() {
        assert_eq!(cip64_max_fee(U256::from(100), U256::ZERO), U256::from(200));
    }

    #[test]
    fn max_fee_falls_back_when_tip_exceeds_gas_price() {
        // Nonsensical node response (tip > gasPrice): still produce a usable cap.
        assert_eq!(cip64_max_fee(U256::from(5), U256::from(9)), U256::from(10));
        // tip == gasPrice hits the same fallback.
        assert_eq!(cip64_max_fee(U256::from(5), U256::from(5)), U256::from(10));
    }

    #[test]
    fn max_fee_saturates_instead_of_overflowing() {
        let max = U256::MAX;
        assert_eq!(cip64_max_fee(max, U256::ZERO), max);
        assert_eq!(cip64_max_fee(max, U256::from(1)), max);
    }
}
