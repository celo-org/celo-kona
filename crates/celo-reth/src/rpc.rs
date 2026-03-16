//! Celo-specific RPC types and converters for reth.
//!
//! Provides [`CeloRpcTypes`], [`CeloReceiptConverter`], and [`CeloEthApiBuilder`] —
//! the Celo equivalents of the op-reth `Optimism` network type, `OpReceiptConverter`,
//! and `OpEthApiBuilder`.

use crate::{primitives::CeloPrimitives, receipt::CeloReceipt};
use alloy_consensus::{TxReceipt, error::ValueError};
use alloy_evm::{env::BlockEnvironment, rpc::EthTxEnvError, EvmEnv};
use alloy_network::TxSigner;
use alloy_primitives::{Address, Bytes, Signature, U256, keccak256};
use alloy_rpc_types_eth::{Log, TransactionInput, request::TransactionRequest};
use celo_alloy_consensus::CeloTxEnvelope;
use celo_revm::CeloTransaction;
use op_alloy_consensus::OpReceipt;
use op_alloy_rpc_types::{OpTransactionReceipt, OpTransactionRequest};
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_evm::ConfigureEvm;
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_node_builder::rpc::{EthApiBuilder, EthApiCtx};
use reth_optimism_forks::OpHardforks;
use reth_optimism_rpc::{
    OpEthApi,
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
    type Receipt = OpTransactionReceipt;
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
            .map(op_tx_to_celo)
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
        SignableTxRequest::<op_alloy_consensus::OpTxEnvelope>::try_build_and_sign(
            self.inner, signer,
        )
        .await
        .map(op_tx_to_celo)
    }
}

// ---------------------------------------------------------------------------
// CeloReceiptConverter
// ---------------------------------------------------------------------------

/// Converter for Celo receipts, producing [`OpTransactionReceipt`] RPC representations.
///
/// Analogous to [`OpReceiptConverter`](reth_optimism_rpc::eth::receipt::OpReceiptConverter)
/// but handles the additional [`CeloReceipt::Cip64`] variant.
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
    type RpcReceipt = OpTransactionReceipt;
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
    /// Converts a single [`ConvertReceiptInput`] with [`CeloReceipt`] to an
    /// [`OpTransactionReceipt`].
    fn build_receipt<ChainSpec: OpHardforks>(
        chain_spec: &ChainSpec,
        input: ConvertReceiptInput<'_, CeloPrimitives>,
        l1_block_info: &mut op_revm::L1BlockInfo,
    ) -> Result<OpTransactionReceipt, reth_optimism_rpc::OpEthApiError> {
        use alloy_consensus::Receipt;

        let timestamp = input.meta.timestamp;
        let block_number = input.meta.block_number;
        let tx_signed = *input.tx.inner();

        let core_receipt = build_receipt(input, None, |celo_receipt, next_log_index, meta| {
            let map_logs = move |receipt: alloy_consensus::Receipt<alloy_primitives::Log>| {
                let Receipt { status, cumulative_gas_used, logs } = receipt;
                let logs = Log::collect_for_receipt(next_log_index, meta, logs);
                Receipt { status, cumulative_gas_used, logs }
            };

            let op_receipt: OpReceipt<Log> = match celo_receipt {
                CeloReceipt::Legacy(r) => OpReceipt::Legacy(map_logs(r)),
                CeloReceipt::Eip2930(r) => OpReceipt::Eip2930(map_logs(r)),
                CeloReceipt::Eip1559(r) => OpReceipt::Eip1559(map_logs(r)),
                CeloReceipt::Eip7702(r) => OpReceipt::Eip7702(map_logs(r)),
                // TODO: Use cip64.base_fee to calculate the effective gas price for
                // CIP-64 receipts. For now, CIP-64 receipts are reported as EIP-1559.
                CeloReceipt::Cip64(cip64) => OpReceipt::Eip1559(map_logs(cip64.inner)),
                CeloReceipt::Deposit(d) => {
                    OpReceipt::Deposit(d.map_inner(|inner| map_logs(inner)))
                }
            };

            op_receipt.into_with_bloom()
        });

        let op_fields = OpReceiptFieldsBuilder::new(timestamp, block_number)
            .l1_block_info(chain_spec, tx_signed, l1_block_info)?
            .build();

        Ok(OpTransactionReceipt {
            inner: core_receipt,
            l1_block_info: op_fields.l1_block_info,
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
#[non_exhaustive]
pub struct CeloEthApiBuilder;

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
        let rpc_converter =
            RpcConverter::new(CeloReceiptConverter::new(ctx.components.provider().clone()))
                .with_mapper(OpTxInfoMapper::new(ctx.components.provider().clone()));

        let eth_api = ctx.eth_api_builder().with_rpc_converter(rpc_converter).build_inner();

        Ok(OpEthApi::new(eth_api, None, U256::from(1_000_000_u64), None))
    }
}

// ---------------------------------------------------------------------------
// Fee-currency-aware gas price RPCs
// ---------------------------------------------------------------------------

use crate::FEE_CURRENCY_DIRECTORY;

/// Type-erased wrapper for the parts of the Eth API we need in the gas price
/// RPC overrides. This avoids leaking the heavily-parameterised [`EthApiServer`]
/// types into the RPC module registration.
pub struct CeloFeeApi {
    gas_price: Box<dyn Fn() -> Pin<Box<dyn Future<Output = jsonrpsee::core::RpcResult<U256>> + Send>> + Send + Sync>,
    priority_fee: Box<dyn Fn() -> Pin<Box<dyn Future<Output = jsonrpsee::core::RpcResult<U256>> + Send>> + Send + Sync>,
    eth_call: Box<dyn Fn(CeloTransactionRequest) -> Pin<Box<dyn Future<Output = jsonrpsee::core::RpcResult<Bytes>> + Send>> + Send + Sync>,
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
                .to(FEE_CURRENCY_DIRECTORY)
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
pub fn make_celo_fee_api<Api>(eth_api: Api) -> CeloFeeApi
where
    Api: reth_rpc_eth_api::EthApiServer<
            CeloTransactionRequest,
            op_alloy_rpc_types::Transaction<CeloTxEnvelope>,
            alloy_rpc_types_eth::Block<
                op_alloy_rpc_types::Transaction<CeloTxEnvelope>,
                alloy_rpc_types_eth::Header,
            >,
            OpTransactionReceipt,
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
