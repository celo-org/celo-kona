use alloy_primitives::{B256, Bytes};
use alloy_provider::{Provider, RootProvider};
use alloy_rlp::Decodable;
use tokio::runtime::Handle;
use kona_mpt::{TrieNode, TrieProvider};
use alloy_network::Ethereum;
use alloy_consensus::Header;
use kona_executor::TrieDBProvider;

/// A trie provider for the L2 execution layer.
#[derive(Debug)]
pub(crate)struct RPCTrieProvider<'a> {
    /// The RPC provider for the L2 execution layer.
    provider: &'a RootProvider<Ethereum>,
}

impl<'a> RPCTrieProvider<'a> {
    /// Create a new [`Trie`] instance.
    pub(crate)fn new(provider: &'a RootProvider) -> Self {
        Self { provider }
    }
}

impl<'a> TrieProvider for &RPCTrieProvider<'a> {
    type Error = TrieError;

    fn trie_node_by_hash(&self, key: B256) -> Result<TrieNode, Self::Error> {
        // Fetch the preimage from the L2 chain provider.
        let preimage: Bytes = tokio::task::block_in_place(move || {
            Handle::current().block_on(async {
                let preimage: Bytes = self
                    .provider
                    .client()
                    .request("debug_dbGet", &[key])
                    .await
                    .map_err(|_| TrieError::PreimageNotFound)?;

                Ok(preimage)
            })
        })?;

        // Decode the preimage into a trie node.
        TrieNode::decode(&mut preimage.as_ref()).map_err(TrieError::Rlp)
    }
}

impl<'a> TrieDBProvider for &RPCTrieProvider<'a> {
    fn bytecode_by_hash(&self, hash: B256) -> Result<Bytes, Self::Error> {
        // geth hashdb scheme code hash key prefix
        const CODE_PREFIX: u8 = b'c';

        // Fetch the preimage from the L2 chain provider.
        let preimage: Bytes = tokio::task::block_in_place(move || {
            Handle::current().block_on(async {
                // Attempt to fetch the code from the L2 chain provider.
                let code_hash = [&[CODE_PREFIX], hash.as_slice()].concat();
                let code = self
                    .provider
                    .client()
                    .request::<&[Bytes; 1], Bytes>("debug_dbGet", &[code_hash.into()])
                    .await;

                // Check if the first attempt to fetch the code failed. If it did, try fetching the
                // code hash preimage without the geth hashdb scheme prefix.
                let code = match code {
                    Ok(code) => code,
                    Err(_) => self
                        .provider
                        .client()
                        .request::<&[B256; 1], Bytes>("debug_dbGet", &[hash])
                        .await
                        .map_err(|_| TrieError::PreimageNotFound)?,
                };

                Ok(code)
            })
        })?;

        Ok(preimage)
    }

    fn header_by_hash(&self, hash: B256) -> Result<Header, Self::Error> {
        let encoded_header: Bytes = tokio::task::block_in_place(move || {
            Handle::current().block_on(async {
                let preimage: Bytes = self
                    .provider
                    .client()
                    .request("debug_getRawHeader", &[hash])
                    .await
                    .map_err(|_| TrieError::PreimageNotFound)?;

                Ok(preimage)
            })
        })?;

        // Decode the Header.
        Header::decode(&mut encoded_header.as_ref()).map_err(TrieError::Rlp)
    }
}

/// An error type for the [`DiskTrieNodeProvider`] and [`ExecutorTestFixtureCreator`].
#[derive(Debug, thiserror::Error)]
pub(crate)enum TrieError {
    /// The preimage was not found in the key-value store.
    #[error("Preimage not found")]
    PreimageNotFound,
    /// Failed to decode the RLP-encoded data.
    #[error("Failed to decode RLP: {0}")]
    Rlp(alloy_rlp::Error),
    /// Failed to write back to the key-value store.
    #[error("Failed to write back to key value store")]
    KVStore,
}