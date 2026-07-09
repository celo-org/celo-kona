//! This module contains all CLI-specific code for the single chain entrypoint.

use crate::single::{CeloConfigBackend, CeloSingleChainHintHandler};
use alloy_provider::RootProvider;
use backon::{ExponentialBuilder, Retryable};
use celo_alloy_network::Celo;
use celo_genesis::CeloRollupConfig;
use clap::Parser;
#[cfg(feature = "eigenda")]
use hokulea_host_bin::eigenda_preimage::OnlineEigenDAPreimageProvider;
#[cfg(feature = "eigenda")]
use hokulea_proof::hint::ExtendedHintType;
use kona_cli::cli_styles;
use kona_host::{
    OfflineHostBackend, OnlineHostBackend, OnlineHostBackendCfg, PreimageServer,
    SharedKeyValueStore,
    eth::rpc_provider,
    single::{SingleChainHost, SingleChainHostError},
};
use kona_preimage::{
    BidirectionalChannel, Channel, HintReader, HintWriter, OracleReader, OracleServer,
};
use kona_proof::HintType;
use kona_providers_alloy::{
    BeaconClient, BeaconClientError, OnlineBeaconClient, OnlineBlobProvider,
};
use kona_std_fpvm::{FileChannel, FileDescriptor};
#[cfg(feature = "eigenda")]
use reqwest::Url;
use serde::Serialize;
use std::{sync::Arc, time::Duration};
use tokio::task::{self, JoinHandle};
use tracing::warn;

/// The host binary CLI application arguments.
#[derive(Default, Parser, Serialize, Clone, Debug)]
#[command(styles = cli_styles())]
pub struct CeloSingleChainHost {
    /// Inherited kona_host::SingleChainHost CLI arguments.
    #[clap(flatten)]
    pub kona_cfg: SingleChainHost,

    /// URL of the EigenDA Proxy endpoint.
    #[clap(
        long,
        visible_alias = "eigenda",
        requires = "l2_node_address",
        requires = "l1_node_address",
        requires = "l1_beacon_address",
        env
    )]
    pub eigenda_proxy_address: Option<String>,

    /// Verbosity level (0-5). Use --verbose N to set the level, e.g., --verbose 3
    #[clap(long, default_value = "0")]
    pub verbose: u8,
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
        // The rollup config (Celo Espresso settings included) that `CeloConfigBackend` serves under
        // `L2_ROLLUP_CONFIG_KEY`. `None` when no rollup config is configured, in which case the
        // request is delegated to the inner backend (yielding `KeyNotFound`), matching upstream.
        let rollup_config_json =
            self.read_rollup_config()?.and_then(|cfg| serde_json::to_vec(&cfg).ok()).map(Arc::new);

        let task_handle = if self.is_offline() {
            task::spawn(async {
                PreimageServer::new(
                    OracleServer::new(preimage),
                    HintReader::new(hint),
                    Arc::new(CeloConfigBackend::new(
                        OfflineHostBackend::new(kv_store),
                        rollup_config_json,
                    )),
                )
                .start()
                .await
                .map_err(SingleChainHostError::from)
            })
        } else {
            let providers = self.create_providers().await?;
            let backend = CeloConfigBackend::new(
                OnlineHostBackend::new(
                    self.clone(),
                    kv_store.clone(),
                    providers,
                    CeloSingleChainHintHandler,
                )
                .with_proactive_hint(proactive_hint_type()),
                rollup_config_json,
            );

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
        ));

        let (_, client_result) = tokio::try_join!(server_task, client_task)?;

        // Bubble up the exit status of the client program if execution completes.
        std::process::exit(client_result.is_err() as i32)
    }

    /// Returns `true` if the host is running in offline mode.
    pub const fn is_offline(&self) -> bool {
        self.kona_cfg.is_offline()
    }

    /// Reads the [`CeloRollupConfig`] from the rollup config path, if one is configured.
    ///
    /// The file is deserialized directly as a [`CeloRollupConfig`] so the Celo Espresso settings
    /// (`espresso_time` / `batch_authenticator_address`) are preserved. Going through the upstream
    /// reader instead would yield an upstream `RollupConfig` and drop those fields.
    pub fn read_rollup_config(&self) -> Result<Option<CeloRollupConfig>, SingleChainHostError> {
        let Some(path) = self.kona_cfg.rollup_config_path.as_ref() else {
            return Ok(None);
        };
        let ser_config = std::fs::read_to_string(path)?;
        let celo_rollup_config =
            serde_json::from_str(&ser_config).map_err(SingleChainHostError::ParseError)?;
        Ok(Some(celo_rollup_config))
    }

    /// Creates the key-value store for the host backend. Delegates to the upstream factory.
    pub fn create_key_value_store(&self) -> Result<SharedKeyValueStore, SingleChainHostError> {
        self.kona_cfg.create_key_value_store()
    }

    /// Creates the providers required for the host backend.
    pub async fn create_providers(&self) -> Result<CeloSingleChainProviders, SingleChainHostError> {
        let l1_provider = rpc_provider(
            self.kona_cfg
                .l1_node_address
                .as_ref()
                .ok_or(SingleChainHostError::Other("Provider must be set"))?,
        )
        .await;
        let beacon_client = OnlineBeaconClient::new_http(
            self.kona_cfg
                .l1_beacon_address
                .clone()
                .ok_or(SingleChainHostError::Other("Beacon API URL must be set"))?,
        );
        // `OnlineBlobProvider::init` panics if the one-off beacon genesis/slot config load hits a
        // transient RPC failure; load those values with the shared backoff and build the provider
        // directly so a flaky beacon endpoint is retried instead of taking the host down.
        let blob_provider = init_blob_provider_with_backoff(beacon_client).await?;
        let l2_provider = rpc_provider::<Celo>(
            self.kona_cfg
                .l2_node_address
                .as_ref()
                .ok_or(SingleChainHostError::Other("L2 node address must be set"))?,
        )
        .await;

        #[cfg(feature = "eigenda")]
        let eigenda_preimage_provider = self
            .eigenda_proxy_address
            .as_ref()
            .map(|url_str| {
                Url::parse(url_str)
                    .map_err(|_| SingleChainHostError::Other("Failed to parse EigenDA API URL"))
                    .map(OnlineEigenDAPreimageProvider::new_http)
            })
            .transpose()?;

        Ok(CeloSingleChainProviders {
            l1: l1_provider,
            blobs: blob_provider,
            l2: l2_provider,
            #[cfg(feature = "eigenda")]
            eigenda_preimage_provider,
        })
    }
}

/// Builds an [`OnlineBlobProvider`] without the panic path in [`OnlineBlobProvider::init`],
/// which `.expect()`s if the one-off beacon genesis-time / slot-interval load hits a transient
/// RPC failure. Those two values are fetched here under a short bounded backoff and the provider
/// is constructed directly, so a flaky beacon endpoint is retried quietly instead of panicking
/// the preimage server (which would abort the whole host run for its consumer). A sustained
/// outage surfaces as a `SingleChainHostError` after the backoff budget.
async fn init_blob_provider_with_backoff(
    beacon: OnlineBeaconClient,
) -> Result<OnlineBlobProvider<OnlineBeaconClient>, SingleChainHostError> {
    let genesis_time = (|| async { beacon.genesis_time().await })
        .retry(beacon_init_retry_policy())
        .when(is_retryable_beacon_err)
        .notify(notify_beacon_retry)
        .await
        .map_err(|e| {
            warn!(target: "single_host_cfg", error = %e, "failed to load beacon genesis time after retries");
            SingleChainHostError::Other("failed to load beacon genesis time")
        })?
        .data
        .genesis_time;

    let slot_interval = (|| async { beacon.slot_interval().await })
        .retry(beacon_init_retry_policy())
        .when(is_retryable_beacon_err)
        .notify(notify_beacon_retry)
        .await
        .map_err(|e| {
            warn!(target: "single_host_cfg", error = %e, "failed to load beacon slot interval after retries");
            SingleChainHostError::Other("failed to load beacon slot interval")
        })?
        .data
        .seconds_per_slot;

    Ok(OnlineBlobProvider { beacon_client: beacon, genesis_time, slot_interval })
}

/// Backoff for the one-off beacon genesis/slot reads at provider construction. Deliberately
/// shorter than the hint-fetch budget: this runs at host startup, so a genuine misconfiguration
/// (bad URL / auth / non-beacon endpoint) — which cannot always be told apart from a transient
/// decode at this layer — fails in ~20s instead of hanging, while a real transient blip that
/// recovers in seconds is still absorbed.
fn beacon_init_retry_policy() -> ExponentialBuilder {
    ExponentialBuilder::default()
        .with_min_delay(Duration::from_millis(200))
        .with_factor(2.0)
        .with_max_delay(Duration::from_secs(10))
        .with_max_times(7)
}

/// Classifies a beacon config-read failure. These reads (`/eth/v1/beacon/genesis`,
/// `/eth/v1/config/spec`) never call `error_for_status`, so a bad HTTP status comes back as a
/// status-less body-decode error. When a status *is* present, retry only the transient ones
/// (`5xx`, `429`) and let deterministic `4xx` (auth, not-found) fail fast; otherwise retry
/// transport failures and body/decode errors — the intermittent flaky-endpoint case this guards
/// against — bounded by the short [`beacon_init_retry_policy`].
fn is_retryable_beacon_err(err: &BeaconClientError) -> bool {
    let BeaconClientError::Http(e) = err else { return false };
    if let Some(status) = e.status() {
        return status.is_server_error() || status.as_u16() == 429;
    }
    e.is_connect() || e.is_timeout() || e.is_body() || e.is_decode()
}

/// Logs a transient beacon config-read failure and the delay before the next attempt.
fn notify_beacon_retry(err: &BeaconClientError, backoff: Duration) {
    warn!(target: "single_host_cfg", error = %err, ?backoff, "transient beacon RPC failure loading blob-provider config; retrying");
}

#[cfg(feature = "eigenda")]
impl OnlineHostBackendCfg for CeloSingleChainHost {
    type HintType = ExtendedHintType;
    type Providers = CeloSingleChainProviders;
}

#[cfg(not(feature = "eigenda"))]
impl OnlineHostBackendCfg for CeloSingleChainHost {
    type HintType = HintType;
    type Providers = CeloSingleChainProviders;
}

/// Returns the proactive hint type for the host backend.
#[cfg(feature = "eigenda")]
const fn proactive_hint_type() -> ExtendedHintType {
    ExtendedHintType::Original(HintType::L2PayloadWitness)
}

/// Returns the proactive hint type for the host backend.
#[cfg(not(feature = "eigenda"))]
const fn proactive_hint_type() -> HintType {
    HintType::L2PayloadWitness
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
    /// The EigenDA blob provider
    #[cfg(feature = "eigenda")]
    pub eigenda_preimage_provider: Option<OnlineEigenDAPreimageProvider>,
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
            (["--server", "--l2-chain-id", "0", "--data-dir", "dummy"].as_slice(), true),
            (["--server", "--rollup-config-path", "dummy", "--data-dir", "dummy"].as_slice(), true),
            (["--native", "--l2-chain-id", "0", "--data-dir", "dummy"].as_slice(), true),
            (["--native", "--rollup-config-path", "dummy", "--data-dir", "dummy"].as_slice(), true),
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
                    "--eigenda-proxy-address",
                    "dummy",
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
            // invalid
            (["--server", "--native", "--l2-chain-id", "0"].as_slice(), false),
            (["--l2-chain-id", "0", "--rollup-config-path", "dummy", "--server"].as_slice(), false),
            (["--server"].as_slice(), false),
            (["--native"].as_slice(), false),
            (["--rollup-config-path", "dummy"].as_slice(), false),
            (["--l2-chain-id", "0"].as_slice(), false),
            (["--l1-node-address", "dummy", "--server", "--l2-chain-id", "0"].as_slice(), false),
            (["--l2-node-address", "dummy", "--server", "--l2-chain-id", "0"].as_slice(), false),
            (["--l1-beacon-address", "dummy", "--server", "--l2-chain-id", "0"].as_slice(), false),
            ([].as_slice(), false),
            (
                [
                    "--eigenda-proxy-address",
                    "dummy",
                    "--server",
                    "--rollup-config-path",
                    "dummy",
                    "--data-dir",
                    "dummy",
                ]
                .as_slice(),
                false,
            ),
        ];

        for (args_ext, valid) in cases.into_iter() {
            let args = default_flags.iter().chain(args_ext.iter()).cloned().collect::<Vec<_>>();

            let parsed = CeloSingleChainHost::try_parse_from(args);
            assert_eq!(parsed.is_ok(), valid);
        }
    }
}
