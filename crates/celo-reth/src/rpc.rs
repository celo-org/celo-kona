//! Celo-specific RPC types and converters for reth.
//!
//! Provides [`CeloRpcTypes`], [`CeloReceiptConverter`], and [`CeloEthApiBuilder`] —
//! the Celo equivalents of the op-reth `Optimism` network type, `OpReceiptConverter`,
//! and `OpEthApiBuilder`.

use crate::{primitives::CeloPrimitives, receipt::CeloReceipt};
use alloy_consensus::{TxReceipt, error::ValueError};
use alloy_evm::{env::BlockEnvironment, rpc::EthTxEnvError, EvmEnv};
use alloy_network::TxSigner;
use alloy_primitives::{Signature, U256};
use alloy_rpc_types_eth::{Log, request::TransactionRequest};
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

/// Newtype around [`OpTransactionRequest`] for use with [`CeloRpcTypes`].
///
/// This local type allows implementing foreign traits (e.g. [`SignableTxRequest`])
/// for a combination of foreign trait + foreign type, working around the orphan rule.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct CeloTransactionRequest(pub OpTransactionRequest);

impl AsRef<TransactionRequest> for CeloTransactionRequest {
    fn as_ref(&self) -> &TransactionRequest {
        self.0.as_ref()
    }
}

impl AsMut<TransactionRequest> for CeloTransactionRequest {
    fn as_mut(&mut self) -> &mut TransactionRequest {
        self.0.as_mut()
    }
}

impl TryIntoSimTx<CeloTxEnvelope> for CeloTransactionRequest {
    fn try_into_sim_tx(self) -> Result<CeloTxEnvelope, ValueError<Self>> {
        self.0
            .try_into_sim_tx()
            .map(op_tx_to_celo)
            .map_err(|e| e.map(CeloTransactionRequest))
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
        let op_tx: op_revm::OpTransaction<TxEnv> = self.0.try_into_tx_env(evm_env)?;
        Ok(CeloTransaction::new(op_tx))
    }
}

impl SignableTxRequest<CeloTxEnvelope> for CeloTransactionRequest {
    async fn try_build_and_sign(
        self,
        signer: impl TxSigner<Signature> + Send,
    ) -> Result<CeloTxEnvelope, SignTxRequestError> {
        SignableTxRequest::<op_alloy_consensus::OpTxEnvelope>::try_build_and_sign(self.0, signer)
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
