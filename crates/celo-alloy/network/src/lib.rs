#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

pub use alloy_network::*;

mod fillers;
pub use fillers::{CeloGasFillable, CeloGasFiller};

use alloy_consensus::TxType;
use celo_alloy_consensus::{CeloTxType, CeloTypedTransaction};
use celo_alloy_rpc_types::CeloTransactionRequest;

/// Types for Celo network.
#[derive(Clone, Copy, Debug)]
pub struct Celo {
    _private: (),
}

impl Network for Celo {
    type TxType = CeloTxType;

    type TxEnvelope = celo_alloy_consensus::CeloTxEnvelope;

    type UnsignedTx = celo_alloy_consensus::CeloTypedTransaction;

    type ReceiptEnvelope = celo_alloy_consensus::CeloReceiptEnvelope;

    type Header = alloy_consensus::Header;

    type TransactionRequest = celo_alloy_rpc_types::CeloTransactionRequest;

    type TransactionResponse = celo_alloy_rpc_types::CeloTransaction;

    type ReceiptResponse = celo_alloy_rpc_types::CeloTransactionReceipt;

    type HeaderResponse = alloy_rpc_types_eth::Header;

    type BlockResponse =
        alloy_rpc_types_eth::Block<Self::TransactionResponse, Self::HeaderResponse>;
}

impl NetworkTransactionBuilder<Celo> for CeloTransactionRequest {
    fn complete_type(&self, ty: CeloTxType) -> Result<(), Vec<&'static str>> {
        match ty {
            CeloTxType::Deposit => Err(vec!["not implemented for deposit tx"]),
            CeloTxType::Cip64 => {
                // CIP-64 is EIP-1559-based: it needs the EIP-1559 keys, and `gasPrice` /
                // `authorizationList` conflict with a fee currency. Rejecting the conflicts
                // instead of silently dropping the fields mirrors celo-reth's
                // `Cip64Conflict` handling.
                let mut errors = Vec::new();
                if self.as_ref().gas_price.is_some() {
                    errors.push("CIP-64 is not compatible with legacy gasPrice");
                }
                if self.as_ref().authorization_list.is_some() {
                    errors.push(
                        "CIP-64 feeCurrency is not compatible with EIP-7702 authorizationList",
                    );
                }
                if let Err(missing) = NetworkTransactionBuilder::<Ethereum>::complete_type(
                    self.as_ref(),
                    TxType::Eip1559,
                ) {
                    errors.extend(missing);
                }
                if errors.is_empty() { Ok(()) } else { Err(errors) }
            }
            _ => {
                let ty = TxType::try_from(ty as u8).unwrap();
                NetworkTransactionBuilder::<Ethereum>::complete_type(self.as_ref(), ty)
            }
        }
    }

    fn can_submit(&self) -> bool {
        NetworkTransactionBuilder::<Ethereum>::can_submit(self.as_ref())
    }

    fn can_build(&self) -> bool {
        NetworkTransactionBuilder::<Ethereum>::can_build(self.as_ref())
    }

    #[doc(alias = "output_transaction_type")]
    fn output_tx_type(&self) -> CeloTxType {
        if self.is_cip64() {
            return CeloTxType::Cip64;
        }
        match NetworkTransactionBuilder::<Ethereum>::output_tx_type(self.as_ref()) {
            TxType::Eip1559 | TxType::Eip4844 => CeloTxType::Eip1559,
            TxType::Eip2930 => CeloTxType::Eip2930,
            TxType::Eip7702 => CeloTxType::Eip7702,
            TxType::Legacy => CeloTxType::Legacy,
        }
    }

    #[doc(alias = "output_transaction_type_checked")]
    fn output_tx_type_checked(&self) -> Option<CeloTxType> {
        if self.is_cip64() {
            // The checked variant must return `None` unless the builder is ready to build
            // (same readiness gate as `build_unsigned`).
            return NetworkTransactionBuilder::<Celo>::complete_type(self, CeloTxType::Cip64)
                .is_ok()
                .then_some(CeloTxType::Cip64);
        }
        NetworkTransactionBuilder::<Ethereum>::output_tx_type_checked(self.as_ref()).map(|tx_ty| {
            match tx_ty {
                TxType::Eip1559 | TxType::Eip4844 => CeloTxType::Eip1559,
                TxType::Eip2930 => CeloTxType::Eip2930,
                TxType::Eip7702 => CeloTxType::Eip7702,
                TxType::Legacy => CeloTxType::Legacy,
            }
        })
    }

    fn prep_for_submission(&mut self) {
        // Capture before delegating: the Ethereum impl overwrites `transaction_type` with
        // its preferred type, which would erase an explicit CIP-64 tag.
        let is_cip64 = self.is_cip64();
        NetworkTransactionBuilder::<Ethereum>::prep_for_submission(self.as_mut());
        if is_cip64 {
            self.as_mut().transaction_type = Some(CeloTxType::Cip64 as u8);
        }
    }

    fn build_unsigned(self) -> BuildResult<CeloTypedTransaction, Celo> {
        if self.is_cip64() {
            // `missing_keys` runs on the inner request and cannot see the fee currency, so
            // the CIP-64 checks (EIP-1559 keys + conflict rejection) live in
            // `complete_type`.
            if let Err(errors) =
                NetworkTransactionBuilder::<Celo>::complete_type(&self, CeloTxType::Cip64)
            {
                return Err(TransactionBuilderError::InvalidTransactionRequest(
                    CeloTxType::Cip64,
                    errors,
                )
                .into_unbuilt(self));
            }
        } else if let Err((tx_type, missing)) = self.as_ref().missing_keys() {
            let tx_type = CeloTxType::try_from(tx_type as u8).unwrap();
            return Err(TransactionBuilderError::InvalidTransactionRequest(tx_type, missing)
                .into_unbuilt(self));
        }
        Ok(self.build_typed_tx().expect("checked by complete_type/missing_keys"))
    }

    async fn build<W: NetworkWallet<Celo>>(
        self,
        wallet: &W,
    ) -> Result<<Celo as Network>::TxEnvelope, TransactionBuilderError<Celo>> {
        Ok(wallet.sign_request(self).await?)
    }
}

// `NetworkWallet<Celo> for EthereumWallet` is provided by alloy-network's blanket impl
// `impl<N: Network> NetworkWallet<N> for EthereumWallet where N::TxEnvelope:
//   From<Signed<N::UnsignedTx>>, N::UnsignedTx: SignableTransaction<Signature>`.
// Both bounds are satisfied by `Celo` (see celo-alloy-consensus).

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256, address};

    fn sample_fc() -> Address {
        address!("0x765DE816845861e75A25fCA122bb6898B8B1282a")
    }

    fn cip64_request() -> CeloTransactionRequest {
        CeloTransactionRequest::default()
            .to(Address::ZERO)
            .value(U256::from(1u64))
            .nonce(1)
            .gas_limit(100_000)
            .max_fee_per_gas(2_000_000)
            .max_priority_fee_per_gas(1_000)
            .fee_currency(sample_fc())
    }

    #[test]
    fn output_tx_type_is_cip64_when_fee_currency_set() {
        let req = cip64_request();
        assert_eq!(NetworkTransactionBuilder::<Celo>::output_tx_type(&req), CeloTxType::Cip64);
        assert_eq!(
            NetworkTransactionBuilder::<Celo>::output_tx_type_checked(&req),
            Some(CeloTxType::Cip64)
        );
    }

    #[test]
    fn output_tx_type_checked_is_none_for_incomplete_cip64() {
        // The checked variant must not claim readiness: fee/gas keys are missing here.
        let req = CeloTransactionRequest::default().to(Address::ZERO).fee_currency(sample_fc());
        assert_eq!(NetworkTransactionBuilder::<Celo>::output_tx_type_checked(&req), None);
        // The unchecked variant still reports the type that would be attempted.
        assert_eq!(NetworkTransactionBuilder::<Celo>::output_tx_type(&req), CeloTxType::Cip64);
    }

    #[test]
    fn output_tx_type_checked_is_none_for_conflicted_cip64() {
        let mut req = cip64_request();
        req.as_mut().gas_price = Some(1);
        assert_eq!(NetworkTransactionBuilder::<Celo>::output_tx_type_checked(&req), None);
    }

    #[test]
    fn output_tx_type_without_fee_currency_is_eip1559() {
        let req = CeloTransactionRequest::default().to(Address::ZERO);
        assert_eq!(NetworkTransactionBuilder::<Celo>::output_tx_type(&req), CeloTxType::Eip1559);
    }

    #[test]
    fn complete_type_cip64_accepts_complete_request() {
        let req = cip64_request();
        NetworkTransactionBuilder::<Celo>::complete_type(&req, CeloTxType::Cip64)
            .expect("complete CIP-64 request should pass");
    }

    #[test]
    fn complete_type_cip64_rejects_gas_price_conflict() {
        let mut req = cip64_request();
        req.as_mut().gas_price = Some(1);
        let errors =
            NetworkTransactionBuilder::<Celo>::complete_type(&req, CeloTxType::Cip64).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("gasPrice")), "got: {errors:?}");
    }

    #[test]
    fn complete_type_cip64_rejects_authorization_list_conflict() {
        let mut req = cip64_request();
        req.as_mut().authorization_list = Some(vec![]);
        let errors =
            NetworkTransactionBuilder::<Celo>::complete_type(&req, CeloTxType::Cip64).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("authorizationList")), "got: {errors:?}");
    }

    #[test]
    fn complete_type_cip64_reports_missing_1559_keys() {
        let req = CeloTransactionRequest::default().to(Address::ZERO).fee_currency(sample_fc());
        let errors =
            NetworkTransactionBuilder::<Celo>::complete_type(&req, CeloTxType::Cip64).unwrap_err();
        assert!(!errors.is_empty());
    }

    #[test]
    fn prep_for_submission_stamps_cip64_type() {
        let mut req = cip64_request();
        NetworkTransactionBuilder::<Celo>::prep_for_submission(&mut req);
        assert_eq!(req.as_ref().transaction_type, Some(CeloTxType::Cip64 as u8));
    }

    #[test]
    fn build_unsigned_produces_cip64() {
        let tx = NetworkTransactionBuilder::<Celo>::build_unsigned(cip64_request())
            .expect("should build");
        let CeloTypedTransaction::Cip64(tx) = tx else {
            panic!("expected CIP-64, got {tx:?}");
        };
        assert_eq!(tx.fee_currency, Some(sample_fc()));
    }

    #[test]
    fn build_unsigned_errors_carry_cip64_type() {
        // Missing fee/gas keys with a fee currency set must be reported as a CIP-64 error.
        let req = CeloTransactionRequest::default().to(Address::ZERO).fee_currency(sample_fc());
        let err = NetworkTransactionBuilder::<Celo>::build_unsigned(req).unwrap_err();
        let TransactionBuilderError::InvalidTransactionRequest(tx_type, _) = err.error else {
            panic!("unexpected error: {:?}", err.error);
        };
        assert_eq!(tx_type, CeloTxType::Cip64);
    }

    #[tokio::test]
    async fn wallet_signs_cip64_envelope() {
        use alloy_signer_local::PrivateKeySigner;

        let signer = PrivateKeySigner::random();
        let sender = signer.address();
        let wallet = EthereumWallet::new(signer);

        let req = cip64_request().from(sender);
        let envelope = NetworkTransactionBuilder::<Celo>::build(req, &wallet)
            .await
            .expect("wallet should sign CIP-64");

        let celo_alloy_consensus::CeloTxEnvelope::Cip64(signed) = &envelope else {
            panic!("expected CIP-64 envelope");
        };
        assert_eq!(signed.tx().fee_currency, Some(sample_fc()));
        assert_eq!(signed.recover_signer().expect("valid signature"), sender);
    }
}
