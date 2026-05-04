#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

pub use alloy_network::*;

use alloy_consensus::{TxEnvelope, TxType, TypedTransaction};
use alloy_primitives::{Address, Bytes, ChainId, TxKind, U256};
use alloy_rpc_types_eth::AccessList;
use celo_alloy_consensus::{CeloTxEnvelope, CeloTxType, CeloTypedTransaction};
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

impl TransactionBuilder<Celo> for CeloTransactionRequest {
    fn chain_id(&self) -> Option<ChainId> {
        self.as_ref().chain_id()
    }

    fn set_chain_id(&mut self, chain_id: ChainId) {
        self.as_mut().set_chain_id(chain_id);
    }

    fn nonce(&self) -> Option<u64> {
        self.as_ref().nonce()
    }

    fn set_nonce(&mut self, nonce: u64) {
        self.as_mut().set_nonce(nonce);
    }

    fn take_nonce(&mut self) -> Option<u64> {
        self.as_mut().take_nonce()
    }

    fn input(&self) -> Option<&Bytes> {
        self.as_ref().input()
    }

    fn set_input<T: Into<Bytes>>(&mut self, input: T) {
        self.as_mut().set_input(input);
    }

    fn from(&self) -> Option<Address> {
        self.as_ref().from()
    }

    fn set_from(&mut self, from: Address) {
        self.as_mut().set_from(from);
    }

    fn kind(&self) -> Option<TxKind> {
        self.as_ref().kind()
    }

    fn clear_kind(&mut self) {
        self.as_mut().clear_kind();
    }

    fn set_kind(&mut self, kind: TxKind) {
        self.as_mut().set_kind(kind);
    }

    fn value(&self) -> Option<U256> {
        self.as_ref().value()
    }

    fn set_value(&mut self, value: U256) {
        self.as_mut().set_value(value);
    }

    fn gas_price(&self) -> Option<u128> {
        self.as_ref().gas_price()
    }

    fn set_gas_price(&mut self, gas_price: u128) {
        self.as_mut().set_gas_price(gas_price);
    }

    fn max_fee_per_gas(&self) -> Option<u128> {
        self.as_ref().max_fee_per_gas()
    }

    fn set_max_fee_per_gas(&mut self, max_fee_per_gas: u128) {
        self.as_mut().set_max_fee_per_gas(max_fee_per_gas);
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.as_ref().max_priority_fee_per_gas()
    }

    fn set_max_priority_fee_per_gas(&mut self, max_priority_fee_per_gas: u128) {
        self.as_mut().set_max_priority_fee_per_gas(max_priority_fee_per_gas);
    }

    fn gas_limit(&self) -> Option<u64> {
        self.as_ref().gas_limit()
    }

    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.as_mut().set_gas_limit(gas_limit);
    }

    fn access_list(&self) -> Option<&AccessList> {
        self.as_ref().access_list()
    }

    fn set_access_list(&mut self, access_list: AccessList) {
        self.as_mut().set_access_list(access_list);
    }

    fn complete_type(&self, ty: CeloTxType) -> Result<(), Vec<&'static str>> {
        match ty {
            CeloTxType::Deposit => Err(vec!["not implemented for deposit tx"]),
            CeloTxType::Cip64 => Err(vec!["not implemented for CIP-64 tx"]),
            _ => {
                let ty = TxType::try_from(ty as u8).unwrap();
                self.as_ref().complete_type(ty)
            }
        }
    }

    fn can_submit(&self) -> bool {
        self.as_ref().can_submit()
    }

    fn can_build(&self) -> bool {
        self.as_ref().can_build()
    }

    #[doc(alias = "output_transaction_type")]
    fn output_tx_type(&self) -> CeloTxType {
        match self.as_ref().preferred_type() {
            TxType::Eip1559 | TxType::Eip4844 => CeloTxType::Eip1559,
            TxType::Eip2930 => CeloTxType::Eip2930,
            TxType::Eip7702 => CeloTxType::Eip7702,
            TxType::Legacy => CeloTxType::Legacy,
        }
    }

    #[doc(alias = "output_transaction_type_checked")]
    fn output_tx_type_checked(&self) -> Option<CeloTxType> {
        self.as_ref().buildable_type().map(|tx_ty| match tx_ty {
            TxType::Eip1559 | TxType::Eip4844 => CeloTxType::Eip1559,
            TxType::Eip2930 => CeloTxType::Eip2930,
            TxType::Eip7702 => CeloTxType::Eip7702,
            TxType::Legacy => CeloTxType::Legacy,
        })
    }

    fn prep_for_submission(&mut self) {
        self.as_mut().prep_for_submission();
    }

    fn build_unsigned(self) -> BuildResult<CeloTypedTransaction, Celo> {
        if let Err((tx_type, missing)) = self.as_ref().missing_keys() {
            let tx_type = CeloTxType::try_from(tx_type as u8).unwrap();
            return Err(TransactionBuilderError::InvalidTransactionRequest(tx_type, missing)
                .into_unbuilt(self));
        }
        Ok(self.build_typed_tx().expect("checked by missing_keys"))
    }

    async fn build<W: NetworkWallet<Celo>>(
        self,
        wallet: &W,
    ) -> Result<<Celo as Network>::TxEnvelope, TransactionBuilderError<Celo>> {
        Ok(wallet.sign_request(self).await?)
    }
}

impl NetworkWallet<Celo> for EthereumWallet {
    fn default_signer_address(&self) -> Address {
        NetworkWallet::<Ethereum>::default_signer_address(self)
    }

    fn has_signer_for(&self, address: &Address) -> bool {
        NetworkWallet::<Ethereum>::has_signer_for(self, address)
    }

    fn signer_addresses(&self) -> impl Iterator<Item = Address> {
        NetworkWallet::<Ethereum>::signer_addresses(self)
    }

    async fn sign_transaction_from(
        &self,
        sender: Address,
        tx: CeloTypedTransaction,
    ) -> alloy_signer::Result<CeloTxEnvelope> {
        let tx = match tx {
            CeloTypedTransaction::Legacy(tx) => TypedTransaction::Legacy(tx),
            CeloTypedTransaction::Eip2930(tx) => TypedTransaction::Eip2930(tx),
            CeloTypedTransaction::Eip1559(tx) => TypedTransaction::Eip1559(tx),
            CeloTypedTransaction::Eip7702(tx) => TypedTransaction::Eip7702(tx),
            CeloTypedTransaction::Cip64(_) => {
                return Err(alloy_signer::Error::other("not implemented for CIP-64 tx"));
            }
            CeloTypedTransaction::Deposit(_) => {
                return Err(alloy_signer::Error::other("not implemented for deposit tx"));
            }
        };
        let tx = NetworkWallet::<Ethereum>::sign_transaction_from(self, sender, tx).await?;

        Ok(match tx {
            TxEnvelope::Eip1559(tx) => CeloTxEnvelope::Eip1559(tx),
            TxEnvelope::Eip2930(tx) => CeloTxEnvelope::Eip2930(tx),
            TxEnvelope::Eip7702(tx) => CeloTxEnvelope::Eip7702(tx),
            TxEnvelope::Legacy(tx) => CeloTxEnvelope::Legacy(tx),
            _ => unreachable!(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{TxEip1559, TxEip2930, TxEip7702, TxLegacy};
    use alloy_eips::eip2930::AccessListItem;
    use alloy_primitives::{B256, address, b256};
    use alloy_signer_local::PrivateKeySigner;
    use celo_alloy_consensus::TxCip64;
    use celo_alloy_rpc_types::CeloTransactionRequest;
    use op_alloy_consensus::TxDeposit;

    /// Builds a `CeloTransactionRequest` with deliberately distinct, non-default
    /// values for every field accessed by the `TransactionBuilder<Celo>` trait
    /// forwarders. Used to pin each accessor's `-> Default`/`-> None` mutant.
    fn populated_request() -> CeloTransactionRequest {
        let mut req = CeloTransactionRequest::default();
        // Each setter exercises both the `set_*` mutation (`-> ()`) and the
        // mirrored getter assertion below.
        TransactionBuilder::<Celo>::set_chain_id(&mut req, 0xa4ec);
        TransactionBuilder::<Celo>::set_nonce(&mut req, 7);
        TransactionBuilder::<Celo>::set_input(&mut req, Bytes::from_static(&[0xCA, 0xFE]));
        TransactionBuilder::<Celo>::set_from(
            &mut req,
            address!("0x000000000000000000000000000000000000aa01"),
        );
        TransactionBuilder::<Celo>::set_kind(
            &mut req,
            TxKind::Call(address!("0x000000000000000000000000000000000000aa02")),
        );
        TransactionBuilder::<Celo>::set_value(&mut req, U256::from(123_u64));
        TransactionBuilder::<Celo>::set_max_fee_per_gas(&mut req, 0x9999);
        TransactionBuilder::<Celo>::set_max_priority_fee_per_gas(&mut req, 0x77);
        TransactionBuilder::<Celo>::set_gas_limit(&mut req, 21_000);
        TransactionBuilder::<Celo>::set_access_list(
            &mut req,
            AccessList(vec![AccessListItem {
                address: address!("0x000000000000000000000000000000000000aa03"),
                storage_keys: vec![b256!(
                    "0x5555555555555555555555555555555555555555555555555555555555555555"
                )],
            }]),
        );
        req
    }

    /// Pins the `chain_id`, `nonce`, `input`, `from`, `kind`, `value`,
    /// `max_fee_per_gas`, `max_priority_fee_per_gas`, `gas_limit`,
    /// `access_list` getters AND their `set_*` siblings against
    /// constant-replacement / no-op mutants in one shot.
    #[test]
    fn transaction_builder_round_trips_every_field() {
        let req = populated_request();
        assert_eq!(TransactionBuilder::<Celo>::chain_id(&req), Some(0xa4ec));
        assert_eq!(TransactionBuilder::<Celo>::nonce(&req), Some(7));
        assert_eq!(
            TransactionBuilder::<Celo>::input(&req),
            Some(&Bytes::from_static(&[0xCA, 0xFE])),
        );
        assert_eq!(
            TransactionBuilder::<Celo>::from(&req),
            Some(address!("0x000000000000000000000000000000000000aa01")),
        );
        assert_eq!(
            TransactionBuilder::<Celo>::kind(&req),
            Some(TxKind::Call(address!("0x000000000000000000000000000000000000aa02"))),
        );
        assert_eq!(TransactionBuilder::<Celo>::value(&req), Some(U256::from(123_u64)));
        assert_eq!(TransactionBuilder::<Celo>::max_fee_per_gas(&req), Some(0x9999));
        assert_eq!(TransactionBuilder::<Celo>::max_priority_fee_per_gas(&req), Some(0x77));
        assert_eq!(TransactionBuilder::<Celo>::gas_limit(&req), Some(21_000));
        let list = TransactionBuilder::<Celo>::access_list(&req).expect("Some list");
        assert_eq!(list.len(), 1);
    }

    /// Pins `take_nonce` against the constant-replacement and `-> None` mutants.
    #[test]
    fn transaction_builder_take_nonce_drains_field() {
        let mut req = populated_request();
        assert_eq!(TransactionBuilder::<Celo>::take_nonce(&mut req), Some(7));
        assert_eq!(TransactionBuilder::<Celo>::nonce(&req), None);
    }

    /// Pins the `clear_kind -> ()` mutant by asserting the field becomes None
    /// after clearing.
    #[test]
    fn transaction_builder_clear_kind_drops_recipient() {
        let mut req = populated_request();
        TransactionBuilder::<Celo>::clear_kind(&mut req);
        assert_eq!(TransactionBuilder::<Celo>::kind(&req), None);
    }

    /// Pins `gas_price` getter / setter (`-> Some(0|1)`, `-> None`, `set -> ()`).
    /// CIP-64 doesn't carry a legacy gas_price, but the request struct stores it
    /// independently — round-trip a non-trivial value.
    #[test]
    fn transaction_builder_gas_price_round_trips() {
        let mut req = CeloTransactionRequest::default();
        TransactionBuilder::<Celo>::set_gas_price(&mut req, 12_345_u128);
        assert_eq!(TransactionBuilder::<Celo>::gas_price(&req), Some(12_345_u128));
    }

    /// Pins both `complete_type -> Ok(())` (rejects CIP-64/Deposit) and the
    /// `delete match arm` mutants for those two cases. With either match arm
    /// removed the call would fall through to the inner `complete_type` and
    /// either fail with a different error message or succeed (depending on
    /// inner logic), so asserting the specific error string suffices.
    #[test]
    fn transaction_builder_complete_type_rejects_cip64() {
        let req = CeloTransactionRequest::default();
        let err = TransactionBuilder::<Celo>::complete_type(&req, CeloTxType::Cip64)
            .expect_err("CIP-64 must not be buildable via this trait");
        assert_eq!(err, vec!["not implemented for CIP-64 tx"]);
    }

    #[test]
    fn transaction_builder_complete_type_rejects_deposit() {
        let req = CeloTransactionRequest::default();
        let err = TransactionBuilder::<Celo>::complete_type(&req, CeloTxType::Deposit)
            .expect_err("Deposit must not be buildable via this trait");
        assert_eq!(err, vec!["not implemented for deposit tx"]);
    }

    /// EIP-1559 is a buildable type; `complete_type` should return `Ok(())`
    /// after the specific match arms fall through to the inner builder. Pins
    /// the `complete_type -> Ok(())` mutation — without the explicit match
    /// arms, the wrapper itself might short-circuit but the buildable case
    /// still needs to succeed.
    #[test]
    fn transaction_builder_complete_type_accepts_eip1559_when_built() {
        let mut req = CeloTransactionRequest::default();
        TransactionBuilder::<Celo>::set_chain_id(&mut req, 0xa4ec);
        TransactionBuilder::<Celo>::set_nonce(&mut req, 1);
        TransactionBuilder::<Celo>::set_max_fee_per_gas(&mut req, 100);
        TransactionBuilder::<Celo>::set_max_priority_fee_per_gas(&mut req, 1);
        TransactionBuilder::<Celo>::set_gas_limit(&mut req, 21_000);
        TransactionBuilder::<Celo>::set_kind(
            &mut req,
            TxKind::Call(address!("0x0000000000000000000000000000000000001111")),
        );
        TransactionBuilder::<Celo>::set_value(&mut req, U256::ZERO);
        assert!(TransactionBuilder::<Celo>::complete_type(&req, CeloTxType::Eip1559).is_ok());
    }

    /// Pins `can_submit -> true|false` by asserting both branches.
    #[test]
    fn transaction_builder_can_submit_distinguishes_complete_and_incomplete() {
        let req = CeloTransactionRequest::default();
        // A default request has no signer/from set.
        assert!(!TransactionBuilder::<Celo>::can_submit(&req));
        let req = populated_request();
        assert!(TransactionBuilder::<Celo>::can_submit(&req));
    }

    /// Pins `can_build -> true|false`.
    #[test]
    fn transaction_builder_can_build_distinguishes_complete_and_incomplete() {
        let req = CeloTransactionRequest::default();
        assert!(!TransactionBuilder::<Celo>::can_build(&req));
        let req = populated_request();
        assert!(TransactionBuilder::<Celo>::can_build(&req));
    }

    /// Pins `output_tx_type -> Default` and the inner-type mapping.
    #[test]
    fn transaction_builder_output_tx_type_maps_eip1559() {
        let mut req = CeloTransactionRequest::default();
        TransactionBuilder::<Celo>::set_max_fee_per_gas(&mut req, 1);
        // Default's `preferred_type` for a tx with max_fee_per_gas is Eip1559.
        assert_eq!(TransactionBuilder::<Celo>::output_tx_type(&req), CeloTxType::Eip1559);
    }

    /// Pins `output_tx_type_checked -> None | Some(Default)`.
    #[test]
    fn transaction_builder_output_tx_type_checked_returns_some_for_buildable() {
        let req = populated_request();
        assert_eq!(
            TransactionBuilder::<Celo>::output_tx_type_checked(&req),
            Some(CeloTxType::Eip1559),
        );
    }

    /// Pins `prep_for_submission -> ()`. The Ethereum impl sets
    /// `transaction_type` to the preferred type when missing — a default
    /// request leaves it as `None`, but after prep it must be `Some(_)`.
    #[test]
    fn transaction_builder_prep_for_submission_sets_transaction_type() {
        let mut req = populated_request();
        assert!(req.as_ref().transaction_type.is_none(), "precondition: type unset");
        TransactionBuilder::<Celo>::prep_for_submission(&mut req);
        assert!(
            req.as_ref().transaction_type.is_some(),
            "prep_for_submission must populate transaction_type",
        );
    }

    /// Pins `build_unsigned -> Default`. A valid populated request should
    /// produce a `CeloTypedTransaction::Eip1559` with the chain id we set.
    #[test]
    fn transaction_builder_build_unsigned_returns_typed_tx() {
        let req = populated_request();
        let typed = TransactionBuilder::<Celo>::build_unsigned(req).expect("build_unsigned");
        match typed {
            CeloTypedTransaction::Eip1559(tx) => assert_eq!(tx.chain_id, 0xa4ec),
            other => panic!("expected Eip1559, got {other:?}"),
        }
    }

    /// Pins `build_unsigned -> Default` from the failure side: an incomplete
    /// request should return Err. The `-> Default` mutation would return
    /// `Ok(Default::default())` instead.
    #[test]
    fn transaction_builder_build_unsigned_errors_for_incomplete() {
        let req = CeloTransactionRequest::default();
        TransactionBuilder::<Celo>::build_unsigned(req)
            .expect_err("default request is not buildable");
    }

    fn local_wallet() -> EthereumWallet {
        let signer = PrivateKeySigner::from_bytes(&b256!(
            "0x1111111111111111111111111111111111111111111111111111111111111111"
        ))
        .expect("valid scalar");
        EthereumWallet::new(signer)
    }

    /// Pins the four `NetworkWallet::<Ethereum>::*` forwarders:
    /// `default_signer_address`, `has_signer_for`, `signer_addresses`. Builds
    /// a wallet with one local signer and asserts each accessor.
    #[test]
    fn network_wallet_forwards_to_ethereum() {
        let wallet = local_wallet();
        let addr = NetworkWallet::<Celo>::default_signer_address(&wallet);
        assert_ne!(addr, Address::ZERO);
        assert!(NetworkWallet::<Celo>::has_signer_for(&wallet, &addr));
        assert!(!NetworkWallet::<Celo>::has_signer_for(&wallet, &Address::ZERO));
        let addrs: Vec<_> = NetworkWallet::<Celo>::signer_addresses(&wallet).collect();
        assert_eq!(addrs, vec![addr]);
    }

    /// Pins `sign_transaction_from -> Ok(Default)` AND each
    /// `delete TxEnvelope::* match arm` (the post-sign re-wrapping). For each
    /// of the four wrap-able tx types (Legacy/Eip2930/Eip1559/Eip7702), build
    /// a typed tx, sign it, and assert the result is the expected
    /// CeloTxEnvelope variant carrying the chain id we set.
    #[tokio::test]
    async fn sign_transaction_from_round_trips_every_supported_variant() {
        let wallet = local_wallet();
        let from = NetworkWallet::<Celo>::default_signer_address(&wallet);

        let to = address!("0x000000000000000000000000000000000000bb01");

        let cases: Vec<(&str, CeloTypedTransaction)> = vec![
            (
                "legacy",
                CeloTypedTransaction::Legacy(TxLegacy {
                    chain_id: Some(0xa4ec),
                    nonce: 1,
                    gas_price: 1,
                    gas_limit: 21_000,
                    to: TxKind::Call(to),
                    value: U256::ZERO,
                    input: Bytes::new(),
                }),
            ),
            (
                "eip2930",
                CeloTypedTransaction::Eip2930(TxEip2930 {
                    chain_id: 0xa4ec,
                    nonce: 1,
                    gas_price: 1,
                    gas_limit: 21_000,
                    to: TxKind::Call(to),
                    value: U256::ZERO,
                    access_list: AccessList::default(),
                    input: Bytes::new(),
                }),
            ),
            (
                "eip1559",
                CeloTypedTransaction::Eip1559(TxEip1559 {
                    chain_id: 0xa4ec,
                    nonce: 1,
                    gas_limit: 21_000,
                    max_fee_per_gas: 100,
                    max_priority_fee_per_gas: 1,
                    to: TxKind::Call(to),
                    value: U256::ZERO,
                    access_list: AccessList::default(),
                    input: Bytes::new(),
                }),
            ),
            (
                "eip7702",
                CeloTypedTransaction::Eip7702(TxEip7702 {
                    chain_id: 0xa4ec,
                    nonce: 1,
                    gas_limit: 21_000,
                    max_fee_per_gas: 100,
                    max_priority_fee_per_gas: 1,
                    to,
                    value: U256::ZERO,
                    access_list: AccessList::default(),
                    authorization_list: vec![],
                    input: Bytes::new(),
                }),
            ),
        ];

        for (label, tx) in cases {
            let signed = NetworkWallet::<Celo>::sign_transaction_from(&wallet, from, tx)
                .await
                .unwrap_or_else(|e| panic!("{label} should sign: {e:?}"));
            // Each arm must dispatch to its matching variant.
            let kind = match label {
                "legacy" => matches!(signed, CeloTxEnvelope::Legacy(_)),
                "eip2930" => matches!(signed, CeloTxEnvelope::Eip2930(_)),
                "eip1559" => matches!(signed, CeloTxEnvelope::Eip1559(_)),
                "eip7702" => matches!(signed, CeloTxEnvelope::Eip7702(_)),
                _ => unreachable!(),
            };
            assert!(kind, "{label}: variant mismatch on {signed:?}");
        }
    }

    /// Pins the CIP-64 / Deposit error branches in `sign_transaction_from`.
    /// Without these branches the function would attempt to sign as a
    /// supported alloy variant (and fail, but with a different error) — the
    /// `Ok(Default)` mutation would silently succeed with a default envelope.
    #[tokio::test]
    async fn sign_transaction_from_errors_on_cip64_and_deposit() {
        let wallet = local_wallet();
        let from = NetworkWallet::<Celo>::default_signer_address(&wallet);

        let cip64 = CeloTypedTransaction::Cip64(TxCip64 {
            chain_id: 0xa4ec,
            nonce: 1,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Bytes::new(),
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 1,
            access_list: AccessList::default(),
            fee_currency: None,
        });
        let err = NetworkWallet::<Celo>::sign_transaction_from(&wallet, from, cip64)
            .await
            .expect_err("CIP-64 must not be signable here");
        assert!(err.to_string().contains("CIP-64"), "got: {err}");

        let deposit = CeloTypedTransaction::Deposit(TxDeposit {
            source_hash: B256::ZERO,
            from,
            to: TxKind::Call(Address::ZERO),
            mint: 0,
            value: U256::ZERO,
            gas_limit: 21_000,
            is_system_transaction: false,
            input: Bytes::new(),
        });
        let err = NetworkWallet::<Celo>::sign_transaction_from(&wallet, from, deposit)
            .await
            .expect_err("Deposit must not be signable here");
        assert!(err.to_string().contains("deposit"), "got: {err}");
    }
}
