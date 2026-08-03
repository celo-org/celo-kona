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

/// Fields that cannot coexist with CIP-64, which is EIP-1559-based: `gasPrice` is legacy and
/// `authorizationList` is EIP-7702. Reporting them instead of silently dropping the offending
/// field mirrors celo-reth's `Cip64Conflict` handling.
fn cip64_conflicts(request: &CeloTransactionRequest) -> Vec<&'static str> {
    let mut errors = Vec::new();
    if request.as_ref().gas_price.is_some() {
        errors.push("CIP-64 is not compatible with legacy gasPrice");
    }
    if request.as_ref().authorization_list.is_some() {
        errors.push("CIP-64 feeCurrency is not compatible with EIP-7702 authorizationList");
    }
    errors
}

impl NetworkTransactionBuilder<Celo> for CeloTransactionRequest {
    fn complete_type(&self, ty: CeloTxType) -> Result<(), Vec<&'static str>> {
        match ty {
            CeloTxType::Deposit => Err(vec!["not implemented for deposit tx"]),
            CeloTxType::Cip64 => {
                let mut errors = cip64_conflicts(self);
                // The build path resolves the inner request's preferred type, so readiness
                // must check that type's keys: blob fields make it EIP-4844 (downgraded to
                // the EIP-1559 shape after building), which additionally requires the blob
                // keys. Checking only the EIP-1559 keys would report ready for a request
                // the inner build rejects.
                let inner_ty = match self.as_ref().preferred_type() {
                    TxType::Eip4844 => TxType::Eip4844,
                    _ => TxType::Eip1559,
                };
                if let Err(missing) =
                    NetworkTransactionBuilder::<Ethereum>::complete_type(self.as_ref(), inner_ty)
                {
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
        // Unlike `can_build`, completeness is not required: the node populates the missing
        // nonce and gas fields of an `eth_sendTransaction`. A self-contradicting request is
        // another matter — `prep_for_submission` delegates the trim to the Ethereum impl,
        // which cannot see the fee currency and so leaves the conflicting field in the
        // submitted JSON for the node to reject. Report that here instead, since dropping the
        // field would silently change fees the caller set.
        if self.is_cip64() && !cip64_conflicts(self).is_empty() {
            return false;
        }
        NetworkTransactionBuilder::<Ethereum>::can_submit(self.as_ref())
    }

    fn can_build(&self) -> bool {
        if self.is_cip64() {
            // Same readiness gate as `build_unsigned` and `output_tx_type_checked`: the
            // inner check cannot see the CIP-64 conflicts (`gasPrice`, `authorizationList`).
            return NetworkTransactionBuilder::<Celo>::complete_type(self, CeloTxType::Cip64)
                .is_ok();
        }
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
            // Celo has no EIP-4844; blob-shaped requests build as their EIP-1559
            // downgrade, so report their missing keys against that type instead of
            // panicking on the unrepresentable tx type.
            let tx_type = CeloTxType::try_from(tx_type as u8).unwrap_or(CeloTxType::Eip1559);
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
    fn can_build_gates_on_cip64_readiness() {
        assert!(NetworkTransactionBuilder::<Celo>::can_build(&cip64_request()));

        // A gasPrice conflict makes the inner request a complete legacy shape, but the
        // CIP-64 build would reject it — `can_build` must agree with `build_unsigned`.
        let mut conflicted = cip64_request();
        conflicted.as_mut().gas_price = Some(1);
        assert!(!NetworkTransactionBuilder::<Celo>::can_build(&conflicted));

        let incomplete =
            CeloTransactionRequest::default().to(Address::ZERO).fee_currency(sample_fc());
        assert!(!NetworkTransactionBuilder::<Celo>::can_build(&incomplete));
    }

    #[test]
    fn cip64_incomplete_blob_shape_is_unready_and_errors_cleanly() {
        // Blob fields make the inner request prefer EIP-4844, whose completeness needs
        // the blob keys too. Passing only the EIP-1559 check would report ready for a
        // request the inner build rejects — and panic in `build_unsigned`.
        let mut req = cip64_request();
        req.as_mut().blob_versioned_hashes = Some(vec![alloy_primitives::B256::ZERO]);
        assert!(!NetworkTransactionBuilder::<Celo>::can_build(&req));
        assert_eq!(NetworkTransactionBuilder::<Celo>::output_tx_type_checked(&req), None);

        let err = NetworkTransactionBuilder::<Celo>::build_unsigned(req).unwrap_err();
        let TransactionBuilderError::InvalidTransactionRequest(tx_type, missing) = err.error else {
            panic!("unexpected error: {:?}", err.error);
        };
        assert_eq!(tx_type, CeloTxType::Cip64);
        assert!(missing.iter().any(|k| k.contains("sidecar")), "got: {missing:?}");
    }

    #[test]
    fn cip64_complete_blob_shape_builds_downgraded() {
        // A complete EIP-4844 shape with a fee currency still builds: the blob parts are
        // dropped by the documented 4844 → 1559 downgrade and the result is CIP-64.
        let mut req = cip64_request();
        req.as_mut().sidecar = Some(Default::default());
        req.as_mut().max_fee_per_blob_gas = Some(1);
        assert!(NetworkTransactionBuilder::<Celo>::can_build(&req));

        let tx = NetworkTransactionBuilder::<Celo>::build_unsigned(req).expect("should build");
        let CeloTypedTransaction::Cip64(tx) = tx else {
            panic!("expected CIP-64, got {tx:?}");
        };
        assert_eq!(tx.fee_currency, Some(sample_fc()));
    }

    #[test]
    fn non_cip64_incomplete_blob_shape_errors_cleanly() {
        // Without CIP-64 intent, `missing_keys` reports EIP-4844 — unrepresentable in
        // `CeloTxType`. This must surface as a build error, not a panic.
        let mut req = CeloTransactionRequest::default()
            .to(Address::ZERO)
            .nonce(1)
            .gas_limit(100_000)
            .max_fee_per_gas(2_000_000)
            .max_priority_fee_per_gas(1_000);
        req.as_mut().blob_versioned_hashes = Some(vec![alloy_primitives::B256::ZERO]);

        let err = NetworkTransactionBuilder::<Celo>::build_unsigned(req).unwrap_err();
        let TransactionBuilderError::InvalidTransactionRequest(tx_type, missing) = err.error else {
            panic!("unexpected error: {:?}", err.error);
        };
        assert_eq!(tx_type, CeloTxType::Eip1559);
        assert!(missing.iter().any(|k| k.contains("sidecar")), "got: {missing:?}");
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
    fn can_submit_rejects_cip64_conflicts() {
        // `prep_for_submission` cannot drop the conflicting field without silently changing
        // the fees the caller set, so the contradiction has to surface before submission
        // rather than as a node-side rejection after the fillers have run.
        let mut gas_price = cip64_request().from(Address::repeat_byte(2));
        gas_price.as_mut().gas_price = Some(1);
        assert!(!NetworkTransactionBuilder::<Celo>::can_submit(&gas_price));

        let mut authorization = cip64_request().from(Address::repeat_byte(2));
        authorization.as_mut().authorization_list = Some(vec![]);
        assert!(!NetworkTransactionBuilder::<Celo>::can_submit(&authorization));
    }

    #[test]
    fn can_submit_allows_incomplete_cip64() {
        // The node fills nonce and the gas fields for an unlocked-account
        // `eth_sendTransaction`, so — unlike `can_build` — missing keys must not block
        // submission. Only `from` is required.
        let req = CeloTransactionRequest::default()
            .to(Address::ZERO)
            .fee_currency(sample_fc())
            .from(Address::repeat_byte(2));
        assert!(!NetworkTransactionBuilder::<Celo>::can_build(&req));
        assert!(NetworkTransactionBuilder::<Celo>::can_submit(&req));
    }

    #[test]
    fn can_submit_ignores_conflicts_without_cip64_intent() {
        // A plain legacy request legitimately carries `gasPrice`; the CIP-64 gate must not
        // reject it.
        let mut req =
            CeloTransactionRequest::default().to(Address::ZERO).from(Address::repeat_byte(2));
        req.as_mut().gas_price = Some(1);
        assert!(NetworkTransactionBuilder::<Celo>::can_submit(&req));
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
