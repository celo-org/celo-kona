
use db_key::Key;
use leveldb::{database::Database, kv::KV, options::ReadOptions, options::Options};
use alloy_primitives::hex;
use kona_mpt::{TrieNode, TrieProvider};
use kona_executor::{
    TrieDBProvider,
    // test_utils::{
    //     DiskTrieNodeProvider, ExecutorTestFixture as OpExecutorTestFixture,
    //     ExecutorTestFixtureCreator as OpExecutorTestFixtureCreator, TestTrieNodeProviderError,
    // },
};
use alloy_primitives::{B256, Bytes};
use alloy_consensus::Header;
use alloy_rlp::Decodable;

use std::path::Path;

/// LevelDBAndRPCTrieDBProvider
#[allow(missing_debug_implementations)]
pub(crate) struct LevelDBTrieProvider {
    /// The LevelDB database.
    db: Database<LevelDBKey>,
}

use thiserror::Error;

/// Error type for the TrieDBProvider.
#[derive(Debug, Error)]
#[error("TrieDBProviderError: {error}")]
pub(crate) enum TrieDBProviderError {
    /// Indicates that the key was not found in the database.
    #[error("Key not found: {0}")]
    KeyNotFound(LevelDBKey),
    /// Indicates that an error occurred while reading from the database.
    #[error("Error: {0}")]
    Error(String),
}

impl LevelDBTrieProvider {
    /// Constructs a new LevelDBAndRPCTrieDBProvider.
    pub(crate) fn new(path: &Path) -> Self {
        let db = Database::open(path, Options::new()).unwrap();
        Self { db }
    }
}


impl TrieDBProvider for LevelDBTrieProvider {
    fn bytecode_by_hash(&self, hash: B256) -> Result<Bytes, Self::Error> {

        const CODE_PREFIX: u8 = b'c';
        let code_hash = [&[CODE_PREFIX], hash.as_slice()].concat();
        let key = LevelDBKey::new(code_hash.to_vec());
        // self.db.get(options, key).map_err(|e| LevelDBTrieDBProviderError { error: e.to_string() }).unwrap().map(Bytes::from)
        // or_else(|| LevelDBTrieDBProviderError::KeyNotFound(hash)).map(Bytes::from)
        self.db
            .get(ReadOptions::new(), &key)
            .map_err(|e| TrieDBProviderError::Error(e.to_string()))?
            .ok_or_else(|| TrieDBProviderError::KeyNotFound(key))
            .map(Bytes::from)
    }


// TODO figure out how to construct the header key and get the header from the db
    fn header_by_hash(&self, hash: B256) -> Result<Header, Self::Error> {
        panic!("Not implemented, should not be called");
        // let options = ReadOptions::new();
        // let key = LevelDBKey::new(hash.to_vec());
        // self.op_executor_test_fixture_creator.header_by_hash(hash).ok_or_else(|| LevelDBTrieDBProviderError::KeyNotFound(hash)).map(Bytes::from)
        // self.db.get(options, key).map_err(|e| LevelDBTrieDBProviderError::Error(e.to_string()))?.ok_or_else(|| LevelDBTrieDBProviderError::KeyNotFound(hash)).map(Bytes::from)
    }
}


impl TrieProvider for LevelDBTrieProvider {
    type Error = TrieDBProviderError;

    fn trie_node_by_hash(&self, hash: B256) -> Result<TrieNode, Self::Error> {
        let key = LevelDBKey::new(hash.to_vec());
        let trie_node_bytes = self
            .db
            .get(ReadOptions::new(), &key)
            .map_err(|e| TrieDBProviderError::Error(e.to_string()))?
            .ok_or_else(|| TrieDBProviderError::KeyNotFound(key))
            .map(Bytes::from)?;
        // Decode the preimage into a trie node.
        TrieNode::decode(&mut trie_node_bytes.as_ref())
            .map_err(|e| TrieDBProviderError::Error(e.to_string()))
    }
}

/// LevelDBKey is a key for the LevelDB database.
#[derive(Debug)]
struct LevelDBKey {
    key: Vec<u8>,
}

impl LevelDBKey {
    /// Constructs a new key from a byte vector.
    fn new(key: Vec<u8>) -> Self {
        Self { key }
    }
}

impl Key for LevelDBKey {
    fn from_u8(key: &[u8]) -> Self {
        Self { key: key.to_vec() }
    }

    fn as_slice<T, F: Fn(&[u8]) -> T>(&self, f: F) -> T {
        f(self.key.as_slice())
    }
}

impl std::fmt::Display for LevelDBKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.key.as_slice()))
    }
}