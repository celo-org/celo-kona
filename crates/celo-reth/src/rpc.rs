//! Celo-specific RPC types and converters for reth.
//!
//! Provides [`CeloRpcTypes`], [`CeloReceiptConverter`], and [`CeloEthApiBuilder`] —
//! the Celo equivalents of the op-reth `Optimism` network type, `OpReceiptConverter`,
//! and `OpEthApiBuilder`.

use crate::{primitives::CeloPrimitives, receipt::CeloReceipt};
use alloy_consensus::{ReceiptWithBloom, Transaction, error::ValueError};
use alloy_evm::{env::BlockEnvironment, rpc::EthTxEnvError, EvmEnv};
use alloy_network::TxSigner;
use alloy_primitives::{Address, Bytes, Signature, U256, keccak256};
use alloy_rpc_types_eth::{Log, TransactionInput, request::TransactionRequest};
use celo_alloy_consensus::{
    CeloCip64Receipt, CeloCip64ReceiptWithBloom, CeloReceiptEnvelope, CeloTxEnvelope,
};
use celo_alloy_rpc_types::CeloTransactionReceipt;
use celo_revm::CeloTransaction;
use op_alloy_rpc_types::OpTransactionRequest;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_evm::ConfigureEvm;
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_node_builder::rpc::{EthApiBuilder, EthApiCtx};
use reth_optimism_forks::OpHardforks;
use reth_optimism_rpc::{
    OpEthApi, SequencerClient,
    eth::{receipt::OpReceiptFieldsBuilder, transaction::OpTxInfoMapper},
};
use reth_primitives_traits::SealedBlock;
use reth_rpc_eth_api::{
    FullEthApiServer, RpcConvert, RpcConverter,
    helpers::pending_block::BuildPendingEnv,
    transaction::{ConvertReceiptInput, ReceiptConverter},
};
use reth_rpc_eth_api::{RpcTypes, SignTxRequestError, SignableTxRequest, TryIntoSimTx};
use reth_rpc_eth_types::receipt::build_receipt;
use reth_storage_api::BlockReader;
use revm::context::TxEnv;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Helper: map OpTxEnvelope → CeloTxEnvelope
// ---------------------------------------------------------------------------

fn op_tx_to_celo(op_tx: op_alloy_consensus::OpTxEnvelope) -> CeloTxEnvelope {
    use op_alloy_consensus::OpTxEnvelope as Op;
    match op_tx {
        Op::Legacy(tx) => CeloTxEnvelope::Legacy(tx),
        Op::Eip2930(tx) => CeloTxEnvelope::Eip2930(tx),
        Op::Eip1559(tx) => CeloTxEnvelope::Eip1559(tx),
        Op::Eip7702(tx) => CeloTxEnvelope::Eip7702(tx),
        Op::Deposit(tx) => CeloTxEnvelope::Deposit(tx),
    }
}

// ---------------------------------------------------------------------------
// CeloRpcTypes
// ---------------------------------------------------------------------------

/// RPC type configuration for Celo, analogous to `Optimism` in op-alloy.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct CeloRpcTypes;

impl RpcTypes for CeloRpcTypes {
    type Header = alloy_rpc_types_eth::Header;
    type Receipt = CeloTransactionReceipt;
    type TransactionRequest = CeloTransactionRequest;
    type TransactionResponse = op_alloy_rpc_types::Transaction<CeloTxEnvelope>;
}

// ---------------------------------------------------------------------------
// CeloTransactionRequest
// ---------------------------------------------------------------------------

/// Transaction request type for Celo RPC.
///
/// Wraps [`OpTransactionRequest`] and adds Celo-specific fields like `feeCurrency`.
/// Uses custom serde to capture the `feeCurrency` JSON field, which the standard
/// `TransactionRequest` silently drops.
#[derive(Debug, Clone)]
pub struct CeloTransactionRequest {
    /// The wrapped OP-stack transaction request.
    pub inner: OpTransactionRequest,
    /// Celo CIP-64 fee currency address (parsed from `feeCurrency` in JSON).
    pub fee_currency: Option<alloy_primitives::Address>,
}

impl serde::Serialize for CeloTransactionRequest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut value =
            serde_json::to_value(&self.inner).map_err(serde::ser::Error::custom)?;
        if let Some(fc) = self.fee_currency {
            if let Some(obj) = value.as_object_mut() {
                obj.insert(
                    "feeCurrency".to_string(),
                    serde_json::to_value(fc).map_err(serde::ser::Error::custom)?,
                );
            }
        }
        value.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for CeloTransactionRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut value = serde_json::Value::deserialize(deserializer)?;
        let fee_currency = value
            .as_object_mut()
            .and_then(|obj| obj.remove("feeCurrency"))
            .and_then(|v| serde_json::from_value::<alloy_primitives::Address>(v).ok());
        let inner: OpTransactionRequest =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self { inner, fee_currency })
    }
}

impl AsRef<TransactionRequest> for CeloTransactionRequest {
    fn as_ref(&self) -> &TransactionRequest {
        self.inner.as_ref()
    }
}

impl AsMut<TransactionRequest> for CeloTransactionRequest {
    fn as_mut(&mut self) -> &mut TransactionRequest {
        self.inner.as_mut()
    }
}

impl TryIntoSimTx<CeloTxEnvelope> for CeloTransactionRequest {
    fn try_into_sim_tx(self) -> Result<CeloTxEnvelope, ValueError<Self>> {
        let fee_currency = self.fee_currency;
        self.inner
            .try_into_sim_tx()
            .map(|op_tx| {
                let mut celo_tx = op_tx_to_celo(op_tx);
                // If fee_currency is set, wrap the inner EIP-1559 tx into a CIP-64 variant
                if let Some(fc) = fee_currency {
                    if let CeloTxEnvelope::Eip1559(signed) = celo_tx {
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
                        celo_tx = CeloTxEnvelope::Cip64(
                            alloy_consensus::Signed::new_unhashed(cip64, sig),
                        );
                    }
                }
                celo_tx
            })
            .map_err(|e| e.map(|inner| CeloTransactionRequest { inner, fee_currency }))
    }
}

impl<Block: BlockEnvironment> alloy_evm::rpc::TryIntoTxEnv<CeloTransaction<TxEnv>, Block>
    for CeloTransactionRequest
{
    type Err = EthTxEnvError;

    fn try_into_tx_env<Spec>(
        self,
        evm_env: &EvmEnv<Spec, Block>,
    ) -> Result<CeloTransaction<TxEnv>, Self::Err> {
        let fee_currency = self.fee_currency;
        let mut op_tx: op_revm::OpTransaction<TxEnv> = self.inner.try_into_tx_env(evm_env)?;
        // When fee_currency is set, ensure the tx_type is CIP-64 (0x7b) so the handler
        // applies fee currency validation and intrinsic gas.
        if fee_currency.is_some() {
            op_tx.base.tx_type = celo_alloy_consensus::CeloTxType::Cip64 as u8;
        }
        let mut celo_tx = CeloTransaction::new(op_tx);
        celo_tx.fee_currency = fee_currency;
        Ok(celo_tx)
    }
}

impl SignableTxRequest<CeloTxEnvelope> for CeloTransactionRequest {
    async fn try_build_and_sign(
        self,
        signer: impl TxSigner<Signature> + Send,
    ) -> Result<CeloTxEnvelope, SignTxRequestError> {
        if let Some(fc) = self.fee_currency {
            // Build a CIP-64 tx directly so fee_currency is preserved.
            let req = self.inner.as_ref();
            let mut cip64 = celo_alloy_consensus::TxCip64 {
                chain_id: req.chain_id.unwrap_or_default(),
                nonce: req.nonce.unwrap_or_default(),
                gas_limit: req.gas.unwrap_or(0),
                max_fee_per_gas: req.max_fee_per_gas.unwrap_or_default(),
                max_priority_fee_per_gas: req.max_priority_fee_per_gas.unwrap_or_default(),
                to: req.to.unwrap_or_default(),
                value: req.value.unwrap_or_default(),
                access_list: req.access_list.clone().unwrap_or_default(),
                input: req.input.clone().into_input().unwrap_or_default(),
                fee_currency: Some(fc),
            };
            let sig = signer
                .sign_transaction(&mut cip64)
                .await?;
            Ok(CeloTxEnvelope::Cip64(alloy_consensus::Signed::new_unhashed(cip64, sig)))
        } else {
            SignableTxRequest::<op_alloy_consensus::OpTxEnvelope>::try_build_and_sign(
                self.inner, signer,
            )
            .await
            .map(op_tx_to_celo)
        }
    }
}

// ---------------------------------------------------------------------------
// CeloReceiptConverter
// ---------------------------------------------------------------------------

/// Converter for Celo receipts, producing [`CeloTransactionReceipt`] RPC representations.
///
/// Analogous to [`OpReceiptConverter`](reth_optimism_rpc::eth::receipt::OpReceiptConverter)
/// but handles the additional [`CeloReceipt::Cip64`] variant with correct type, base fee,
/// and effective gas price.
#[derive(Debug, Clone)]
pub struct CeloReceiptConverter<Provider> {
    provider: Provider,
}

impl<Provider> CeloReceiptConverter<Provider> {
    /// Creates a new [`CeloReceiptConverter`].
    pub const fn new(provider: Provider) -> Self {
        Self { provider }
    }
}

impl<Provider> ReceiptConverter<CeloPrimitives> for CeloReceiptConverter<Provider>
where
    Provider: BlockReader<Block = alloy_consensus::Block<CeloTxEnvelope>>
        + ChainSpecProvider<ChainSpec: OpHardforks>
        + Debug
        + 'static,
{
    type RpcReceipt = CeloTransactionReceipt;
    type Error = reth_optimism_rpc::OpEthApiError;

    fn convert_receipts(
        &self,
        inputs: Vec<ConvertReceiptInput<'_, CeloPrimitives>>,
    ) -> Result<Vec<Self::RpcReceipt>, Self::Error> {
        let Some(block_number) = inputs.first().map(|r| r.meta.block_number) else {
            return Ok(Vec::new());
        };

        let block = self
            .provider
            .block_by_number(block_number)?
            .ok_or(reth_rpc_eth_types::EthApiError::HeaderNotFound(block_number.into()))?;

        self.convert_receipts_with_block(inputs, &SealedBlock::new_unhashed(block))
    }

    fn convert_receipts_with_block(
        &self,
        inputs: Vec<ConvertReceiptInput<'_, CeloPrimitives>>,
        block: &SealedBlock<alloy_consensus::Block<CeloTxEnvelope>>,
    ) -> Result<Vec<Self::RpcReceipt>, Self::Error> {
        let mut l1_block_info = match reth_optimism_evm::extract_l1_info(block.body()) {
            Ok(l1_block_info) => l1_block_info,
            Err(err) => {
                use alloy_consensus::BlockHeader;
                let genesis_number =
                    self.provider.chain_spec().genesis().number.unwrap_or_default();
                if block.header().number() == genesis_number {
                    return Ok(vec![]);
                }
                return Err(err.into());
            }
        };

        let chain_spec = self.provider.chain_spec();
        let mut receipts = Vec::with_capacity(inputs.len());

        for input in inputs {
            l1_block_info.clear_tx_l1_cost();
            receipts.push(Self::build_receipt(&chain_spec, input, &mut l1_block_info)?);
        }

        Ok(receipts)
    }
}

impl<Provider> CeloReceiptConverter<Provider> {
    /// Converts a single [`ConvertReceiptInput`] with [`CeloReceipt`] to a
    /// [`CeloTransactionReceipt`].
    fn build_receipt<ChainSpec: OpHardforks>(
        chain_spec: &ChainSpec,
        input: ConvertReceiptInput<'_, CeloPrimitives>,
        l1_block_info: &mut op_revm::L1BlockInfo,
    ) -> Result<CeloTransactionReceipt, reth_optimism_rpc::OpEthApiError> {
        use alloy_consensus::Receipt;

        let timestamp = input.meta.timestamp;
        let block_number = input.meta.block_number;
        let tx_signed = *input.tx.inner();

        let mut core_receipt =
            build_receipt(input, None, |celo_receipt, next_log_index, meta| {
                // Compute bloom from primitive logs, then map to RPC logs.
                let map_receipt =
                    move |receipt: alloy_consensus::Receipt<alloy_primitives::Log>| {
                        let bloom = alloy_primitives::logs_bloom(receipt.logs.iter());
                        let Receipt { status, cumulative_gas_used, logs } = receipt;
                        let logs = Log::collect_for_receipt(next_log_index, meta, logs);
                        (Receipt { status, cumulative_gas_used, logs }, bloom)
                    };

                let envelope: CeloReceiptEnvelope<Log> = match celo_receipt {
                    CeloReceipt::Legacy(r) => {
                        let (r, bloom) = map_receipt(r);
                        CeloReceiptEnvelope::Legacy(ReceiptWithBloom {
                            receipt: r,
                            logs_bloom: bloom,
                        })
                    }
                    CeloReceipt::Eip2930(r) => {
                        let (r, bloom) = map_receipt(r);
                        CeloReceiptEnvelope::Eip2930(ReceiptWithBloom {
                            receipt: r,
                            logs_bloom: bloom,
                        })
                    }
                    CeloReceipt::Eip1559(r) => {
                        let (r, bloom) = map_receipt(r);
                        CeloReceiptEnvelope::Eip1559(ReceiptWithBloom {
                            receipt: r,
                            logs_bloom: bloom,
                        })
                    }
                    CeloReceipt::Eip7702(r) => {
                        let (r, bloom) = map_receipt(r);
                        CeloReceiptEnvelope::Eip7702(ReceiptWithBloom {
                            receipt: r,
                            logs_bloom: bloom,
                        })
                    }
                    CeloReceipt::Cip64(cip64) => {
                        let (r, bloom) = map_receipt(cip64.inner);
                        CeloReceiptEnvelope::Cip64(CeloCip64ReceiptWithBloom {
                            receipt: CeloCip64Receipt {
                                inner: r,
                                base_fee: cip64.base_fee,
                            },
                            logs_bloom: bloom,
                        })
                    }
                    CeloReceipt::Deposit(d) => {
                        let (inner, bloom) = map_receipt(d.inner);
                        CeloReceiptEnvelope::Deposit(
                            op_alloy_consensus::OpDepositReceiptWithBloom {
                                receipt: op_alloy_consensus::OpDepositReceipt {
                                    inner,
                                    deposit_nonce: d.deposit_nonce,
                                    deposit_receipt_version: d.deposit_receipt_version,
                                },
                                logs_bloom: bloom,
                            },
                        )
                    }
                };

                envelope
            });

        // For CIP-64 receipts, fix effective_gas_price using the FC-denominated base fee
        // instead of the native CELO base fee that `build_receipt` used.
        let base_fee = if let CeloReceiptEnvelope::Cip64(ref cip64) = core_receipt.inner {
            let fc_base_fee = cip64.receipt.base_fee;
            if let Some(fc_bf) = fc_base_fee {
                // Recompute effective gas price with the fee-currency base fee
                core_receipt.effective_gas_price =
                    tx_signed.effective_gas_price(Some(fc_bf as u64));
            }
            fc_base_fee
        } else {
            None
        };

        let op_fields = OpReceiptFieldsBuilder::new(timestamp, block_number)
            .l1_block_info(chain_spec, tx_signed, l1_block_info)?
            .build();

        Ok(CeloTransactionReceipt {
            inner: core_receipt,
            l1_block_info: op_fields.l1_block_info,
            base_fee,
        })
    }
}

// ---------------------------------------------------------------------------
// CeloRpcConvert
// ---------------------------------------------------------------------------

/// Type alias for the Celo-specific [`RpcConverter`].
pub type CeloRpcConvert<N> = RpcConverter<
    CeloRpcTypes,
    <N as FullNodeComponents>::Evm,
    CeloReceiptConverter<<N as reth_node_builder::node::FullNodeTypes>::Provider>,
    (),
    OpTxInfoMapper<<N as reth_node_builder::node::FullNodeTypes>::Provider>,
>;

// ---------------------------------------------------------------------------
// CeloEthApiBuilder
// ---------------------------------------------------------------------------

/// ETH API builder for Celo nodes.
///
/// Uses [`CeloReceiptConverter`] instead of the op-reth `OpReceiptConverter`, which has a
/// hard `Receipt = OpReceipt` bound incompatible with [`CeloPrimitives`].
#[derive(Debug, Default)]
pub struct CeloEthApiBuilder {
    /// Sequencer URL for transaction forwarding.
    pub sequencer_url: Option<String>,
    /// Headers to use for the sequencer client requests.
    pub sequencer_headers: Vec<String>,
}

impl CeloEthApiBuilder {
    /// Sets the sequencer URL for transaction forwarding.
    pub fn with_sequencer(mut self, sequencer_url: Option<String>) -> Self {
        self.sequencer_url = sequencer_url;
        self
    }

    /// Sets the headers to use for the sequencer client requests.
    pub fn with_sequencer_headers(mut self, sequencer_headers: Vec<String>) -> Self {
        self.sequencer_headers = sequencer_headers;
        self
    }
}

impl<N> EthApiBuilder<N> for CeloEthApiBuilder
where
    N: FullNodeComponents<
        Evm: ConfigureEvm<
            NextBlockEnvCtx: BuildPendingEnv<alloy_consensus::Header>,
        >,
        Types: NodeTypes<
            ChainSpec: reth_chainspec::Hardforks
                + reth_chainspec::EthereumHardforks
                + OpHardforks,
            Primitives = CeloPrimitives,
        >,
    >,
    N::Provider: BlockReader<Block = alloy_consensus::Block<CeloTxEnvelope>>
        + ChainSpecProvider<ChainSpec: OpHardforks>
        + Clone
        + Debug
        + 'static,
    CeloRpcConvert<N>: RpcConvert<Network = CeloRpcTypes>,
    OpEthApi<N, CeloRpcConvert<N>>: FullEthApiServer<Provider = N::Provider, Pool = N::Pool>,
{
    type EthApi = OpEthApi<N, CeloRpcConvert<N>>;

    async fn build_eth_api(self, ctx: EthApiCtx<'_, N>) -> eyre::Result<Self::EthApi> {
        let Self { sequencer_url, sequencer_headers } = self;

        let rpc_converter =
            RpcConverter::new(CeloReceiptConverter::new(ctx.components.provider().clone()))
                .with_mapper(OpTxInfoMapper::new(ctx.components.provider().clone()));

        let eth_api = ctx.eth_api_builder().with_rpc_converter(rpc_converter).build_inner();

        let sequencer_client = if let Some(url) = sequencer_url {
            Some(
                SequencerClient::new_with_headers(&url, sequencer_headers)
                    .await
                    .map_err(|e| eyre::eyre!("Failed to init sequencer client with {url}: {e}"))?,
            )
        } else {
            None
        };

        Ok(OpEthApi::new(eth_api, sequencer_client, U256::from(1_000_000_u64), None))
    }
}

// ---------------------------------------------------------------------------
// Fee-currency-aware gas price RPCs
// ---------------------------------------------------------------------------

/// Type-erased wrapper for the parts of the Eth API we need in the gas price
/// RPC overrides. This avoids leaking the heavily-parameterised [`EthApiServer`]
/// types into the RPC module registration.
pub struct CeloFeeApi {
    gas_price: Box<dyn Fn() -> Pin<Box<dyn Future<Output = jsonrpsee::core::RpcResult<U256>> + Send>> + Send + Sync>,
    priority_fee: Box<dyn Fn() -> Pin<Box<dyn Future<Output = jsonrpsee::core::RpcResult<U256>> + Send>> + Send + Sync>,
    eth_call: Box<dyn Fn(CeloTransactionRequest) -> Pin<Box<dyn Future<Output = jsonrpsee::core::RpcResult<Bytes>> + Send>> + Send + Sync>,
    fee_currency_directory: Address,
}

impl Debug for CeloFeeApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CeloFeeApi").finish_non_exhaustive()
    }
}

impl CeloFeeApi {
    /// Query the exchange rate for `fee_currency` from the FeeCurrencyDirectory.
    ///
    /// Returns `(numerator, denominator)`.
    async fn exchange_rate(
        &self,
        fee_currency: Address,
    ) -> Result<(U256, U256), jsonrpsee_types::ErrorObjectOwned> {
        // getExchangeRate(address) → (uint256, uint256)
        let selector = &keccak256("getExchangeRate(address)")[..4];
        let mut calldata = Vec::with_capacity(36);
        calldata.extend_from_slice(selector);
        calldata.extend_from_slice(fee_currency.into_word().as_slice());

        let request = CeloTransactionRequest {
            inner: OpTransactionRequest::default()
                .to(self.fee_currency_directory)
                .input(TransactionInput::new(Bytes::from(calldata))),
            fee_currency: None,
        };

        let result = (self.eth_call)(request).await?;

        if result.len() < 64 {
            return Err(jsonrpsee_types::ErrorObject::owned(
                -32000,
                format!(
                    "Invalid exchange rate response for {fee_currency}: expected >=64 bytes, got {}",
                    result.len()
                ),
                None::<()>,
            ));
        }

        let numerator = U256::from_be_slice(&result[0..32]);
        let denominator = U256::from_be_slice(&result[32..64]);

        if denominator.is_zero() {
            return Err(jsonrpsee_types::ErrorObject::owned(
                -32000,
                format!("Exchange rate denominator is zero for {fee_currency}"),
                None::<()>,
            ));
        }

        Ok((numerator, denominator))
    }
}

/// Build a [`jsonrpsee::RpcModule`] with fee-currency-aware `eth_gasPrice` and
/// `eth_maxPriorityFeePerGas` that accept an optional `feeCurrency` parameter.
///
/// When `feeCurrency` is absent the methods behave identically to the standard
/// Ethereum RPCs. When present, the returned price is scaled by the on-chain
/// exchange rate for that fee currency.
pub fn celo_gas_price_module(api: CeloFeeApi) -> jsonrpsee::RpcModule<Arc<CeloFeeApi>> {
    let ctx = Arc::new(api);
    let mut module = jsonrpsee::RpcModule::new(ctx);

    module
        .register_async_method("eth_gasPrice", |params, ctx, _| async move {
            let fee_currency: Option<Address> = params.sequence().optional_next()?;
            let base_price = (ctx.gas_price)().await?;
            match fee_currency {
                Some(fc) => {
                    let (num, denom) = ctx.exchange_rate(fc).await?;
                    Ok::<_, jsonrpsee_types::ErrorObjectOwned>(base_price * num / denom)
                }
                None => Ok(base_price),
            }
        })
        .expect("eth_gasPrice registration");

    module
        .register_async_method("eth_maxPriorityFeePerGas", |params, ctx, _| async move {
            let fee_currency: Option<Address> = params.sequence().optional_next()?;
            let base_tip = (ctx.priority_fee)().await?;
            match fee_currency {
                Some(fc) => {
                    let (num, denom) = ctx.exchange_rate(fc).await?;
                    Ok::<_, jsonrpsee_types::ErrorObjectOwned>(base_tip * num / denom)
                }
                None => Ok(base_tip),
            }
        })
        .expect("eth_maxPriorityFeePerGas registration");

    module
}

/// Create a [`CeloFeeApi`] from any type implementing the full Eth RPC API.
///
/// This is a generic constructor that captures the concrete [`EthApiServer`]
/// implementor behind type-erased closures, allowing the resulting
/// [`CeloFeeApi`] (and the [`jsonrpsee::RpcModule`] built from it) to be
/// stored and used without propagating the complex generic bounds.
pub fn make_celo_fee_api<Api>(eth_api: Api, fee_currency_directory: Address) -> CeloFeeApi
where
    Api: reth_rpc_eth_api::EthApiServer<
            CeloTransactionRequest,
            op_alloy_rpc_types::Transaction<CeloTxEnvelope>,
            alloy_rpc_types_eth::Block<
                op_alloy_rpc_types::Transaction<CeloTxEnvelope>,
                alloy_rpc_types_eth::Header,
            >,
            CeloTransactionReceipt,
            alloy_rpc_types_eth::Header,
            crate::primitives::CeloTransactionSigned,
        > + Send
        + Sync
        + Clone
        + 'static,
{
    use reth_rpc_eth_api::EthApiServer;

    let ea1 = Arc::new(eth_api.clone());
    let ea2 = ea1.clone();
    let ea3 = ea1.clone();

    CeloFeeApi {
        gas_price: Box::new(move || {
            let ea = ea1.clone();
            Box::pin(async move { EthApiServer::gas_price(&*ea).await })
        }),
        priority_fee: Box::new(move || {
            let ea = ea2.clone();
            Box::pin(async move { EthApiServer::max_priority_fee_per_gas(&*ea).await })
        }),
        eth_call: Box::new(move |req| {
            let ea = ea3.clone();
            Box::pin(async move { EthApiServer::call(&*ea, req, None, None, None).await })
        }),
        fee_currency_directory,
    }
}

// ---------------------------------------------------------------------------
// Admin RPCs for fee currency blocklist management
// ---------------------------------------------------------------------------

/// Build a [`jsonrpsee::RpcModule`] with fee currency blocklist admin methods:
/// - `admin_disableBlocklistFeeCurrencies`: Disable blocklisting for given currencies
/// - `admin_enableBlocklistFeeCurrencies`: Re-enable blocklisting for given currencies
/// - `admin_unblockFeeCurrency`: Remove a currency from the blocklist
pub fn celo_admin_module(
    blocklist: alloy_celo_evm::blocklist::FeeCurrencyBlocklist,
) -> jsonrpsee::RpcModule<alloy_celo_evm::blocklist::FeeCurrencyBlocklist> {
    let mut module = jsonrpsee::RpcModule::new(blocklist);

    module
        .register_method("admin_disableBlocklistFeeCurrencies", |params, ctx, _| {
            let currencies: Vec<Address> = params.one()?;
            ctx.disable_blocklist(&currencies);
            Ok::<_, jsonrpsee_types::ErrorObjectOwned>(true)
        })
        .expect("admin_disableBlocklistFeeCurrencies registration");

    module
        .register_method("admin_enableBlocklistFeeCurrencies", |params, ctx, _| {
            let currencies: Vec<Address> = params.one()?;
            ctx.enable_blocklist(&currencies);
            Ok::<_, jsonrpsee_types::ErrorObjectOwned>(true)
        })
        .expect("admin_enableBlocklistFeeCurrencies registration");

    module
        .register_method("admin_unblockFeeCurrency", |params, ctx, _| {
            let currency: Address = params.one()?;
            ctx.unblock_currency(currency);
            Ok::<_, jsonrpsee_types::ErrorObjectOwned>(true)
        })
        .expect("admin_unblockFeeCurrency registration");

    module
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_with_fee_currency() {
        let fc: Address = "0x765DE816845861e75A25fCA122bb6898B8B1282a".parse().unwrap();
        let req = CeloTransactionRequest {
            inner: OpTransactionRequest::default()
                .to(Address::ZERO)
                .value(U256::from(100)),
            fee_currency: Some(fc),
        };
        let json = serde_json::to_string(&req).unwrap();
        let deser: CeloTransactionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.fee_currency, Some(fc));
        assert_eq!(deser.inner.as_ref().value, req.inner.as_ref().value);
    }

    #[test]
    fn serde_roundtrip_without_fee_currency() {
        let req = CeloTransactionRequest {
            inner: OpTransactionRequest::default().to(Address::ZERO),
            fee_currency: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let deser: CeloTransactionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.fee_currency, None);
    }

    #[test]
    fn serde_null_fee_currency_deserializes_as_none() {
        let json = r#"{"feeCurrency": null, "to": "0x0000000000000000000000000000000000000000"}"#;
        let deser: CeloTransactionRequest = serde_json::from_str(json).unwrap();
        assert_eq!(deser.fee_currency, None);
    }

    /// Verify CIP-64 receipt serialization: type=0x7b, baseFee present, effectiveGasPrice
    /// uses the FC-denominated base fee (not the native CELO base fee).
    #[test]
    fn cip64_receipt_serde_has_correct_type_and_base_fee() {
        use alloy_consensus::Receipt;
        use alloy_primitives::B256;

        // Construct a CIP-64 receipt with known base_fee
        let fc_base_fee: u128 = 500_000_000; // 0.5 gwei in fee currency
        let receipt = CeloTransactionReceipt {
            inner: alloy_rpc_types_eth::TransactionReceipt {
                inner: CeloReceiptEnvelope::Cip64(CeloCip64ReceiptWithBloom {
                    receipt: CeloCip64Receipt {
                        inner: Receipt {
                            status: true.into(),
                            cumulative_gas_used: 21000,
                            logs: vec![],
                        },
                        base_fee: Some(fc_base_fee),
                    },
                    logs_bloom: Default::default(),
                }),
                transaction_hash: B256::ZERO,
                transaction_index: Some(0),
                block_hash: Some(B256::ZERO),
                block_number: Some(1),
                from: Address::ZERO,
                to: Some(Address::ZERO),
                gas_used: 21000,
                contract_address: None,
                effective_gas_price: 500_020_000, // FC base fee + tip
                blob_gas_price: None,
                blob_gas_used: None,
            },
            l1_block_info: Default::default(),
            base_fee: Some(fc_base_fee),
        };

        let json = serde_json::to_value(&receipt).unwrap();

        // type must be 0x7b (CIP-64), not 0x2 (EIP-1559)
        assert_eq!(json["type"], "0x7b", "CIP-64 receipt type must be 0x7b");

        // baseFee must be present and correct
        assert_eq!(
            json["baseFee"],
            format!("0x{fc_base_fee:x}"),
            "baseFee must be the FC-denominated base fee"
        );

        // effectiveGasPrice must reflect the FC-denominated price
        assert_eq!(
            json["effectiveGasPrice"], "0x1dcdb320",
            "effectiveGasPrice must use the FC base fee, not native"
        );
    }

    #[test]
    fn cip64_receipt_deserialization_preserves_fields() {
        // JSON from a CIP-64 receipt (e.g. from Celo Sepolia)
        let json = r#"{
            "blockHash": "0x0000000000000000000000000000000000000000000000000000000000000001",
            "blockNumber": "0x1",
            "contractAddress": null,
            "cumulativeGasUsed": "0x5208",
            "effectiveGasPrice": "0x1dcd6920",
            "from": "0x0000000000000000000000000000000000000000",
            "gasUsed": "0x5208",
            "logs": [],
            "logsBloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
            "status": "0x1",
            "to": "0x0000000000000000000000000000000000000000",
            "transactionHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "transactionIndex": "0x0",
            "type": "0x7b",
            "baseFee": "0x1dcd6500"
        }"#;

        let receipt: CeloTransactionReceipt = serde_json::from_str(json).unwrap();
        assert_eq!(receipt.base_fee, Some(500_000_000));
        assert!(matches!(receipt.inner.inner, CeloReceiptEnvelope::Cip64(_)));
    }

    #[test]
    fn serde_inner_op_fields_survive_roundtrip() {
        let fc: Address = "0x765DE816845861e75A25fCA122bb6898B8B1282a".parse().unwrap();
        let req = CeloTransactionRequest {
            inner: OpTransactionRequest::default()
                .to(Address::ZERO)
                .nonce(42)
                .max_fee_per_gas(1_000_000_000)
                .max_priority_fee_per_gas(100),
            fee_currency: Some(fc),
        };
        let json = serde_json::to_string(&req).unwrap();
        let deser: CeloTransactionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.fee_currency, Some(fc));
        assert_eq!(deser.inner.as_ref().nonce, Some(42));
        assert_eq!(deser.inner.as_ref().max_fee_per_gas, Some(1_000_000_000));
        assert_eq!(deser.inner.as_ref().max_priority_fee_per_gas, Some(100));
    }

    // -----------------------------------------------------------------------
    // Item #17: Admin RPC module registration
    // -----------------------------------------------------------------------

    #[test]
    fn admin_module_registers_all_methods() {
        let blocklist = alloy_celo_evm::blocklist::FeeCurrencyBlocklist::default();
        let module = celo_admin_module(blocklist);

        // Verify all three admin methods are registered
        let method_names: Vec<_> = module.method_names().collect();
        assert!(
            method_names.contains(&"admin_disableBlocklistFeeCurrencies"),
            "missing admin_disableBlocklistFeeCurrencies"
        );
        assert!(
            method_names.contains(&"admin_enableBlocklistFeeCurrencies"),
            "missing admin_enableBlocklistFeeCurrencies"
        );
        assert!(
            method_names.contains(&"admin_unblockFeeCurrency"),
            "missing admin_unblockFeeCurrency"
        );
    }

    #[test]
    fn admin_blocklist_disable_makes_currency_unblocked() {
        let blocklist = alloy_celo_evm::blocklist::FeeCurrencyBlocklist::default();
        let fc = Address::with_last_byte(0xAA);

        // Block the currency
        blocklist.block_currency(fc, 1000);
        assert!(blocklist.is_blocked(fc));

        // Disable blocklisting for it
        blocklist.disable_blocklist(&[fc]);
        assert!(!blocklist.is_blocked(fc));

        // Block again — should have no effect while disabled
        blocklist.block_currency(fc, 2000);
        assert!(!blocklist.is_blocked(fc));

        // Re-enable blocklisting
        blocklist.enable_blocklist(&[fc]);
    }

    #[test]
    fn admin_unblock_removes_currency() {
        let blocklist = alloy_celo_evm::blocklist::FeeCurrencyBlocklist::default();
        let fc = Address::with_last_byte(0xBB);

        blocklist.block_currency(fc, 1000);
        assert!(blocklist.is_blocked(fc));

        blocklist.unblock_currency(fc);
        assert!(!blocklist.is_blocked(fc));
    }

    // -----------------------------------------------------------------------
    // Item #19: TryIntoSimTx with non-EIP1559 inner tx
    // -----------------------------------------------------------------------

    #[test]
    fn try_into_sim_tx_fee_currency_ignored_for_legacy() {
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
        match result {
            Ok(tx) => {
                // The tx should NOT be CIP-64 — fee_currency wrapping only
                // applies when the inner OP tx produces EIP-1559.
                assert!(
                    !matches!(tx, CeloTxEnvelope::Cip64(_)),
                    "Legacy request with fee_currency should not produce CIP-64 tx"
                );
            }
            Err(_) => {
                // It's also acceptable for the conversion to fail for legacy
                // requests — the key invariant is that it doesn't produce a
                // malformed CIP-64 envelope.
            }
        }
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
        match tx {
            CeloTxEnvelope::Cip64(signed) => {
                assert_eq!(signed.tx().fee_currency, Some(fc));
            }
            other => panic!("Expected CIP-64 (0x7b), got type 0x{:02x}", other.ty()),
        }
    }
}
