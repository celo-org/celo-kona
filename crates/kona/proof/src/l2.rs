//! Contains the concrete implementation of the [CeloOracleL2ChainProvider] trait for the client
//! program.
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use alloy_consensus::{BlockBody, Header};
use alloy_eips::Decodable2718;
use alloy_primitives::{Address, B256, Bytes};
use async_trait::async_trait;
use celo_alloy_consensus::{CeloBlock, CeloTxEnvelope};
use celo_protocol::{
    CeloBatchValidationProvider, CeloL2BlockInfo, CeloL2ChainProvider,
    convert_celo_block_to_op_block,
};
use kona_driver::PipelineCursor;
use kona_executor::TrieDBProvider;
use kona_genesis::{RollupConfig, SystemConfig};
use kona_mpt::{OrderedListWalker, TrieHinter, TrieNode, TrieProvider};
use kona_preimage::CommsClient;
use kona_proof::{
    HintType, eip_2935_history_lookup, errors::OracleProviderError, l2::OracleL2ChainProvider,
};
use kona_protocol::{L2BlockInfo, to_system_config};
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use spin::RwLock;

/// The oracle-backed L2 chain provider for the client program.
#[derive(Debug, Clone)]
pub struct CeloOracleL2ChainProvider<T: CommsClient> {
    /// The L2 safe head block hash.
    l2_head: B256,
    /// The rollup configuration.
    rollup_config: Arc<RollupConfig>,
    /// The preimage oracle client.
    oracle: Arc<T>,
    /// The derivation pipeline cursor
    cursor: Option<Arc<RwLock<PipelineCursor>>>,
    /// The L2 chain ID to use for the provider's hints.
    chain_id: Option<u64>,
}

impl<T: CommsClient> CeloOracleL2ChainProvider<T> {
    /// Creates a new [CeloOracleL2ChainProvider] with the given boot information and oracle client.
    pub const fn new(l2_head: B256, rollup_config: Arc<RollupConfig>, oracle: Arc<T>) -> Self {
        Self { l2_head, rollup_config, oracle, cursor: None, chain_id: None }
    }

    /// Sets the L2 chain ID to use for the provider's hints.
    pub const fn set_chain_id(&mut self, chain_id: Option<u64>) {
        self.chain_id = chain_id;
    }

    /// Updates the derivation pipeline cursor
    pub fn set_cursor(&mut self, cursor: Arc<RwLock<PipelineCursor>>) {
        self.cursor = Some(cursor);
    }

    /// Fetches the latest known safe head block hash according to the derivation pipeline cursor
    /// or uses the initial l2_head value if no cursor is set.
    pub async fn l2_safe_head(&self) -> Result<B256, OracleProviderError> {
        self.cursor
            .as_ref()
            .map_or(Ok(self.l2_head), |cursor| Ok(cursor.read().l2_safe_head().block_info.hash))
    }

    /// Converts CeloOracleL2ChainProvider to OracleL2ChainProvider
    pub fn to_oracle_l2_chain_provider(&self) -> OracleL2ChainProvider<T> {
        let mut provider = OracleL2ChainProvider::new(
            self.l2_head,
            Arc::clone(&self.rollup_config),
            Arc::clone(&self.oracle),
        );
        provider.set_chain_id(self.chain_id);
        if let Some(ref cursor) = self.cursor {
            provider.set_cursor(Arc::clone(cursor));
        }
        provider
    }
}

impl<T: CommsClient> CeloOracleL2ChainProvider<T> {
    /// Returns a [Header] corresponding to the given L2 block number, by walking back from the
    /// L2 safe head.
    /// Re-implement here because header_by_number is private in OracleL2ChainProvider
    async fn header_by_number(&mut self, block_number: u64) -> Result<Header, OracleProviderError> {
        // Fetch the starting block header.
        let mut header = self.header_by_hash(self.l2_safe_head().await?)?;

        // Check if the block number is in range. If not, we can fail early.
        if block_number > header.number {
            return Err(OracleProviderError::BlockNumberPastHead(block_number, header.number));
        }

        let mut linear_fallback = false;
        while header.number > block_number {
            if self.rollup_config.is_isthmus_active(header.timestamp) && !linear_fallback {
                // If Isthmus is active, the EIP-2935 contract is used to perform leaping lookbacks
                // through consulting the ring buffer within the contract. If this
                // lookup fails for any reason, we fall back to linear walk back.
                let block_hash =
                    match eip_2935_history_lookup(&header, block_number, self, self).await {
                        Ok(hash) => hash,
                        Err(_) => {
                            // If the EIP-2935 lookup fails for any reason, attempt fallback to
                            // linear walk back.
                            linear_fallback = true;
                            continue;
                        }
                    };

                header = self.header_by_hash(block_hash)?;
            } else {
                // Walk back the block headers one-by-one until the desired block number is reached.
                header = self.header_by_hash(header.parent_hash)?;
            }
        }

        Ok(header)
    }
}

#[async_trait]
impl<T: CommsClient + Send + Sync> CeloBatchValidationProvider for CeloOracleL2ChainProvider<T> {
    type Error = OracleProviderError;

    async fn l2_block_info_by_number(
        &mut self,
        number: u64,
    ) -> Result<L2BlockInfo, OracleProviderError> {
        // Get the block at the given number.
        let block = self.block_by_number(number).await?;
        // Construct the system config from the payload.
        CeloL2BlockInfo::from_block_and_genesis(&block, &self.rollup_config.genesis)
            // Convert CeloL2BlockInfo to L2BlockInfo to match the original interface
            .map(|celo_info| celo_info.op_l2_block_info)
            .map_err(OracleProviderError::BlockInfo)
    }

    async fn block_by_number(&mut self, number: u64) -> Result<CeloBlock, OracleProviderError> {
        // Fetch the header for the given block number.
        let header @ Header { transactions_root, timestamp, .. } =
            self.header_by_number(number).await?;
        let header_hash = header.hash_slow();

        // Fetch the transactions in the block.
        HintType::L2Transactions
            .with_data(&[header_hash.as_ref()])
            .with_data(self.chain_id.map_or_else(Vec::new, |id| id.to_be_bytes().to_vec()))
            .send(self.oracle.as_ref())
            .await?;
        let trie_walker = OrderedListWalker::try_new_hydrated(transactions_root, self)
            .map_err(OracleProviderError::TrieWalker)?;
        // Decode the transactions within the transactions trie.
        let transactions = trie_walker
            .into_iter()
            // Use CeloTxEnvelope decoder instead of CeloTxEnvelope's one to decode CIP-64 tx
            .map(|(_, rlp)| Ok(CeloTxEnvelope::decode_2718(&mut rlp.as_ref())?))
            .collect::<Result<Vec<_>, _>>()
            .map_err(OracleProviderError::Rlp)?;
        let optimism_block = CeloBlock {
            header,
            body: BlockBody {
                transactions,
                ommers: Vec::new(),
                withdrawals: self
                    .rollup_config
                    .is_canyon_active(timestamp)
                    .then(|| alloy_eips::eip4895::Withdrawals::new(Vec::new())),
            },
        };
        Ok(optimism_block)
    }
}

#[async_trait]
impl<T: CommsClient + Send + Sync> CeloL2ChainProvider for CeloOracleL2ChainProvider<T> {
    type Error = OracleProviderError;

    async fn system_config_by_number(
        &mut self,
        number: u64,
        rollup_config: Arc<RollupConfig>,
    ) -> Result<SystemConfig, <Self as CeloL2ChainProvider>::Error> {
        let block = self.block_by_number(number).await?;
        // Construct the system config from the payload.
        // `CeloBlock`` can be safely converted to `OpBlock`` here
        // since `to_system_config`` depends solely on the block header (and not on transactions)
        to_system_config(&convert_celo_block_to_op_block(block), rollup_config.as_ref())
            .map_err(OracleProviderError::OpBlockConversion)
    }
}

impl<T: CommsClient> TrieProvider for CeloOracleL2ChainProvider<T> {
    type Error = OracleProviderError;

    fn trie_node_by_hash(&self, key: B256) -> Result<TrieNode, OracleProviderError> {
        self.to_oracle_l2_chain_provider().trie_node_by_hash(key)
    }
}

impl<T: CommsClient> TrieDBProvider for CeloOracleL2ChainProvider<T> {
    fn bytecode_by_hash(&self, hash: B256) -> Result<Bytes, OracleProviderError> {
        self.to_oracle_l2_chain_provider().bytecode_by_hash(hash)
    }

    fn header_by_hash(&self, hash: B256) -> Result<Header, OracleProviderError> {
        self.to_oracle_l2_chain_provider().header_by_hash(hash)
    }
}

impl<T: CommsClient> TrieHinter for CeloOracleL2ChainProvider<T> {
    type Error = OracleProviderError;

    fn hint_trie_node(&self, hash: B256) -> Result<(), Self::Error> {
        self.to_oracle_l2_chain_provider().hint_trie_node(hash)
    }

    fn hint_account_proof(&self, address: Address, block_number: u64) -> Result<(), Self::Error> {
        self.to_oracle_l2_chain_provider().hint_account_proof(address, block_number)
    }

    fn hint_storage_proof(
        &self,
        address: alloy_primitives::Address,
        slot: alloy_primitives::U256,
        block_number: u64,
    ) -> Result<(), Self::Error> {
        self.to_oracle_l2_chain_provider().hint_storage_proof(address, slot, block_number)
    }

    fn hint_execution_witness(
        &self,
        parent_hash: B256,
        op_payload_attributes: &OpPayloadAttributes,
    ) -> Result<(), Self::Error> {
        self.to_oracle_l2_chain_provider()
            .hint_execution_witness(parent_hash, op_payload_attributes)
    }
}
