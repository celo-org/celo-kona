//! Sends a CIP-64 (fee-currency) transaction through the `Celo` network provider.
//!
//! This is the reference recipe for external Rust clients: with the `Celo` network and
//! its recommended fillers, setting `fee_currency` on the request is all that's needed —
//! nonce, chain id, gas limit (including the CIP-64 intrinsic surcharge) and the
//! fee-currency-denominated fee fields are filled automatically, and the wallet signs a
//! type-`0x7b` transaction.
//!
//! Environment:
//! - `ETH_RPC_URL`     RPC endpoint of a Celo node (default: `http://127.0.0.1:8545`)
//! - `ACC_PRIVKEY`     hex private key of a funded account (must hold the fee currency)
//! - `FEE_CURRENCY`    address of a whitelisted fee currency
//! - `TO`              recipient (default: the sender itself)
//! - `VALUE`           native value in wei to transfer (default: 1)
//!
//! Run against the e2e dev node: `e2e_test/test_rust_client_cip64.sh`.

use alloy_consensus::Transaction as _;
use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use celo_alloy_consensus::CeloReceiptEnvelope;
use celo_alloy_network::{Celo, EthereumWallet, ReceiptResponse};
use celo_alloy_rpc_types::CeloTransactionRequest;
use std::error::Error;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let rpc_url =
        std::env::var("ETH_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8545".to_string());
    let signer: PrivateKeySigner = std::env::var("ACC_PRIVKEY")?.parse()?;
    let fee_currency: Address = std::env::var("FEE_CURRENCY")?.parse()?;
    let to: Address = match std::env::var("TO") {
        Ok(to) => to.parse()?,
        Err(_) => signer.address(),
    };
    let value: U256 = match std::env::var("VALUE") {
        Ok(value) => value.parse()?,
        Err(_) => U256::from(1u64),
    };

    let sender = signer.address();
    let wallet = EthereumWallet::new(signer);
    let provider =
        ProviderBuilder::new_with_network::<Celo>().wallet(wallet).connect(&rpc_url).await?;

    let request = CeloTransactionRequest::default()
        .from(sender)
        .to(to)
        .value(value)
        .fee_currency(fee_currency);

    let pending = provider.send_transaction(request).await?;
    let tx_hash = *pending.tx_hash();
    println!("sent CIP-64 tx: {tx_hash}");

    let receipt = pending.get_receipt().await?;
    println!("mined in block {:?}, status: {}", receipt.block_number(), receipt.status());

    // The receipt must be a CIP-64 receipt with a fee-currency base fee.
    let CeloReceiptEnvelope::Cip64(cip64) = &receipt.inner.inner else {
        return Err(format!("expected a CIP-64 receipt, got {:?}", receipt.inner.inner).into());
    };
    if !receipt.status() {
        return Err("transaction reverted".into());
    }
    let base_fee =
        cip64.receipt.base_fee.ok_or("CIP-64 receipt is missing the fee-currency baseFee")?;
    println!("fee-currency baseFee: {base_fee}");

    // The submitted transaction must report the fee currency back over RPC.
    let tx = provider
        .get_transaction_by_hash(tx_hash)
        .await?
        .ok_or("transaction not found after mining")?;
    if tx.fee_currency() != Some(fee_currency) {
        return Err(
            format!("expected feeCurrency {fee_currency}, got {:?}", tx.fee_currency()).into()
        );
    }
    println!("feeCurrency: {fee_currency}");

    // The filler must take its suggestions from the fee-currency-parameterized `eth_gasPrice`
    // and `eth_maxPriorityFeePerGas`; filling from the native ones instead would be off by the
    // currency's exchange rate. Correct filling caps at `2·baseFee + tip` in fee-currency
    // units, so the cap is at least twice the receipt's fee-currency base fee, while native
    // suggestions land near the base fee itself. Requiring 1.5x separates the two and still
    // tolerates a base fee that rose between filling and inclusion.
    let max_fee = tx.max_fee_per_gas();
    if max_fee.saturating_mul(2) < base_fee.saturating_mul(3) {
        return Err(format!(
            "maxFeePerGas {max_fee} is too close to the fee-currency baseFee {base_fee}; \
             the fees look native-denominated"
        )
        .into());
    }
    println!("maxFeePerGas: {max_fee} (fee-currency units)");

    Ok(())
}
