//! Access to the EVM journal needed for an uncommitted irregular transition.

use super::upgrade18::FeeVaultConfig;
use crate::CeloEvm;
use alloy_evm::{Database, Evm, block::BlockExecutionError};
use alloy_primitives::{Address, Bytes, U256, address, keccak256};
use celo_revm::CeloContext;
use revm::{
    Inspector, context::Journal, handler::PrecompileProvider, interpreter::InterpreterResult,
};

/// EVM access required to stage migration before the ordinary activation deposit executes.
pub trait CgtV2Evm: Evm {
    /// The live journal, including accounts cached by earlier work in this block.
    fn cgt_v2_journal(&mut self) -> &mut Journal<Self::DB>;
    /// Read the old vault configuration without committing temporary call effects.
    fn read_cgt_v2_vault_config(&mut self) -> Result<FeeVaultConfig, BlockExecutionError>;
}

impl<DB, I, P> CgtV2Evm for CeloEvm<DB, I, P>
where
    DB: Database,
    I: Inspector<CeloContext<DB>>,
    P: PrecompileProvider<CeloContext<DB>, Output = InterpreterResult>,
{
    fn cgt_v2_journal(&mut self) -> &mut Journal<Self::DB> {
        &mut self.ctx_mut().journaled_state
    }

    fn read_cgt_v2_vault_config(&mut self) -> Result<FeeVaultConfig, BlockExecutionError> {
        let mut read = |signature: &str| -> Result<U256, BlockExecutionError> {
            let (output, _, _) = celo_revm::contracts::core_contracts::call_read_only(
                &mut self.inner,
                address!("4200000000000000000000000000000000000011"),
                Bytes::copy_from_slice(&keccak256(signature)[..4]),
                Some(100_000),
            )
            .map_err(BlockExecutionError::other)?;
            if output.len() != 32 {
                return Err(BlockExecutionError::msg(
                    "CGTv2 legacy fee-vault getter returned malformed data",
                ));
            }
            Ok(U256::from_be_slice(&output))
        };
        let recipient = read("recipient()")?;
        let min_withdrawal_amount = read("minWithdrawalAmount()")?;
        let network = read("withdrawalNetwork()")?;
        if recipient >> 160 != U256::ZERO || network > U256::from(1) {
            return Err(BlockExecutionError::msg(
                "CGTv2 legacy fee-vault getter returned an invalid setting",
            ));
        }
        Ok(FeeVaultConfig {
            recipient: Address::from_word(recipient.into()),
            min_withdrawal_amount,
            withdrawal_network: network.to::<u8>(),
        })
    }
}
