use rusty_leveldb::{DB,Options, Snapshot};
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
use std::sync::Mutex;

/// LevelDBAndRPCTrieDBProvider
#[allow(missing_debug_implementations)]
pub(crate) struct LevelDBTrieProvider {
    /// The LevelDB database.
    db: Mutex<DB>,
    snapshot: Snapshot,
}

use thiserror::Error;

/// Error type for the TrieDBProvider.
#[derive(Debug, Error)]
#[error("TrieDBProviderError: {error}")]
pub(crate) enum TrieDBProviderError {
    /// Indicates that the key was not found in the database.
    #[error("Key not found: {}", hex::encode(.0))]
    KeyNotFound(Vec<u8>),
    /// Indicates that an error occurred while reading from the database.
    #[error("Error: {0}")]
    Error(String),
}

impl LevelDBTrieProvider {
    /// Constructs a new LevelDBAndRPCTrieDBProvider.
    pub(crate) fn new(path: &Path) -> Self {
        let mut db = DB::open(path, Options::default()).unwrap();
        let snapshot = db.get_snapshot();
        Self { db: Mutex::new(db), snapshot: snapshot }
    }
}


impl TrieDBProvider for LevelDBTrieProvider {
    fn bytecode_by_hash(&self, hash: B256) -> Result<Bytes, Self::Error> {

        const CODE_PREFIX: u8 = b'c';
        let code_hash = [&[CODE_PREFIX], hash.as_slice()].concat();
        
        let mut db = self.db.lock().unwrap();
        db.get_at(&self.snapshot, &code_hash)
            .map_err(|e| TrieDBProviderError::Error(e.to_string()))?
            .ok_or_else(|| TrieDBProviderError::KeyNotFound(code_hash))
            .map(Bytes::from)
    }


// TODO figure out how to construct the header key and get the header from the db
    fn header_by_hash(&self, _hash: B256) -> Result<Header, Self::Error> {
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
        let trie_node_bytes = self
            .db.lock().unwrap()
            .get_at(&self.snapshot, &hash.to_vec())
            .map_err(|e| TrieDBProviderError::Error(e.to_string()))?
            .ok_or_else(|| TrieDBProviderError::KeyNotFound(hash.to_vec()))
            .map(Bytes::from)?;
        // Decode the preimage into a trie node.
        TrieNode::decode(&mut trie_node_bytes.as_ref())
            .map_err(|e| TrieDBProviderError::Error(e.to_string()))
    }
}