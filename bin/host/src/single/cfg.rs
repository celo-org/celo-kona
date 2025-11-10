//! This module is copied from https://github.com/Layr-Labs/hokulea/blob/ede4e93969fa3e181093c8807576afb85c60cb0e/bin/host/src/cfg.rs and https://github.com/op-rs/kona/blob/kona-client/v1.1.7/bin/host/src/single/cfg.rs.

use crate::single::CeloSingleChainHintHandler;
use anyhow::Result;
use celo_genesis::CeloRollupConfig;
use clap::Parser;
use hokulea_host_bin::eigenda_preimage::OnlineEigenDAPreimageProvider;
use hokulea_proof::hint::ExtendedHintType;
use kona_cli::cli_styles;
use kona_host::{
    OfflineHostBackend, OnlineHostBackend, OnlineHostBackendCfg, PreimageServer,
    single::{SingleChainHostError, SingleChainProviders},
};
use kona_preimage::{
    BidirectionalChannel, Channel, HintReader, HintWriter, OracleReader, OracleServer,
};
use kona_proof::HintType;
use kona_std_fpvm::{FileChannel, FileDescriptor};
use serde::Serialize;
use std::sync::Arc;
use tokio::task::{self, JoinHandle};
use tracing::warn;

/// The host Eigenda binary CLI application arguments. Replacing SingleChainHostWithEigenDA in hokulea
#[derive(Default, Parser, Serialize, Clone, Debug)]
#[command(styles = cli_styles())]
pub struct CeloSingleChainHost {
    /// Inherited kona_host::SingleChainHost CLI arguments.
    #[clap(flatten)]
    pub kona_cfg: kona_host::single::SingleChainHost,

    /// URL of the EigenDA RPC endpoint.
    #[clap(
        long,
        visible_alias = "eigenda",
        requires = "l2_node_address",
        requires = "l1_node_address",
        requires = "l1_beacon_address",
        env
    )]
    pub eigenda_proxy_address: Option<String>,

    /// Recency window determining if an EigenDA certificate
    /// is stale. It is VERY CRITICAL!!! to ensure the same value
    /// is used by proxy connecting this host. Currently, the
    /// only valid value is the seq_window_size in the
    /// L2 rollup config.
    #[arg(long, visible_alias = "recency_window", env)]
    pub recency_window: u64,

    /// Verbosity level (-v, -vv, -vvv, etc.)
    /// TODO: think this should be upstreamed to kona_cfg
    #[clap(
        short,
        long,
        action = clap::ArgAction::Count,
        default_value_t = 0
    )]
    pub verbose: u8,
}

impl CeloSingleChainHost {
    /// Starts the [CeloSingleChainHost] application. This is copy from
    /// <https://github.com/op-rs/kona/blob/b3eef14771015f6f7427f4f05cf70e508b641802/bin/host/src/single/cfg.rs#L133-L143>
    pub async fn start(self) -> Result<(), SingleChainHostError> {
        warn!("please ensure the configured recency window is equal to the recency window on the eigenda proxy. Inconsistent
                values can lead to halting");

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
        let kv_store = self.kona_cfg.create_key_value_store()?;

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
            .with_proactive_hint(ExtendedHintType::Original(HintType::L2PayloadWitness));

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
        // Start the client program in a separate child process.

        let client_task = task::spawn(celo_client::single::run(
            OracleReader::new(preimage.client),
            HintWriter::new(hint.client),
        ));

        let (_, client_result) = tokio::try_join!(server_task, client_task)?;

        // Bubble up the exit status of the client program if execution completes.
        std::process::exit(client_result.is_err() as i32)
    }

    /// Returns `true` if the host is running in offline mode.
    pub const fn is_offline(&self) -> bool {
        self.kona_cfg.is_offline() && self.eigenda_proxy_address.is_none()
    }

    /// Reads the [CeloRollupConfig] from the file system and returns it as a string.
    pub fn read_rollup_config(&self) -> Result<CeloRollupConfig, SingleChainHostError> {
        let rollup_config = self.kona_cfg.read_rollup_config()?;
        let celo_rollup_config = CeloRollupConfig(rollup_config);
        Ok(celo_rollup_config)
    }

    /// Creates the providers with eigenda
    pub async fn create_providers(&self) -> Result<CeloSingleChainProviders, SingleChainHostError> {
        let kona_providers = self.kona_cfg.create_providers().await?;

        let eigenda_preimage_provider = OnlineEigenDAPreimageProvider::new_http(
            self.eigenda_proxy_address
                .clone()
                .ok_or(SingleChainHostError::Other("EigenDA API URL must be set"))?,
        );

        Ok(CeloSingleChainProviders { kona_providers, eigenda_preimage_provider })
    }
}

/// Specify the wrapper type
impl OnlineHostBackendCfg for CeloSingleChainHost {
    type HintType = ExtendedHintType;
    type Providers = CeloSingleChainProviders;
}

/// The providers required for the single chain host.
#[derive(Debug, Clone)]
pub struct CeloSingleChainProviders {
    /// The Kona providers
    pub kona_providers: SingleChainProviders,
    /// The EigenDA preimage provider
    pub eigenda_preimage_provider: OnlineEigenDAPreimageProvider,
}
