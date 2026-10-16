//! Missing CIP-64 state must abort execution rather than justify a deposit-only block.

use super::*;
use alloy_primitives::{Address, B256, Bytes, Sealable};
use celo_alloy_consensus::CeloTxEnvelope;
use kona_executor::test_utils::{DiskTrieNodeProvider, load_test_fixture};
use kona_mpt::{NoopTrieHinter, TrieNode, TrieProvider};
use rstest::rstest;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
struct WitnessProvider {
    inner: Arc<DiskTrieNodeProvider>,
    missing: Option<B256>,
    reads: Arc<Mutex<Vec<B256>>>,
}

impl TrieProvider for WitnessProvider {
    type Error = String;

    fn trie_node_by_hash(&self, key: B256) -> Result<TrieNode, Self::Error> {
        self.reads.lock().unwrap().push(key);
        if self.missing == Some(key) {
            return Err("injected missing CIP-64 preimage".into());
        }
        self.inner.trie_node_by_hash(key).map_err(|err| err.to_string())
    }
}

impl TrieDBProvider for WitnessProvider {
    fn bytecode_by_hash(&self, key: B256) -> Result<Bytes, Self::Error> {
        self.inner.bytecode_by_hash(key).map_err(|err| err.to_string())
    }

    fn header_by_hash(&self, key: B256) -> Result<Header, Self::Error> {
        self.inner.header_by_hash(key).map_err(|err| err.to_string())
    }
}

#[rstest]
#[case::directory_storage(true)]
#[case::payer_balance(false)]
#[tokio::test]
async fn missing_cip64_preimage_is_not_an_invalid_payload(#[case] directory: bool) {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata/sepolia-cip64-erc20-transfer-tx_block-2346450.tar.gz");
    let loaded = load_test_fixture(path).await;
    let fixture = &loaded.fixture;
    let cfg = CeloRollupConfig::new(fixture.rollup_config.clone());
    let provider = WitnessProvider {
        inner: Arc::new(loaded.provider),
        missing: None,
        reads: Arc::new(Mutex::new(Vec::new())),
    };
    let builder = |provider| {
        CeloStatelessL2Builder::new(
            &cfg,
            CeloEvmFactory::default(),
            provider,
            NoopTrieHinter,
            fixture.parent_header.clone().seal_slow(),
        )
    };
    let outcome = builder(provider.clone())
        .build_block(fixture.executing_payload.clone())
        .expect("complete witness must execute");
    assert_eq!(outcome.header.hash(), fixture.expected_block_hash);

    let mut probe = builder(provider.clone());
    let missing = if directory {
        let address =
            celo_revm::constants::get_addresses(cfg.l2_chain_id.id()).fee_currency_directory;
        probe.trie_db.get_trie_account(&address).unwrap().unwrap().storage_root
    } else {
        // Read the real payer's balance to find the leaf needed by balanceOf, including
        // proxy and token-specific storage layout. This avoids guessing a mapping slot.
        let (token, payer): (Address, Address) = fixture
            .executing_payload
            .celo_recovered_transactions()
            .map(Result::unwrap)
            .find_map(|tx| match tx.inner() {
                CeloTxEnvelope::Cip64(signed) => {
                    Some((signed.tx().fee_currency.unwrap(), tx.signer()))
                }
                _ => None,
            })
            .expect("fixture must contain a CIP-64 transaction");
        let (params, min_fee) =
            CeloStatelessL2Builder::<WitnessProvider, NoopTrieHinter>::active_base_fee_params(
                &cfg,
                &fixture.parent_header,
                fixture.executing_payload.payload_attributes.timestamp,
            )
            .unwrap();
        let env = probe
            .evm_env(&fixture.parent_header, &fixture.executing_payload, &params, min_fee)
            .unwrap();
        provider.reads.lock().unwrap().clear();
        let mut evm = CeloEvmFactory::default().create_evm(&mut probe.trie_db, env);
        assert!(evm.erc20_balance(token, payer, 1_000_000).unwrap() > alloy_primitives::U256::ZERO);
        let leaf = *provider.reads.lock().unwrap().last().expect("balance read must fetch storage");
        assert!(matches!(provider.inner.trie_node_by_hash(leaf).unwrap(), TrieNode::Leaf { .. }));
        leaf
    };

    let mut incomplete = provider;
    incomplete.missing = Some(missing);
    let error = builder(incomplete)
        .build_block(fixture.executing_payload.clone())
        .expect_err("missing witness must fail execution");
    assert!(error.to_string().contains("injected missing CIP-64 preimage"), "{error:?}");
    assert!(!error.is_invalid_payload(), "missing state cannot prove a payload invalid: {error:?}");
}
