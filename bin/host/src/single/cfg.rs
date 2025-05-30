//! This module contains all CLI-specific code for the single chain entrypoint.

use crate::single::CeloSingleChainHintHandler;
use alloy_celo_evm::CeloEvmFactory;
use alloy_provider::RootProvider;
use celo_alloy_network::Celo;
use clap::Parser;
use kona_cli::cli_styles;
use kona_genesis::RollupConfig;
use kona_host::{
    OfflineHostBackend, OnlineHostBackend, OnlineHostBackendCfg, PreimageServer,
    SharedKeyValueStore,
    eth::http_provider,
    single::{SingleChainHost, SingleChainHostError},
};
use kona_preimage::{
    BidirectionalChannel, Channel, HintReader, HintWriter, OracleReader, OracleServer,
};
use kona_proof::HintType;
use kona_providers_alloy::{OnlineBeaconClient, OnlineBlobProvider};
use kona_std_fpvm::{FileChannel, FileDescriptor};
use serde::Serialize;
use std::sync::Arc;
use tokio::task::{self, JoinHandle};

/// The host binary CLI application arguments.
#[derive(Default, Parser, Serialize, Clone, Debug)]
#[command(styles = cli_styles())]
pub struct CeloSingleChainHost {
    /// Inherited kona_host::SingleChainHost CLI arguments.
    #[clap(flatten)]
    pub kona_cfg: SingleChainHost,
}

impl CeloSingleChainHost {
    /// Starts the [CeloSingleChainHost] application.
    pub async fn start(self) -> Result<(), SingleChainHostError> {
        if self.kona_cfg.server {
            let hint = FileChannel::new(FileDescriptor::HintRead, FileDescriptor::HintWrite);
            let preimage =
                FileChannel::new(FileDescriptor::PreimageRead, FileDescriptor::PreimageWrite);

            self.start_server(hint, preimage).await?.await?
        } else {
            self.start_native().await
        }
    }

    /// Starts the preimage server, communicating with the client over the provided channels.
    pub async fn start_server<C>(
        &self,
        hint: C,
        preimage: C,
    ) -> Result<JoinHandle<Result<(), SingleChainHostError>>, SingleChainHostError>
    where
        C: Channel + Send + Sync + 'static,
    {
        let kv_store = self.create_key_value_store()?;

        let task_handle = if self.is_offline() {
            task::spawn(async {
                PreimageServer::new(
                    OracleServer::new(preimage),
                    HintReader::new(hint),
                    Arc::new(OfflineHostBackend::new(kv_store)),
                )
                .start()
                .await
                .map_err(SingleChainHostError::from)
            })
        } else {
            let providers = self.create_providers().await?;
            let backend = OnlineHostBackend::new(
                self.clone(),
                kv_store.clone(),
                providers,
                CeloSingleChainHintHandler,
            )
            .with_proactive_hint(HintType::L2PayloadWitness);

            task::spawn(async {
                PreimageServer::new(
                    OracleServer::new(preimage),
                    HintReader::new(hint),
                    Arc::new(backend),
                )
                .start()
                .await
                .map_err(SingleChainHostError::from)
            })
        };

        Ok(task_handle)
    }

    /// Starts the host in native mode, running both the client and preimage server in the same
    /// process.
    async fn start_native(&self) -> Result<(), SingleChainHostError> {
        let hint = BidirectionalChannel::new()?;
        let preimage = BidirectionalChannel::new()?;

        let server_task = self.start_server(hint.host, preimage.host).await?;
        let client_task = task::spawn(celo_client::single::run(
            OracleReader::new(preimage.client),
            HintWriter::new(hint.client),
            CeloEvmFactory::default(),
        ));

        let (_, client_result) = tokio::try_join!(server_task, client_task)?;

        // Bubble up the exit status of the client program if execution completes.
        std::process::exit(client_result.is_err() as i32)
    }

    /// Returns `true` if the host is running in offline mode.
    pub const fn is_offline(&self) -> bool {
        self.kona_cfg.is_offline()
    }

    /// Reads the [RollupConfig] from the file system and returns it as a string.
    pub fn read_rollup_config(&self) -> Result<RollupConfig, SingleChainHostError> {
        self.kona_cfg.read_rollup_config()
    }

    /// Creates the key-value store for the host backend.
    pub fn create_key_value_store(&self) -> Result<SharedKeyValueStore, SingleChainHostError> {
        self.kona_cfg.create_key_value_store()
    }

    /// Creates the providers required for the host backend.
    pub async fn create_providers(&self) -> Result<CeloSingleChainProviders, SingleChainHostError> {
        let l1_provider = http_provider(
            self.kona_cfg
                .l1_node_address
                .as_ref()
                .ok_or(SingleChainHostError::Other("Provider must be set"))?,
        );
        let blob_provider = OnlineBlobProvider::init(OnlineBeaconClient::new_http(
            self.kona_cfg
                .l1_beacon_address
                .clone()
                .ok_or(SingleChainHostError::Other("Beacon API URL must be set"))?,
        ))
        .await;
        let l2_provider = http_provider::<Celo>(
            self.kona_cfg
                .l2_node_address
                .as_ref()
                .ok_or(SingleChainHostError::Other("L2 node address must be set"))?,
        );

        Ok(CeloSingleChainProviders {
            l1: l1_provider,
            blobs: blob_provider,
            l2: l2_provider,
        })
    }
}

impl OnlineHostBackendCfg for CeloSingleChainHost {
    type HintType = HintType;
    type Providers = CeloSingleChainProviders;
}

/// The providers required for the single chain host.
#[derive(Debug, Clone)]
pub struct CeloSingleChainProviders {
    /// The L1 EL provider.
    pub l1: RootProvider,
    /// The L1 beacon node provider.
    pub blobs: OnlineBlobProvider<OnlineBeaconClient>,
    /// The L2 EL provider.
    pub l2: RootProvider<Celo>,
}

#[cfg(test)]
mod test {
    use crate::single::CeloSingleChainHost;
    use alloy_primitives::B256;
    use clap::Parser;

    #[test]
    fn test_flags() {
        let zero_hash_str = &B256::ZERO.to_string();
        let default_flags = [
            "single",
            "--l1-head",
            zero_hash_str,
            "--l2-head",
            zero_hash_str,
            "--l2-output-root",
            zero_hash_str,
            "--l2-claim",
            zero_hash_str,
            "--l2-block-number",
            "0",
        ];

        let cases = [
            // valid
            (
                ["--server", "--l2-chain-id", "0", "--data-dir", "dummy"].as_slice(),
                true,
            ),
            (
                [
                    "--server",
                    "--rollup-config-path",
                    "dummy",
                    "--data-dir",
                    "dummy",
                ]
                .as_slice(),
                true,
            ),
            (
                ["--native", "--l2-chain-id", "0", "--data-dir", "dummy"].as_slice(),
                true,
            ),
            (
                [
                    "--native",
                    "--rollup-config-path",
                    "dummy",
                    "--data-dir",
                    "dummy",
                ]
                .as_slice(),
                true,
            ),
            (
                [
                    "--l1-node-address",
                    "dummy",
                    "--l2-node-address",
                    "dummy",
                    "--l1-beacon-address",
                    "dummy",
                    "--server",
                    "--l2-chain-id",
                    "0",
                ]
                .as_slice(),
                true,
            ),
            (
                [
                    "--server",
                    "--l2-chain-id",
                    "0",
                    "--data-dir",
                    "dummy",
                    "--enable-experimental-witness-endpoint",
                ]
                .as_slice(),
                true,
            ),
            // invalid
            (
                ["--server", "--native", "--l2-chain-id", "0"].as_slice(),
                false,
            ),
            (
                [
                    "--l2-chain-id",
                    "0",
                    "--rollup-config-path",
                    "dummy",
                    "--server",
                ]
                .as_slice(),
                false,
            ),
            (["--server"].as_slice(), false),
            (["--native"].as_slice(), false),
            (["--rollup-config-path", "dummy"].as_slice(), false),
            (["--l2-chain-id", "0"].as_slice(), false),
            (
                [
                    "--l1-node-address",
                    "dummy",
                    "--server",
                    "--l2-chain-id",
                    "0",
                ]
                .as_slice(),
                false,
            ),
            (
                [
                    "--l2-node-address",
                    "dummy",
                    "--server",
                    "--l2-chain-id",
                    "0",
                ]
                .as_slice(),
                false,
            ),
            (
                [
                    "--l1-beacon-address",
                    "dummy",
                    "--server",
                    "--l2-chain-id",
                    "0",
                ]
                .as_slice(),
                false,
            ),
            ([].as_slice(), false),
        ];

        for (args_ext, valid) in cases.into_iter() {
            let args = default_flags
                .iter()
                .chain(args_ext.iter())
                .cloned()
                .collect::<Vec<_>>();

            let parsed = CeloSingleChainHost::try_parse_from(args);
            assert_eq!(parsed.is_ok(), valid);
        }
    }
}
