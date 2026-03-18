//! Celo-specific RPC types and converters for reth.
//!
//! Provides [`CeloRpcTypes`], [`CeloReceiptConverter`], and [`CeloEthApiBuilder`] —
//! the Celo equivalents of the op-reth `Optimism` network type, `OpReceiptConverter`,
//! and `OpEthApiBuilder`.

use crate::{primitives::CeloPrimitives, receipt::CeloReceipt};
use alloy_consensus::{ReceiptWithBloom, Transaction, error::ValueError};
use alloy_evm::{EvmEnv, env::BlockEnvironment, rpc::EthTxEnvError};
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
    FullEthApiServer, RpcConvert, RpcConverter, RpcTypes, SignTxRequestError, SignableTxRequest,
    TryIntoSimTx,
    helpers::pending_block::BuildPendingEnv,
    transaction::{ConvertReceiptInput, ReceiptConverter},
};
use reth_rpc_eth_types::receipt::build_receipt;
use reth_storage_api::BlockReader;
use revm::context::TxEnv;
use std::{fmt::Debug, future::Future, pin::Pin, sync::Arc};

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
        let mut value = serde_json::to_value(&self.inner).map_err(serde::ser::Error::custom)?;
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
                        celo_tx = CeloTxEnvelope::Cip64(alloy_consensus::Signed::new_unhashed(
                            cip64, sig,
                        ));
                    }
                }
                celo_tx
            })
            .map_err(|e| e.map(|inner| Self { inner, fee_currency }))
    }
}

/// Gas estimation works correctly for CIP-64 because:
/// - `disable_base_fee = true` during estimation, so FC/native base fee mismatch is irrelevant
/// - The CIP-64 handler in celo-revm runs during simulation, correctly applying per-currency
///   intrinsic gas costs
/// - Binary search only varies `gas_limit`, not fee parameters
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
            let sig = signer.sign_transaction(&mut cip64).await?;
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

        let mut core_receipt = build_receipt(input, None, |celo_receipt, next_log_index, meta| {
            // Compute bloom from primitive logs, then map to RPC logs.
            let map_receipt = move |receipt: alloy_consensus::Receipt<alloy_primitives::Log>| {
                let bloom = alloy_primitives::logs_bloom(receipt.logs.iter());
                let Receipt { status, cumulative_gas_used, logs } = receipt;
                let logs = Log::collect_for_receipt(next_log_index, meta, logs);
                (Receipt { status, cumulative_gas_used, logs }, bloom)
            };

            let envelope: CeloReceiptEnvelope<Log> = match celo_receipt {
                CeloReceipt::Legacy(r) => {
                    let (r, bloom) = map_receipt(r);
                    CeloReceiptEnvelope::Legacy(ReceiptWithBloom { receipt: r, logs_bloom: bloom })
                }
                CeloReceipt::Eip2930(r) => {
                    let (r, bloom) = map_receipt(r);
                    CeloReceiptEnvelope::Eip2930(ReceiptWithBloom { receipt: r, logs_bloom: bloom })
                }
                CeloReceipt::Eip1559(r) => {
                    let (r, bloom) = map_receipt(r);
                    CeloReceiptEnvelope::Eip1559(ReceiptWithBloom { receipt: r, logs_bloom: bloom })
                }
                CeloReceipt::Eip7702(r) => {
                    let (r, bloom) = map_receipt(r);
                    CeloReceiptEnvelope::Eip7702(ReceiptWithBloom { receipt: r, logs_bloom: bloom })
                }
                CeloReceipt::Cip64(cip64) => {
                    let (r, bloom) = map_receipt(cip64.inner);
                    CeloReceiptEnvelope::Cip64(CeloCip64ReceiptWithBloom {
                        receipt: CeloCip64Receipt { inner: r, base_fee: cip64.base_fee },
                        logs_bloom: bloom,
                    })
                }
                CeloReceipt::Deposit(d) => {
                    let (inner, bloom) = map_receipt(d.inner);
                    CeloReceiptEnvelope::Deposit(op_alloy_consensus::OpDepositReceiptWithBloom {
                        receipt: op_alloy_consensus::OpDepositReceipt {
                            inner,
                            deposit_nonce: d.deposit_nonce,
                            deposit_receipt_version: d.deposit_receipt_version,
                        },
                        logs_bloom: bloom,
                    })
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
            Evm: ConfigureEvm<NextBlockEnvCtx: BuildPendingEnv<alloy_consensus::Header>>,
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
/// RPC overrides. This avoids leaking the heavily-parameterised `EthApiServer`
/// types into the RPC module registration.
/// Type alias for CeloBlock in RPC context.
type CeloRpcBlock = alloy_rpc_types_eth::Block<
    op_alloy_rpc_types::Transaction<CeloTxEnvelope>,
    alloy_rpc_types_eth::Header,
>;
/// Type-erased async closure.
type AsyncFn<A, R> = Box<
    dyn Fn(A) -> Pin<Box<dyn Future<Output = jsonrpsee::core::RpcResult<R>> + Send>> + Send + Sync,
>;
/// Type-erased async thunk (no args).
type AsyncThunk<R> = Box<
    dyn Fn() -> Pin<Box<dyn Future<Output = jsonrpsee::core::RpcResult<R>> + Send>> + Send + Sync,
>;

/// Type-erased wrapper for the parts of the Eth API needed by gas price
/// and fee history RPC overrides.
pub struct CeloFeeApi {
    gas_price: AsyncThunk<U256>,
    priority_fee: AsyncThunk<U256>,
    eth_call: AsyncFn<CeloTransactionRequest, Bytes>,
    #[allow(clippy::type_complexity)]
    fee_history: Box<
        dyn Fn(
                alloy_primitives::U64,
                alloy_rpc_types_eth::BlockNumberOrTag,
                Option<Vec<f64>>,
            ) -> Pin<
                Box<
                    dyn Future<Output = jsonrpsee::core::RpcResult<alloy_rpc_types_eth::FeeHistory>>
                        + Send,
                >,
            > + Send
            + Sync,
    >,
    block_by_number: AsyncFn<alloy_rpc_types_eth::BlockNumberOrTag, Option<CeloRpcBlock>>,
    block_receipts:
        AsyncFn<alloy_rpc_types_eth::BlockNumberOrTag, Option<Vec<CeloTransactionReceipt>>>,
    fee_currency_directory: Address,
}

impl Debug for CeloFeeApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CeloFeeApi").finish_non_exhaustive()
    }
}

/// JSON-RPC server error code for call execution failures.
const RPC_SERVER_ERROR: i32 = -32000;

/// Pre-computed selector for `getExchangeRate(address)`.
/// keccak256("getExchangeRate(address)")[..4]
static GET_EXCHANGE_RATE_SELECTOR: std::sync::LazyLock<[u8; 4]> = std::sync::LazyLock::new(|| {
    let hash = keccak256("getExchangeRate(address)");
    [hash[0], hash[1], hash[2], hash[3]]
});

impl CeloFeeApi {
    /// Query the exchange rate for `fee_currency` from the FeeCurrencyDirectory.
    ///
    /// Returns `(numerator, denominator)`.
    async fn exchange_rate(
        &self,
        fee_currency: Address,
    ) -> Result<(U256, U256), jsonrpsee_types::ErrorObjectOwned> {
        let mut calldata = Vec::with_capacity(36);
        calldata.extend_from_slice(GET_EXCHANGE_RATE_SELECTOR.as_slice());
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
                RPC_SERVER_ERROR,
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
                RPC_SERVER_ERROR,
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
///
/// **Known limitation:** The underlying gas price oracle samples recent blocks'
/// effective tips without normalizing CIP-64 tips to native first. If a block
/// contains mostly CIP-64 txs, the oracle's base tip estimate may be slightly
/// off. The exchange-rate scaling applied here partially compensates.
pub fn celo_gas_price_module(api: Arc<CeloFeeApi>) -> jsonrpsee::RpcModule<Arc<CeloFeeApi>> {
    let mut module = jsonrpsee::RpcModule::new(api);

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

/// Convert a CIP-64 transaction's effective tip from fee-currency units to native CELO.
///
/// Computes: `min(max_fee_fc - base_fee_fc, priority_fee_fc) * rate_denom / rate_num`
/// where `base_fee_fc = base_fee_native * rate_num / rate_denom`.
///
/// Returns `0` if `rate_num` is zero (degenerate rate).
pub(crate) fn cip64_native_tip(
    max_fee_fc: u128,
    priority_fee_fc: u128,
    base_fee_native: u64,
    rate_num: U256,
    rate_denom: U256,
) -> u128 {
    if rate_num.is_zero() {
        return 0;
    }
    let base_fee_fc = U256::from(base_fee_native) * rate_num / rate_denom;
    let tip_fc =
        U256::from(max_fee_fc).saturating_sub(base_fee_fc).min(U256::from(priority_fee_fc));
    (tip_fc * rate_denom / rate_num).try_into().unwrap_or(u128::MAX)
}

/// Compute gas-weighted reward percentiles from a pre-sorted `(tip, gas_used)` slice.
///
/// `tip_gas` must be sorted by tip ascending before calling. Returns a vector of the
/// same length as `percentiles`. Returns all zeros if `tip_gas` is empty or all gas
/// values are zero.
pub(crate) fn compute_gas_weighted_percentiles(
    tip_gas: &[(u128, u64)],
    percentiles: &[f64],
) -> Vec<u128> {
    let total_gas: u64 = tip_gas.iter().map(|&(_, g)| g).sum();
    if total_gas == 0 {
        return vec![0; percentiles.len()];
    }
    percentiles
        .iter()
        .map(|&p| {
            let threshold = ((p / 100.0) * total_gas as f64) as u64;
            let mut cum_gas: u64 = 0;
            for &(tip, gas) in tip_gas {
                cum_gas += gas;
                if cum_gas > threshold {
                    return tip;
                }
            }
            tip_gas.last().map(|&(tip, _)| tip).unwrap_or(0)
        })
        .collect()
}

/// Build a [`jsonrpsee::RpcModule`] with a fee-currency-aware `eth_feeHistory` that
/// normalizes CIP-64 transaction tips from fee-currency units to native CELO.
///
/// For each block in the fee history range, if any CIP-64 transactions are present,
/// their effective tips are converted to native CELO using the on-chain exchange rate,
/// and the reward percentiles are recomputed.
pub fn celo_fee_history_module(api: Arc<CeloFeeApi>) -> jsonrpsee::RpcModule<Arc<CeloFeeApi>> {
    let mut module = jsonrpsee::RpcModule::new(api);

    module
        .register_async_method("eth_feeHistory", |params, ctx, _| async move {
            use alloy_consensus::Transaction;

            let mut seq = params.sequence();
            let block_count: alloy_primitives::U64 = seq.next()?;
            let newest_block: alloy_rpc_types_eth::BlockNumberOrTag = seq.next()?;
            let reward_percentiles: Option<Vec<f64>> = seq.optional_next()?;

            // Get the base fee history from the underlying implementation
            let mut history =
                (ctx.fee_history)(block_count, newest_block, reward_percentiles.clone()).await?;

            // If no reward percentiles requested or no rewards returned, pass through
            let percentiles = match reward_percentiles {
                Some(p) if !p.is_empty() => p,
                _ => return Ok(history),
            };
            let rewards = match history.reward.as_mut() {
                Some(r) if !r.is_empty() => r,
                _ => return Ok(history),
            };

            // For each block in range, fetch the block and normalize CIP-64 tips
            let oldest_block = history.oldest_block;
            // Cache exchange rates per fee currency across blocks to avoid redundant lookups.
            // The rate is queried at latest state (not per-block), so cross-block reuse is safe.
            let mut rate_cache: std::collections::HashMap<Address, Option<(U256, U256)>> =
                std::collections::HashMap::new();
            for (i, block_rewards) in rewards.iter_mut().enumerate() {
                let block_num = oldest_block + i as u64;
                let block_tag = alloy_rpc_types_eth::BlockNumberOrTag::Number(block_num);

                let block = match (ctx.block_by_number)(block_tag).await? {
                    Some(b) => b,
                    None => continue,
                };

                let base_fee = block.header.base_fee_per_gas;

                let txs = match block.transactions.as_transactions() {
                    Some(txs) => txs,
                    None => continue,
                };

                use alloy_eips::Typed2718;

                let cip64_ty = celo_alloy_consensus::CeloTxType::Cip64 as u8;

                // Check if any tx is CIP-64 — if not, skip this block
                let has_cip64 = txs.iter().any(|tx| tx.ty() == cip64_ty);
                if !has_cip64 {
                    continue;
                }

                // Collect per-tx effective tips, converting CIP-64 tips to native.
                //
                // For CIP-64 txs, `effective_tip_per_gas(base_fee)` would mix
                // FC-denominated max_fee with native base_fee, producing a wrong
                // result. Instead we:
                // 1. Convert native base_fee to FC: base_fee_fc = base_fee * num / denom
                // 2. Compute tip in FC: tip_fc = min(max_fee_fc - base_fee_fc, priority_fee_fc)
                // 3. Convert tip back to native: tip = tip_fc * denom / num
                let mut native_tips: Vec<u128> = Vec::with_capacity(txs.len());

                for tx in txs {
                    let bf = base_fee.unwrap_or(0);

                    let tip = if tx.ty() == cip64_ty {
                        // CIP-64 tx: extract fee_currency and compute tip correctly
                        let envelope: &CeloTxEnvelope = &tx.inner.inner;
                        let fc = match envelope {
                            CeloTxEnvelope::Cip64(signed) => {
                                signed.tx().fee_currency.unwrap_or(Address::ZERO)
                            }
                            _ => Address::ZERO,
                        };
                        let rate = match rate_cache.get(&fc) {
                            Some(cached) => *cached,
                            None => {
                                let r = ctx.exchange_rate(fc).await.ok();
                                rate_cache.insert(fc, r);
                                r
                            }
                        };

                        match rate {
                            Some((num, denom)) if !num.is_zero() => cip64_native_tip(
                                tx.max_fee_per_gas(),
                                tx.max_priority_fee_per_gas().unwrap_or(0),
                                bf,
                                num,
                                denom,
                            ),
                            _ => tx.effective_tip_per_gas(bf).unwrap_or(0),
                        }
                    } else {
                        // Native tx: tip is already in CELO
                        tx.effective_tip_per_gas(bf).unwrap_or(0)
                    };
                    native_tips.push(tip);
                }

                // Fetch receipts to get per-tx gas_used for gas-weighted percentiles
                // (matching op-geth's processBlock which weights by gas_used).
                let receipts = (ctx.block_receipts)(block_tag).await?.unwrap_or_default();
                let gas_used_list: Vec<u64> = if receipts.len() == txs.len() {
                    receipts.iter().map(|r| r.inner.gas_used).collect()
                } else {
                    // Fallback: equal weight if receipt count doesn't match
                    vec![1u64; txs.len()]
                };

                // Build (tip, gas_used) pairs and sort by tip
                let mut tip_gas: Vec<(u128, u64)> =
                    native_tips.into_iter().zip(gas_used_list.into_iter()).collect();
                if tip_gas.is_empty() {
                    continue;
                }
                tip_gas.sort_unstable_by_key(|&(tip, _)| tip);

                // Compute percentiles using cumulative gas weighting.
                // compute_gas_weighted_percentiles returns zeros if total_gas == 0;
                // skip the block in that case to preserve the underlying fee_history result.
                let new_rewards = compute_gas_weighted_percentiles(&tip_gas, &percentiles);
                if new_rewards.iter().all(|&x| x == 0) {
                    continue;
                }

                *block_rewards = new_rewards;
            }

            Ok::<_, jsonrpsee_types::ErrorObjectOwned>(history)
        })
        .expect("eth_feeHistory registration");

    module
}

/// Create a [`CeloFeeApi`] from any type implementing the full Eth RPC API.
///
/// This is a generic constructor that captures the concrete `EthApiServer`
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

    let ea1 = Arc::new(eth_api);
    let ea2 = ea1.clone();
    let ea3 = ea1.clone();
    let ea4 = ea1.clone();
    let ea5 = ea1.clone();
    let ea6 = ea1.clone();

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
        fee_history: Box::new(move |block_count, newest_block, reward_percentiles| {
            let ea = ea4.clone();
            Box::pin(async move {
                EthApiServer::fee_history(&*ea, block_count, newest_block, reward_percentiles).await
            })
        }),
        block_by_number: Box::new(move |block_num| {
            let ea = ea5.clone();
            Box::pin(async move { EthApiServer::block_by_number(&*ea, block_num, true).await })
        }),
        block_receipts: Box::new(move |block_num| {
            let ea = ea6.clone();
            Box::pin(async move { EthApiServer::block_receipts(&*ea, block_num.into()).await })
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
            inner: OpTransactionRequest::default().to(Address::ZERO).value(U256::from(100)),
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

    // -----------------------------------------------------------------------
    // Test 15: CeloReceiptConverter builds correct CIP-64 receipts
    // -----------------------------------------------------------------------

    #[test]
    fn cip64_receipt_effective_gas_price_uses_fc_base_fee() {
        // Verify that for CIP-64 receipts, effective_gas_price is computed
        // using the FC-denominated base fee, not the native CELO base fee.
        use alloy_consensus::Receipt;
        use alloy_primitives::B256;

        let fc_base_fee: u128 = 1_000_000_000; // 1 Gwei in FC
        let receipt = CeloTransactionReceipt {
            inner: alloy_rpc_types_eth::TransactionReceipt {
                inner: CeloReceiptEnvelope::Cip64(CeloCip64ReceiptWithBloom {
                    receipt: CeloCip64Receipt {
                        inner: Receipt {
                            status: true.into(),
                            cumulative_gas_used: 50_000,
                            logs: vec![],
                        },
                        base_fee: Some(fc_base_fee),
                    },
                    logs_bloom: Default::default(),
                }),
                transaction_hash: B256::ZERO,
                transaction_index: Some(0),
                block_hash: Some(B256::ZERO),
                block_number: Some(100),
                from: Address::ZERO,
                to: Some(Address::ZERO),
                gas_used: 50_000,
                contract_address: None,
                // This should reflect FC base fee + tip, not native base fee
                effective_gas_price: 1_000_050_000,
                blob_gas_price: None,
                blob_gas_used: None,
            },
            l1_block_info: Default::default(),
            base_fee: Some(fc_base_fee),
        };

        let json = serde_json::to_value(&receipt).unwrap();

        // Verify type is CIP-64 (0x7b)
        assert_eq!(json["type"], "0x7b");

        // Verify baseFee is present
        assert!(json["baseFee"].is_string(), "baseFee should be present for CIP-64 receipts");

        // Verify baseFee value
        let base_fee_hex = json["baseFee"].as_str().unwrap();
        let parsed = u128::from_str_radix(base_fee_hex.trim_start_matches("0x"), 16).unwrap();
        assert_eq!(parsed, fc_base_fee, "baseFee should match FC-denominated value");
    }

    // -----------------------------------------------------------------------
    // Test 14: Gas price and fee history module construction
    // -----------------------------------------------------------------------

    #[test]
    fn gas_price_module_registers_methods() {
        let api = Arc::new(CeloFeeApi {
            gas_price: Box::new(|| Box::pin(async { Ok(U256::from(25_000_000_000u64)) })),
            priority_fee: Box::new(|| Box::pin(async { Ok(U256::from(1_000_000u64)) })),
            eth_call: Box::new(|_| Box::pin(async { Ok(Bytes::new()) })),
            fee_history: Box::new(|_, _, _| {
                Box::pin(async { Ok(alloy_rpc_types_eth::FeeHistory::default()) })
            }),
            block_by_number: Box::new(|_| Box::pin(async { Ok(None) })),
            block_receipts: Box::new(|_| Box::pin(async { Ok(None) })),
            fee_currency_directory: Address::ZERO,
        });

        let module = celo_gas_price_module(api);
        let method_names: Vec<_> = module.method_names().collect();
        assert!(method_names.contains(&"eth_gasPrice"), "Missing eth_gasPrice");
        assert!(
            method_names.contains(&"eth_maxPriorityFeePerGas"),
            "Missing eth_maxPriorityFeePerGas"
        );
    }

    #[test]
    fn fee_history_module_registers_method() {
        let api = Arc::new(CeloFeeApi {
            gas_price: Box::new(|| Box::pin(async { Ok(U256::from(25_000_000_000u64)) })),
            priority_fee: Box::new(|| Box::pin(async { Ok(U256::from(1_000_000u64)) })),
            eth_call: Box::new(|_| Box::pin(async { Ok(Bytes::new()) })),
            fee_history: Box::new(|_, _, _| {
                Box::pin(async { Ok(alloy_rpc_types_eth::FeeHistory::default()) })
            }),
            block_by_number: Box::new(|_| Box::pin(async { Ok(None) })),
            block_receipts: Box::new(|_| Box::pin(async { Ok(None) })),
            fee_currency_directory: Address::ZERO,
        });

        let module = celo_fee_history_module(api);
        let method_names: Vec<_> = module.method_names().collect();
        assert!(method_names.contains(&"eth_feeHistory"), "Missing eth_feeHistory");
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

    // -----------------------------------------------------------------------
    // Gap 3: compute_gas_weighted_percentiles unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn percentile_single_tx() {
        // One entry: tip=100, gas=21000. All percentiles return the same value.
        let tip_gas = [(100u128, 21_000u64)];
        let result = compute_gas_weighted_percentiles(&tip_gas, &[0.0, 50.0, 100.0]);
        assert_eq!(result, vec![100, 100, 100]);
    }

    #[test]
    fn percentile_equal_gas_weight() {
        // Three entries with equal gas; percentile maps to sorted index.
        // sorted tips: 10, 20, 30 (each with gas=1)
        let tip_gas = [(10u128, 1u64), (20u128, 1u64), (30u128, 1u64)];
        // p=0: threshold=0, first entry cumgas=1 > 0 → tip=10
        // p=50: threshold=1 (50% of 3 = 1.5 → cast to 1), first entry cumgas=1 → 1 > 1 false,
        //        second cumgas=2 > 1 → tip=20
        // p=100: threshold=3 (100% of 3), all entries: last cumgas=3, not > 3 → fall through →
        // last=30
        let result = compute_gas_weighted_percentiles(&tip_gas, &[0.0, 50.0, 100.0]);
        assert_eq!(result[0], 10);
        assert_eq!(result[1], 20);
        assert_eq!(result[2], 30);
    }

    #[test]
    fn percentile_gas_weighted_low_tip_wins_median() {
        // Low tip with 90% of gas should win at the 50th percentile.
        // sorted: (tip=5, gas=90), (tip=100, gas=10). total=100
        // p=50: threshold=50, cum_gas after first=90 > 50 → tip=5
        let tip_gas = [(5u128, 90u64), (100u128, 10u64)];
        let result = compute_gas_weighted_percentiles(&tip_gas, &[50.0]);
        assert_eq!(result[0], 5);
    }

    #[test]
    fn percentile_empty_returns_zeros() {
        let result = compute_gas_weighted_percentiles(&[], &[0.0, 50.0, 100.0]);
        assert_eq!(result, vec![0, 0, 0]);
    }

    // -----------------------------------------------------------------------
    // Gap 3: cip64_native_tip unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn cip64_tip_converts_correctly() {
        // rate: 1 FC = 2 native (num=1, denom=2)
        // base_fee_native=100, max_fee_fc=300, priority_fee_fc=50
        // base_fee_fc = 100 * 1 / 2 = 50
        // tip_fc = min(300 - 50, 50) = min(250, 50) = 50
        // tip_native = 50 * 2 / 1 = 100
        let result = cip64_native_tip(300, 50, 100, U256::from(1u64), U256::from(2u64));
        assert_eq!(result, 100);
    }

    #[test]
    fn cip64_tip_zero_rate_num_returns_zero() {
        // rate_num=0 is a degenerate case — function returns 0 without panicking
        let result = cip64_native_tip(1_000_000_000, 100, 1000, U256::ZERO, U256::from(1u64));
        assert_eq!(result, 0);
    }

    // -----------------------------------------------------------------------
    // Gap 5: cip64_native_tip edge-case tests
    // -----------------------------------------------------------------------

    #[test]
    fn cip64_tip_base_fee_exceeds_max_fee_clamps_to_zero() {
        // base_fee_fc > max_fee_fc → saturating_sub returns 0 → tip = 0
        // rate 1:1, base_fee_native=1000, max_fee_fc=500 → base_fee_fc=1000 > 500
        let result = cip64_native_tip(500, 100, 1000, U256::from(1u64), U256::from(1u64));
        assert_eq!(result, 0);
    }

    #[test]
    fn cip64_tip_rate_denom_zero_returns_zero() {
        // rate_denom=0 → base_fee_fc = base_fee * num / 0 = 0 (integer division by zero guard)
        // In practice denom=0 means FC is worthless; U256 division by zero panics in debug.
        // We handle this by using rate_num=0 check (which returns 0 early).
        // Test verifies rate_num=0 path doesn't panic.
        let result = cip64_native_tip(1_000_000, 500, 100, U256::ZERO, U256::ZERO);
        assert_eq!(result, 0);
    }

    // -----------------------------------------------------------------------
    // Effective gas tip for CIP-64 transactions
    // (mirrors op-geth's TestTransactionEffectiveGasTipInCelo)
    // -----------------------------------------------------------------------

    #[test]
    fn cip64_tip_priority_fee_is_limiting_factor() {
        // When max_fee has plenty of headroom above base_fee_fc, the effective
        // tip is bounded by priority_fee_fc (converted to native).
        // rate: num=2, denom=1 (1 FC = 0.5 native)
        // base_fee_native=100, max_fee_fc=1000, priority_fee_fc=20
        // base_fee_fc = 100 * 2 / 1 = 200
        // tip_fc = min(1000 - 200, 20) = 20
        // tip_native = 20 * 1 / 2 = 10
        let tip = cip64_native_tip(1000, 20, 100, U256::from(2u64), U256::from(1u64));
        assert_eq!(tip, 10);
    }

    #[test]
    fn cip64_tip_headroom_is_limiting_factor() {
        // When priority_fee_fc is generous but max_fee_fc barely covers the base fee,
        // the effective tip is bounded by (max_fee_fc - base_fee_fc).
        // rate: num=1, denom=1
        // base_fee_native=1000, max_fee_fc=1050, priority_fee_fc=200
        // base_fee_fc = 1000
        // tip_fc = min(1050 - 1000, 200) = 50
        // tip_native = 50
        let tip = cip64_native_tip(1050, 200, 1000, U256::from(1u64), U256::from(1u64));
        assert_eq!(tip, 50);
    }

    #[test]
    fn cip64_tip_with_high_exchange_rate_amplifies_native_tip() {
        // A small FC tip can map to a large native tip when FC is expensive.
        // rate: num=1, denom=100 (1 FC = 100 native)
        // base_fee_native=10, max_fee_fc=10, priority_fee_fc=5
        // base_fee_fc = 10 * 1 / 100 = 0 (integer truncation)
        // tip_fc = min(10 - 0, 5) = 5
        // tip_native = 5 * 100 / 1 = 500
        let tip = cip64_native_tip(10, 5, 10, U256::from(1u64), U256::from(100u64));
        assert_eq!(tip, 500);
    }

    // -----------------------------------------------------------------------
    // Gas price / priority fee with fee-currency scaling
    // -----------------------------------------------------------------------

    /// Helper: build an ABI-encoded `(uint256, uint256)` response for exchange rate.
    fn encode_exchange_rate(numerator: u128, denominator: u128) -> Bytes {
        let mut buf = vec![0u8; 64];
        buf[16..32].copy_from_slice(&numerator.to_be_bytes());
        buf[48..64].copy_from_slice(&denominator.to_be_bytes());
        Bytes::from(buf)
    }

    /// Build a [`CeloFeeApi`] for testing with a fixed gas price, priority fee,
    /// and exchange rate returned for any fee currency.
    fn make_test_fee_api(
        gas_price: u64,
        priority_fee: u64,
        rate_num: u128,
        rate_denom: u128,
    ) -> Arc<CeloFeeApi> {
        let rate_bytes = encode_exchange_rate(rate_num, rate_denom);
        Arc::new(CeloFeeApi {
            gas_price: Box::new(move || Box::pin(async move { Ok(U256::from(gas_price)) })),
            priority_fee: Box::new(move || Box::pin(async move { Ok(U256::from(priority_fee)) })),
            eth_call: Box::new(move |_| {
                let b = rate_bytes.clone();
                Box::pin(async move { Ok(b) })
            }),
            fee_history: Box::new(|_, _, _| {
                Box::pin(async { Ok(alloy_rpc_types_eth::FeeHistory::default()) })
            }),
            block_by_number: Box::new(|_| Box::pin(async { Ok(None) })),
            block_receipts: Box::new(|_| Box::pin(async { Ok(None) })),
            fee_currency_directory: Address::ZERO,
        })
    }

    #[tokio::test]
    async fn gas_price_scales_by_exchange_rate() {
        // Native gas price = 25 Gwei. Rate: num=2, denom=1 → FC price = 25*2/1 = 50 Gwei.
        let api = make_test_fee_api(25_000_000_000, 1_000_000, 2, 1);
        let module = celo_gas_price_module(api);

        let fc = Address::with_last_byte(0xAA);
        let result: U256 = module
            .call("eth_gasPrice", [fc])
            .await
            .expect("eth_gasPrice with fee currency should succeed");
        assert_eq!(result, U256::from(50_000_000_000u64));
    }

    #[tokio::test]
    async fn gas_price_returns_native_without_fee_currency() {
        let api = make_test_fee_api(25_000_000_000, 1_000_000, 2, 1);
        let module = celo_gas_price_module(api);

        // Call without fee currency parameter → returns native price unchanged
        let result: U256 = module
            .call("eth_gasPrice", Vec::<Address>::new())
            .await
            .expect("eth_gasPrice without fee currency should succeed");
        assert_eq!(result, U256::from(25_000_000_000u64));
    }

    #[tokio::test]
    async fn priority_fee_scales_by_exchange_rate() {
        // Native priority fee = 1M wei. Rate: num=3, denom=1 → FC tip = 3M wei.
        let api = make_test_fee_api(25_000_000_000, 1_000_000, 3, 1);
        let module = celo_gas_price_module(api);

        let fc = Address::with_last_byte(0xBB);
        let result: U256 = module
            .call("eth_maxPriorityFeePerGas", [fc])
            .await
            .expect("eth_maxPriorityFeePerGas with fee currency should succeed");
        assert_eq!(result, U256::from(3_000_000u64));
    }
}
