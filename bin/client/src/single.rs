//! Single-chain fault proof program entrypoint.

use alloc::sync::Arc;
use alloy_celo_evm::CeloEvmFactory;
use alloy_consensus::Sealed;
use alloy_primitives::B256;
use celo_driver::CeloDriver;
use celo_genesis::CeloRollupConfig;
use celo_proof::{
    CeloBootInfo, CeloOracleL2ChainProvider, CeloOraclePipeline, executor::CeloExecutor,
    new_oracle_pipeline_cursor,
};
use core::fmt::Debug;
use hokulea_eigenda::{EigenDABlobSource, EigenDADataSource};
use hokulea_proof::eigenda_provider::OracleEigenDAProvider;
use kona_client::single::FaultProofProgramError;
use kona_derive::prelude::EthereumDataSource;
use kona_executor::TrieDBProvider;
use kona_preimage::{CommsClient, HintWriterClient, PreimageKey, PreimageOracleClient};
use kona_proof::{
    CachingOracle, HintType,
    errors::OracleProviderError,
    l1::{OracleBlobProvider, OracleL1ChainProvider},
    l2::OracleL2ChainProvider,
};
use tracing::{error, info};

/// Executes the fault proof program with the given [PreimageOracleClient] and [HintWriterClient].
#[inline]
pub async fn run<P, H>(oracle_client: P, hint_client: H) -> Result<(), FaultProofProgramError>
where
    P: PreimageOracleClient + Send + Sync + Debug + Clone + 'static,
    H: HintWriterClient + Send + Sync + Debug + Clone + 'static,
{
    const ORACLE_LRU_SIZE: usize = 1024;

    ////////////////////////////////////////////////////////////////
    //                          PROLOGUE                          //
    ////////////////////////////////////////////////////////////////

    let oracle =
        Arc::new(CachingOracle::new(ORACLE_LRU_SIZE, oracle_client.clone(), hint_client.clone()));
    let boot = CeloBootInfo::load(oracle.as_ref()).await?;

    // Wrap RollupConfig to CeloRollupConfig
    let celo_rollup_config =
        CeloRollupConfig { op_rollup_config: boot.op_boot_info.rollup_config.clone() };
    let celo_rollup_config = Arc::new(celo_rollup_config);

    let rollup_config = Arc::new(boot.op_boot_info.rollup_config);
    let safe_head_hash =
        fetch_safe_head_hash(oracle.as_ref(), boot.op_boot_info.agreed_l2_output_root).await?;

    let mut l1_provider = OracleL1ChainProvider::new(boot.op_boot_info.l1_head, oracle.clone());
    let mut l2_provider =
        OracleL2ChainProvider::new(safe_head_hash, rollup_config.clone(), oracle.clone());
    let mut celo_l2_provider =
        CeloOracleL2ChainProvider::new(safe_head_hash, rollup_config.clone(), oracle.clone());
    let beacon = OracleBlobProvider::new(oracle.clone());

    // Fetch the safe head's block header.
    let safe_head = l2_provider
        .header_by_hash(safe_head_hash)
        .map(|header| Sealed::new_unchecked(header, safe_head_hash))?;

    // If the claimed L2 block number is less than the safe head of the L2 chain, the claim is
    // invalid.
    if boot.op_boot_info.claimed_l2_block_number < safe_head.number {
        error!(
            target: "client",
            claimed = boot.op_boot_info.claimed_l2_block_number,
            safe = safe_head.number,
            "Claimed L2 block number is less than the safe head",
        );
        return Err(FaultProofProgramError::InvalidClaim(
            boot.op_boot_info.agreed_l2_output_root,
            boot.op_boot_info.claimed_l2_output_root,
        ));
    }

    // In the case where the agreed upon L2 output root is the same as the claimed L2 output root,
    // trace extension is detected and we can skip the derivation and execution steps.
    if boot.op_boot_info.agreed_l2_output_root == boot.op_boot_info.claimed_l2_output_root {
        info!(
            target: "client",
            "Trace extension detected. State transition is already agreed upon.",
        );
        return Ok(());
    }

    ////////////////////////////////////////////////////////////////
    //                   DERIVATION & EXECUTION                   //
    ////////////////////////////////////////////////////////////////

    // Create a new derivation driver with the given boot information and oracle.
    let cursor = new_oracle_pipeline_cursor(
        rollup_config.as_ref(),
        safe_head,
        &mut l1_provider,
        &mut celo_l2_provider,
    )
    .await?;
    l2_provider.set_cursor(cursor.clone());

    let evm_factory = CeloEvmFactory::default();
    let eth_data_source =
        EthereumDataSource::new_from_parts(l1_provider.clone(), beacon, &rollup_config);

    let eigenda_blob_provider = OracleEigenDAProvider::new(oracle.clone());
    let eigenda_blob_source = EigenDABlobSource::new(eigenda_blob_provider);
    let da_provider = EigenDADataSource::new(eth_data_source, eigenda_blob_source);

    let pipeline = CeloOraclePipeline::new(
        rollup_config.clone(),
        cursor.clone(),
        oracle.clone(),
        da_provider,
        l1_provider.clone(),
        celo_l2_provider.clone(),
    )
    .await?;
    let executor = CeloExecutor::new(
        celo_rollup_config.as_ref(),
        l2_provider.clone(),
        l2_provider,
        evm_factory,
        None,
    );
    let mut driver = CeloDriver::new(cursor, executor, pipeline);

    // Run the derivation pipeline until we are able to produce the output root of the claimed
    // L2 block.
    let (safe_head, output_root) = driver
        .advance_to_target(
            celo_rollup_config.as_ref(),
            Some(boot.op_boot_info.claimed_l2_block_number),
        )
        .await?;

    ////////////////////////////////////////////////////////////////
    //                          EPILOGUE                          //
    ////////////////////////////////////////////////////////////////

    if output_root != boot.op_boot_info.claimed_l2_output_root {
        error!(
            target: "client",
            number = safe_head.block_info.number,
            output_root = ?output_root,
            "Failed to validate L2 block",
        );
        return Err(FaultProofProgramError::InvalidClaim(
            output_root,
            boot.op_boot_info.claimed_l2_output_root,
        ));
    }

    info!(
        target: "client",
        number = safe_head.block_info.number,
        output_root = ?output_root,
        "Successfully validated L2 block",
    );

    Ok(())
}

/// Fetches the safe head hash of the L2 chain based on the agreed upon L2 output root in the
/// [CeloBootInfo].
pub async fn fetch_safe_head_hash<O>(
    caching_oracle: &O,
    agreed_l2_output_root: B256,
) -> Result<B256, OracleProviderError>
where
    O: CommsClient,
{
    let mut output_preimage = [0u8; 128];
    HintType::StartingL2Output
        .with_data(&[agreed_l2_output_root.as_ref()])
        .send(caching_oracle)
        .await?;
    caching_oracle
        .get_exact(PreimageKey::new_keccak256(*agreed_l2_output_root), output_preimage.as_mut())
        .await?;

    output_preimage[96..128].try_into().map_err(OracleProviderError::SliceConversion)
}
