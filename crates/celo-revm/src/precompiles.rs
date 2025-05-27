pub mod transfer;

pub use transfer::{TRANSFER_ADDRESS, transfer_run};

use core::iter;
use op_revm::{OpSpecId, precompiles::OpPrecompiles};
use revm::{
    context::Cfg,
    context_interface::ContextTr,
    handler::PrecompileProvider,
    interpreter::{InputsImpl, InterpreterResult},
    primitives::Address,
};
use std::{boxed::Box, string::String};

// Celo precompile provider
#[derive(Debug, Clone)]
pub struct CeloPrecompiles {
    op_precompiles: OpPrecompiles,
}

impl CeloPrecompiles {
    /// Create a new precompile provider with the given OpSpec.
    #[inline]
    pub fn new_with_spec(spec: OpSpecId) -> Self {
        Self {
            op_precompiles: OpPrecompiles::new_with_spec(spec),
        }
    }
}

impl<CTX> PrecompileProvider<CTX> for CeloPrecompiles
where
    CTX: ContextTr<Cfg: Cfg<Spec = OpSpecId>>,
{
    type Output = InterpreterResult;

    #[inline]
    fn set_spec(&mut self, spec: <CTX::Cfg as Cfg>::Spec) -> bool {
        <OpPrecompiles as PrecompileProvider<CTX>>::set_spec(&mut self.op_precompiles, spec)
    }

    #[inline]
    fn run(
        &mut self,
        context: &mut CTX,
        address: &Address,
        inputs: &InputsImpl,
        is_static: bool,
        gas_limit: u64,
    ) -> Result<Option<Self::Output>, String> {
        if *address == TRANSFER_ADDRESS {
            transfer_run(context, inputs, gas_limit)
        } else {
            self.op_precompiles
                .run(context, address, inputs, is_static, gas_limit)
        }
    }

    #[inline]
    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        let op_iter =
            <OpPrecompiles as PrecompileProvider<CTX>>::warm_addresses(&self.op_precompiles);
        let transfer_iter = iter::once(TRANSFER_ADDRESS);
        Box::new(op_iter.chain(transfer_iter))
    }

    #[inline]
    fn contains(&self, address: &Address) -> bool {
        *address == TRANSFER_ADDRESS
            || <OpPrecompiles as PrecompileProvider<CTX>>::contains(&self.op_precompiles, address)
    }
}

impl Default for CeloPrecompiles {
    fn default() -> Self {
        Self::new_with_spec(OpSpecId::ISTHMUS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CeloContext, DefaultCelo,
        constants::{CELO_MAINNET_CHAIN_ID, get_addresses},
        precompiles::transfer::TRANSFER_GAS_COST,
    };
    use revm::{
        Context,
        context::{JournalOutput, JournalTr},
        database::{EmptyDB, in_memory_db::InMemoryDB},
        interpreter::InstructionResult,
        primitives::{Bytes, U256},
        state::AccountInfo,
    };

    #[test]
    fn test_celo_precompiles_count() {
        let celo_precompiles = CeloPrecompiles::default();
        let op_precompiles = OpPrecompiles::default();

        let celo_count =
            <CeloPrecompiles as PrecompileProvider<CeloContext<EmptyDB>>>::warm_addresses(
                &celo_precompiles,
            )
            .count();
        let op_count = <OpPrecompiles as PrecompileProvider<CeloContext<EmptyDB>>>::warm_addresses(
            &op_precompiles,
        )
        .count();

        assert_eq!(celo_count, op_count + 1);
    }

    #[test]
    fn test_transfer() {
        let mut precompiles = CeloPrecompiles::default();

        let from = Address::with_last_byte(1);
        let to = Address::with_last_byte(2);
        let value = 10;
        let mut input_vec = vec![0u8; 96];
        input_vec[31] = 1;
        input_vec[63] = 2;
        input_vec[95] = value;
        let input = Bytes::from(input_vec);

        let inputs = InputsImpl {
            target_address: TRANSFER_ADDRESS,
            caller_address: from,
            input,
            call_value: U256::from(value),
        };

        let initial_balance = 100;
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            from,
            AccountInfo {
                balance: U256::from(initial_balance),
                ..Default::default()
            },
        );

        // Test calling with valid parameters
        let mut ctx = Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.caller = get_addresses(CELO_MAINNET_CHAIN_ID).celo_token;
            })
            .modify_cfg_chained(|cfg| cfg.chain_id = CELO_MAINNET_CHAIN_ID)
            .with_db(db);

        let res = precompiles.run(
            &mut ctx,
            &TRANSFER_ADDRESS,
            &inputs,
            false,
            TRANSFER_GAS_COST,
        );
        assert!(res.is_ok());

        let JournalOutput { state, .. } = ctx.journal().finalize();
        let from_account = state.get(&from).unwrap();
        let to_account = state.get(&to).unwrap();
        assert_eq!(
            from_account.info.balance,
            U256::from(initial_balance - value)
        );
        assert_eq!(to_account.info.balance, U256::from(value));

        // Test calling with too little gas
        let res = precompiles.run(
            &mut ctx,
            &TRANSFER_ADDRESS,
            &inputs,
            false,
            TRANSFER_GAS_COST - 1_000,
        );
        assert!(matches!(
            res,
            Ok(Some(InterpreterResult {
                result: InstructionResult::PrecompileOOG,
                ..
            }))
        ));

        // Test calling with too short input
        let short_input = Bytes::from(vec![0; 63]);
        let bad_input = InputsImpl {
            input: short_input,
            ..inputs
        };
        let res = precompiles.run(
            &mut ctx,
            &TRANSFER_ADDRESS,
            &bad_input,
            false,
            TRANSFER_GAS_COST,
        );
        assert!(matches!(
            res,
            Ok(Some(InterpreterResult {
                result: InstructionResult::PrecompileError,
                ..
            }))
        ));

        // Test calling from the wrong address
        ctx.modify_tx(|tx| {
            tx.op_tx.base.caller = from;
        });
        let res = precompiles.run(
            &mut ctx,
            &TRANSFER_ADDRESS,
            &inputs,
            false,
            TRANSFER_GAS_COST,
        );
        assert!(matches!(
            res,
            Ok(Some(InterpreterResult {
                result: InstructionResult::PrecompileError,
                ..
            }))
        ));

        // Test calling with value bigger than balance
        let mut input_vec = vec![0u8; 96];
        input_vec[31] = 1;
        input_vec[63] = 2;
        input_vec[95] = initial_balance + 10;
        let input = Bytes::from(input_vec);
        let big_value_input = InputsImpl { input, ..inputs };
        let res = precompiles.run(
            &mut ctx,
            &TRANSFER_ADDRESS,
            &big_value_input,
            false,
            TRANSFER_GAS_COST,
        );
        assert!(matches!(
            res,
            Ok(Some(InterpreterResult {
                result: InstructionResult::PrecompileError,
                ..
            }))
        ));
    }
}
