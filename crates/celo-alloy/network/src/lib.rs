#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

pub use alloy_network::*;

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
            CeloTxType::Cip64 => Err(vec!["not implemented for CIP-64 tx"]),
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
        match NetworkTransactionBuilder::<Ethereum>::output_tx_type(self.as_ref()) {
            TxType::Eip1559 | TxType::Eip4844 => CeloTxType::Eip1559,
            TxType::Eip2930 => CeloTxType::Eip2930,
            TxType::Eip7702 => CeloTxType::Eip7702,
            TxType::Legacy => CeloTxType::Legacy,
        }
    }

    #[doc(alias = "output_transaction_type_checked")]
    fn output_tx_type_checked(&self) -> Option<CeloTxType> {
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
        NetworkTransactionBuilder::<Ethereum>::prep_for_submission(self.as_mut());
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

// `NetworkWallet<Celo> for EthereumWallet` is provided by alloy-network's blanket impl
// `impl<N: Network> NetworkWallet<N> for EthereumWallet where N::TxEnvelope:
//   From<Signed<N::UnsignedTx>>, N::UnsignedTx: SignableTransaction<Signature>`.
// Both bounds are satisfied by `Celo` (see celo-alloy-consensus).
