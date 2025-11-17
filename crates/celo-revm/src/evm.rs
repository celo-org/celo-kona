use crate::{CeloContext, CeloPrecompiles, fee_currency_context::FeeCurrencyContext};
use op_revm::{OpEvm, OpSpecId};
use revm::{
    Database, Inspector,
    context::{ContextError, Evm, FrameStack},
    context_interface::{Cfg, ContextTr},
    handler::{
        EvmTr, FrameInitOrResult, ItemOrResult, evm::FrameTr, instructions::EthInstructions,
    },
    inspector::InspectorEvmTr,
    interpreter::interpreter::EthInterpreter,
};

pub struct CeloEvm<DB: Database, INSP> {
    pub inner: OpEvm<
        CeloContext<DB>,
        INSP,
        EthInstructions<EthInterpreter, CeloContext<DB>>,
        CeloPrecompiles,
    >,
    pub fee_currency_context: FeeCurrencyContext,
}

impl<DB, INSP> CeloEvm<DB, INSP>
where
    DB: Database,
{
    pub fn new(ctx: CeloContext<DB>, inspector: INSP) -> Self {
        Self {
            inner: OpEvm(Evm {
                ctx,
                inspector,
                instruction: EthInstructions::new_mainnet(),
                precompiles: CeloPrecompiles::default(),
                frame_stack: FrameStack::new(),
            }),
            fee_currency_context: FeeCurrencyContext::default(),
        }
    }

    /// Consumed self and returns a new Evm type with given Inspector.
    pub fn with_inspector(self, inspector: INSP) -> CeloEvm<DB, INSP> {
        Self {
            inner: OpEvm(self.inner.0.with_inspector(inspector)),
            fee_currency_context: self.fee_currency_context,
        }
    }

    /// Consumes self and returns a new Evm type with given Precompiles.
    pub fn with_precompiles(self, precompiles: CeloPrecompiles) -> CeloEvm<DB, INSP> {
        Self {
            inner: OpEvm(self.inner.0.with_precompiles(precompiles)),
            fee_currency_context: self.fee_currency_context,
        }
    }

    /// Consumes self and returns the inner Inspector.
    pub fn into_inspector(self) -> INSP {
        self.inner.into_inspector()
    }
}

impl<DB, INSP> InspectorEvmTr for CeloEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
{
    type Inspector = INSP;

    #[inline]
    fn all_inspector(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &FrameStack<Self::Frame>,
        &Self::Inspector,
    ) {
        self.inner.all_inspector()
    }

    #[inline]
    fn all_mut_inspector(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
        &mut Self::Inspector,
    ) {
        self.inner.all_mut_inspector()
    }
}

impl<DB, INSP> EvmTr for CeloEvm<DB, INSP>
where
    DB: Database,
    CeloContext<DB>: ContextTr<Cfg: Cfg<Spec = OpSpecId>>,
{
    type Context = CeloContext<DB>;
    type Instructions = EthInstructions<EthInterpreter, CeloContext<DB>>;
    type Precompiles = CeloPrecompiles;
    type Frame = <op_revm::OpEvm<
        CeloContext<DB>,
        INSP,
        EthInstructions<EthInterpreter, CeloContext<DB>>,
        CeloPrecompiles,
    > as EvmTr>::Frame;

    #[inline]
    fn all(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &FrameStack<Self::Frame>,
    ) {
        self.inner.all()
    }

    #[inline]
    fn all_mut(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
    ) {
        self.inner.all_mut()
    }

    fn frame_init(
        &mut self,
        frame_input: <Self::Frame as FrameTr>::FrameInit,
    ) -> Result<
        ItemOrResult<&mut Self::Frame, <Self::Frame as FrameTr>::FrameResult>,
        ContextError<<<Self::Context as ContextTr>::Db as Database>::Error>,
    > {
        self.inner.frame_init(frame_input)
    }

    fn frame_run(
        &mut self,
    ) -> Result<
        FrameInitOrResult<Self::Frame>,
        ContextError<<<Self::Context as ContextTr>::Db as Database>::Error>,
    > {
        self.inner.frame_run()
    }

    #[doc = " Returns the result of the frame to the caller. Frame is popped from the frame stack."]
    #[doc = " Consumes the frame result or returns it if there is more frames to run."]
    fn frame_return_result(
        &mut self,
        result: <Self::Frame as FrameTr>::FrameResult,
    ) -> Result<
        Option<<Self::Frame as FrameTr>::FrameResult>,
        ContextError<<<Self::Context as ContextTr>::Db as Database>::Error>,
    > {
        self.inner.frame_return_result(result)
    }
}

#[cfg(test)]
mod tests {
    use crate::{CeloBuilder, CeloTransaction, DefaultCelo};
    use op_revm::{
        OpHaltReason, OpSpecId, precompiles::bn254_pair::GRANITE_MAX_INPUT_SIZE,
        transaction::deposit::DEPOSIT_TRANSACTION_TYPE,
    };
    use revm::{
        Context, ExecuteEvm, Journal,
        bytecode::opcode,
        context::{
            BlockEnv, CfgEnv, TxEnv,
            result::{ExecutionResult, OutOfGasError},
        },
        context_interface::result::HaltReason,
        database::{BENCH_CALLER, BENCH_CALLER_BALANCE, BENCH_TARGET, BenchmarkDB, EmptyDB},
        interpreter::gas::{InitialAndFloorGas, calculate_initial_tx_gas},
        precompile::{bls12_381_const, bls12_381_utils, bn254, secp256r1, u64_to_address},
        primitives::{Address, Bytes, TxKind, U256},
        state::Bytecode,
    };

    // TODO: add cip64 tx test

    #[test]
    fn test_deposit_tx() {
        let ctx = Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.enveloped_tx = None;
                tx.op_tx.deposit.mint = Some(100);
                tx.op_tx.base.tx_type = DEPOSIT_TRANSACTION_TYPE;
            })
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::HOLOCENE);

        let mut evm = ctx.build_celo();

        let output = evm.replay().unwrap();

        // balance should be 100
        assert_eq!(
            output
                .state
                .get(&Address::default())
                .map(|a| a.info.balance),
            Some(U256::from(100))
        );
    }

    #[test]
    fn test_halted_deposit_tx() {
        let ctx = Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.enveloped_tx = None;
                tx.op_tx.deposit.mint = Some(100);
                tx.op_tx.base.tx_type = DEPOSIT_TRANSACTION_TYPE;
                tx.op_tx.base.caller = BENCH_CALLER;
                tx.op_tx.base.kind = TxKind::Call(BENCH_TARGET);
            })
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::HOLOCENE)
            .with_db(BenchmarkDB::new_bytecode(Bytecode::new_legacy(
                [opcode::POP].into(),
            )));

        // POP would return a halt.
        let mut evm = ctx.build_celo();

        let output = evm.replay().unwrap();

        // balance should be 100 + previous balance
        assert_eq!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::FailedDeposit,
                gas_used: 16_777_216
            }
        );
        assert_eq!(
            output.state.get(&BENCH_CALLER).map(|a| a.info.balance),
            Some(U256::from(100) + BENCH_CALLER_BALANCE)
        );
    }

    fn p256verify_test_tx() -> Context<
        BlockEnv,
        CeloTransaction<TxEnv>,
        CfgEnv<OpSpecId>,
        EmptyDB,
        Journal<EmptyDB>,
        op_revm::L1BlockInfo,
    > {
        const SPEC_ID: OpSpecId = OpSpecId::FJORD;

        let InitialAndFloorGas { initial_gas, .. } =
            calculate_initial_tx_gas(SPEC_ID.into(), &[], false, 0, 0, 0);

        Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.kind = TxKind::Call(u64_to_address(secp256r1::P256VERIFY_ADDRESS));
                tx.op_tx.base.gas_limit = initial_gas + secp256r1::P256VERIFY_BASE_GAS_FEE;
            })
            .modify_cfg_chained(|cfg| cfg.spec = SPEC_ID)
    }

    #[test]
    fn test_tx_call_p256verify() {
        let ctx = p256verify_test_tx();

        let mut evm = ctx.build_celo();
        let output = evm.replay().unwrap();

        // assert successful call to P256VERIFY
        assert!(output.result.is_success());
    }

    #[test]
    fn test_halted_tx_call_p256verify() {
        let ctx = p256verify_test_tx().modify_tx_chained(|tx| tx.op_tx.base.gas_limit -= 1);

        let mut evm = ctx.build_celo();
        let output = evm.replay().unwrap();

        // assert out of gas for P256VERIFY
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::OutOfGas(OutOfGasError::Precompile)),
                ..
            }
        ));
    }

    fn bn254_pair_test_tx(
        spec: OpSpecId,
    ) -> Context<
        BlockEnv,
        CeloTransaction<TxEnv>,
        CfgEnv<OpSpecId>,
        EmptyDB,
        Journal<EmptyDB>,
        op_revm::L1BlockInfo,
    > {
        let input = Bytes::from([1; GRANITE_MAX_INPUT_SIZE + 2]);
        let InitialAndFloorGas { initial_gas, .. } =
            calculate_initial_tx_gas(spec.into(), &input[..], false, 0, 0, 0);

        Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.kind = TxKind::Call(bn254::pair::ADDRESS);
                tx.op_tx.base.data = input;
                tx.op_tx.base.gas_limit = initial_gas;
            })
            .modify_cfg_chained(|cfg| cfg.spec = spec)
    }

    #[test]
    fn test_halted_tx_call_bn254_pair_fjord() {
        let ctx = bn254_pair_test_tx(OpSpecId::FJORD);

        let mut evm = ctx.build_celo();
        let output = evm.replay().unwrap();

        // assert out of gas
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::OutOfGas(OutOfGasError::Precompile)),
                ..
            }
        ));
    }

    #[test]
    fn test_halted_tx_call_bn254_pair_granite() {
        let ctx = bn254_pair_test_tx(OpSpecId::GRANITE);

        let mut evm = ctx.build_celo();
        let output = evm.replay().unwrap();

        // assert bails early because input size too big
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::PrecompileErrorWithContext(ref msg)),
                ..
            } if msg == "bn254 invalid pair length"
        ));
    }

    #[test]
    fn test_halted_tx_call_bls12_381_g1_add_out_of_gas() {
        let ctx = Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.kind = TxKind::Call(bls12_381_const::G1_ADD_ADDRESS);
                tx.op_tx.base.gas_limit = 21_000 + bls12_381_const::G1_ADD_BASE_GAS_FEE - 1;
            })
            .modify_chain_chained(|l1_block| {
                l1_block.operator_fee_constant = Some(U256::ZERO);
                l1_block.operator_fee_scalar = Some(U256::ZERO)
            })
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::ISTHMUS);

        let mut evm = ctx.build_celo();

        let output = evm.replay().unwrap();

        // assert out of gas
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::OutOfGas(OutOfGasError::Precompile)),
                ..
            }
        ));
    }

    #[test]
    fn test_halted_tx_call_bls12_381_g1_add_input_wrong_size() {
        let ctx = Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.kind = TxKind::Call(bls12_381_const::G1_ADD_ADDRESS);
                tx.op_tx.base.gas_limit = 21_000 + bls12_381_const::G1_ADD_BASE_GAS_FEE;
            })
            .modify_chain_chained(|l1_block| {
                l1_block.operator_fee_constant = Some(U256::ZERO);
                l1_block.operator_fee_scalar = Some(U256::ZERO)
            })
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::ISTHMUS);

        let mut evm = ctx.build_celo();
        let output = evm.replay().unwrap();

        // assert fails post gas check, because input is wrong size
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::PrecompileErrorWithContext(ref msg)),
                ..
            } if msg == "bls12-381 g1 add input length error"
        ));
    }

    fn g1_msm_test_tx() -> Context<
        BlockEnv,
        CeloTransaction<TxEnv>,
        CfgEnv<OpSpecId>,
        EmptyDB,
        Journal<EmptyDB>,
        op_revm::L1BlockInfo,
    > {
        const SPEC_ID: OpSpecId = OpSpecId::ISTHMUS;

        let input = Bytes::from([1; bls12_381_const::G1_MSM_INPUT_LENGTH]);
        let InitialAndFloorGas { initial_gas, .. } =
            calculate_initial_tx_gas(SPEC_ID.into(), &input[..], false, 0, 0, 0);
        let gs1_msm_gas = bls12_381_utils::msm_required_gas(
            1,
            &bls12_381_const::DISCOUNT_TABLE_G1_MSM,
            bls12_381_const::G1_MSM_BASE_GAS_FEE,
        );

        Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.kind = TxKind::Call(bls12_381_const::G1_MSM_ADDRESS);
                tx.op_tx.base.data = input;
                tx.op_tx.base.gas_limit = initial_gas + gs1_msm_gas;
            })
            .modify_chain_chained(|l1_block| {
                l1_block.operator_fee_constant = Some(U256::ZERO);
                l1_block.operator_fee_scalar = Some(U256::ZERO)
            })
            .modify_cfg_chained(|cfg| cfg.spec = SPEC_ID)
    }

    #[test]
    fn test_halted_tx_call_bls12_381_g1_msm_input_wrong_size() {
        let ctx = g1_msm_test_tx()
            .modify_tx_chained(|tx| tx.op_tx.base.data = tx.op_tx.base.data.slice(1..));

        let mut evm = ctx.build_celo();
        let output = evm.replay().unwrap();

        // assert fails pre gas check, because input is wrong size
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::PrecompileErrorWithContext(ref msg)),
                ..
            } if msg == "bls12-381 g1 msm input length error"
        ));
    }

    #[test]
    fn test_halted_tx_call_bls12_381_g1_msm_out_of_gas() {
        let ctx = g1_msm_test_tx().modify_tx_chained(|tx| tx.op_tx.base.gas_limit -= 1);

        let mut evm = ctx.build_celo();
        let output = evm.replay().unwrap();

        // assert out of gas
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::OutOfGas(OutOfGasError::Precompile)),
                ..
            }
        ));
    }

    #[test]
    fn test_halted_tx_call_bls12_381_g1_msm_wrong_input_layout() {
        let ctx = g1_msm_test_tx();

        let mut evm = ctx.build_celo();
        let output = evm.replay().unwrap();

        // assert fails post gas check, because input is wrong layout
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::PrecompileErrorWithContext(ref msg)),
                ..
            } if msg == "bls12-381 fp 64 top bytes of input are not zero"
        ));
    }

    #[test]
    fn test_halted_tx_call_bls12_381_g2_add_out_of_gas() {
        let ctx = Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.kind = TxKind::Call(bls12_381_const::G2_ADD_ADDRESS);
                tx.op_tx.base.gas_limit = 21_000 + bls12_381_const::G2_ADD_BASE_GAS_FEE - 1;
            })
            .modify_chain_chained(|l1_block| {
                l1_block.operator_fee_constant = Some(U256::ZERO);
                l1_block.operator_fee_scalar = Some(U256::ZERO)
            })
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::ISTHMUS);

        let mut evm = ctx.build_celo();

        let output = evm.replay().unwrap();

        // assert out of gas
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::OutOfGas(OutOfGasError::Precompile)),
                ..
            }
        ));
    }

    #[test]
    fn test_halted_tx_call_bls12_381_g2_add_input_wrong_size() {
        let ctx = Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.kind = TxKind::Call(bls12_381_const::G2_ADD_ADDRESS);
                tx.op_tx.base.gas_limit = 21_000 + bls12_381_const::G2_ADD_BASE_GAS_FEE;
            })
            .modify_chain_chained(|l1_block| {
                l1_block.operator_fee_constant = Some(U256::ZERO);
                l1_block.operator_fee_scalar = Some(U256::ZERO)
            })
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::ISTHMUS);

        let mut evm = ctx.build_celo();

        let output = evm.replay().unwrap();

        // assert fails post gas check, because input is wrong size
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::PrecompileErrorWithContext(ref msg)),
                ..
            } if msg == "bls12-381 g2 add input length error"
        ));
    }

    fn g2_msm_test_tx() -> Context<
        BlockEnv,
        CeloTransaction<TxEnv>,
        CfgEnv<OpSpecId>,
        EmptyDB,
        Journal<EmptyDB>,
        op_revm::L1BlockInfo,
    > {
        const SPEC_ID: OpSpecId = OpSpecId::ISTHMUS;

        let input = Bytes::from([1; bls12_381_const::G2_MSM_INPUT_LENGTH]);
        let InitialAndFloorGas { initial_gas, .. } =
            calculate_initial_tx_gas(SPEC_ID.into(), &input[..], false, 0, 0, 0);
        let gs2_msm_gas = bls12_381_utils::msm_required_gas(
            1,
            &bls12_381_const::DISCOUNT_TABLE_G2_MSM,
            bls12_381_const::G2_MSM_BASE_GAS_FEE,
        );

        Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.kind = TxKind::Call(bls12_381_const::G2_MSM_ADDRESS);
                tx.op_tx.base.data = input;
                tx.op_tx.base.gas_limit = initial_gas + gs2_msm_gas;
            })
            .modify_chain_chained(|l1_block| {
                l1_block.operator_fee_constant = Some(U256::ZERO);
                l1_block.operator_fee_scalar = Some(U256::ZERO)
            })
            .modify_cfg_chained(|cfg| cfg.spec = SPEC_ID)
    }

    #[test]
    fn test_halted_tx_call_bls12_381_g2_msm_input_wrong_size() {
        let ctx = g2_msm_test_tx()
            .modify_tx_chained(|tx| tx.op_tx.base.data = tx.op_tx.base.data.slice(1..));

        let mut evm = ctx.build_celo();
        let output = evm.replay().unwrap();

        // assert fails pre gas check, because input is wrong size
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::PrecompileErrorWithContext(ref msg)),
                ..
            } if msg == "bls12-381 g2 msm input length error"
        ));
    }

    #[test]
    fn test_halted_tx_call_bls12_381_g2_msm_out_of_gas() {
        let ctx = g2_msm_test_tx().modify_tx_chained(|tx| tx.op_tx.base.gas_limit -= 1);

        let mut evm = ctx.build_celo();
        let output = evm.replay().unwrap();

        // assert out of gas
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::OutOfGas(OutOfGasError::Precompile)),
                ..
            }
        ));
    }

    #[test]
    fn test_halted_tx_call_bls12_381_g2_msm_wrong_input_layout() {
        let ctx = g2_msm_test_tx();

        let mut evm = ctx.build_celo();
        let output = evm.replay().unwrap();

        // assert fails post gas check, because input is wrong layout
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::PrecompileErrorWithContext(ref msg)),
                ..
            } if msg == "bls12-381 fp 64 top bytes of input are not zero"
        ));
    }

    fn bl12_381_pairing_test_tx() -> Context<
        BlockEnv,
        CeloTransaction<TxEnv>,
        CfgEnv<OpSpecId>,
        EmptyDB,
        Journal<EmptyDB>,
        op_revm::L1BlockInfo,
    > {
        const SPEC_ID: OpSpecId = OpSpecId::ISTHMUS;

        let input = Bytes::from([1; bls12_381_const::PAIRING_INPUT_LENGTH]);
        let InitialAndFloorGas { initial_gas, .. } =
            calculate_initial_tx_gas(SPEC_ID.into(), &input[..], false, 0, 0, 0);

        let pairing_gas: u64 =
            bls12_381_const::PAIRING_MULTIPLIER_BASE + bls12_381_const::PAIRING_OFFSET_BASE;

        Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.kind = TxKind::Call(bls12_381_const::PAIRING_ADDRESS);
                tx.op_tx.base.data = input;
                tx.op_tx.base.gas_limit = initial_gas + pairing_gas;
            })
            .modify_chain_chained(|l1_block| {
                l1_block.operator_fee_constant = Some(U256::ZERO);
                l1_block.operator_fee_scalar = Some(U256::ZERO)
            })
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::ISTHMUS)
    }

    #[test]
    fn test_halted_tx_call_bls12_381_pairing_input_wrong_size() {
        let ctx = bl12_381_pairing_test_tx()
            .modify_tx_chained(|tx| tx.op_tx.base.data = tx.op_tx.base.data.slice(1..));

        let mut evm = ctx.build_celo();
        let output = evm.replay().unwrap();

        // assert fails pre gas check, because input is wrong size
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::PrecompileErrorWithContext(ref msg)),
                ..
            } if msg == "bls12-381 pairing input length error"
        ));
    }

    #[test]
    fn test_halted_tx_call_bls12_381_pairing_out_of_gas() {
        let ctx = bl12_381_pairing_test_tx().modify_tx_chained(|tx| tx.op_tx.base.gas_limit -= 1);

        let mut evm = ctx.build_celo();
        let output = evm.replay().unwrap();

        // assert out of gas
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::OutOfGas(OutOfGasError::Precompile)),
                ..
            }
        ));
    }

    #[test]
    fn test_tx_call_bls12_381_pairing_wrong_input_layout() {
        let ctx = bl12_381_pairing_test_tx();

        let mut evm = ctx.build_celo();
        let output = evm.replay().unwrap();

        // assert fails post gas check, because input is wrong layout
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::PrecompileErrorWithContext(ref msg)),
                ..
            } if msg == "bls12-381 fp 64 top bytes of input are not zero"
        ));
    }

    fn fp_to_g1_test_tx() -> Context<
        BlockEnv,
        CeloTransaction<TxEnv>,
        CfgEnv<OpSpecId>,
        EmptyDB,
        Journal<EmptyDB>,
        op_revm::L1BlockInfo,
    > {
        const SPEC_ID: OpSpecId = OpSpecId::ISTHMUS;

        let input = Bytes::from([1; bls12_381_const::PADDED_FP_LENGTH]);
        let InitialAndFloorGas { initial_gas, .. } =
            calculate_initial_tx_gas(SPEC_ID.into(), &input[..], false, 0, 0, 0);

        Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.kind = TxKind::Call(bls12_381_const::MAP_FP_TO_G1_ADDRESS);
                tx.op_tx.base.data = input;
                tx.op_tx.base.gas_limit = initial_gas + bls12_381_const::MAP_FP_TO_G1_BASE_GAS_FEE;
            })
            .modify_chain_chained(|l1_block| {
                l1_block.operator_fee_constant = Some(U256::ZERO);
                l1_block.operator_fee_scalar = Some(U256::ZERO)
            })
            .modify_cfg_chained(|cfg| cfg.spec = SPEC_ID)
    }

    #[test]
    fn test_halted_tx_call_bls12_381_map_fp_to_g1_out_of_gas() {
        let ctx = fp_to_g1_test_tx().modify_tx_chained(|tx| tx.op_tx.base.gas_limit -= 1);

        let mut evm = ctx.build_celo();
        let output = evm.replay().unwrap();

        // assert out of gas
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::OutOfGas(OutOfGasError::Precompile)),
                ..
            }
        ));
    }

    #[test]
    fn test_halted_tx_call_bls12_381_map_fp_to_g1_input_wrong_size() {
        let ctx = fp_to_g1_test_tx()
            .modify_tx_chained(|tx| tx.op_tx.base.data = tx.op_tx.base.data.slice(1..));

        let mut evm = ctx.build_celo();
        let output = evm.replay().unwrap();

        // assert fails post gas check, because input is wrong size
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::PrecompileErrorWithContext(ref msg)),
                ..
            } if msg == "bls12-381 map fp to g1 input length error"
        ));
    }

    fn fp2_to_g2_test_tx() -> Context<
        BlockEnv,
        CeloTransaction<TxEnv>,
        CfgEnv<OpSpecId>,
        EmptyDB,
        Journal<EmptyDB>,
        op_revm::L1BlockInfo,
    > {
        const SPEC_ID: OpSpecId = OpSpecId::ISTHMUS;

        let input = Bytes::from([1; bls12_381_const::PADDED_FP2_LENGTH]);
        let InitialAndFloorGas { initial_gas, .. } =
            calculate_initial_tx_gas(SPEC_ID.into(), &input[..], false, 0, 0, 0);

        Context::celo()
            .modify_tx_chained(|tx| {
                tx.op_tx.base.kind = TxKind::Call(bls12_381_const::MAP_FP2_TO_G2_ADDRESS);
                tx.op_tx.base.data = input;
                tx.op_tx.base.gas_limit = initial_gas + bls12_381_const::MAP_FP2_TO_G2_BASE_GAS_FEE;
            })
            .modify_chain_chained(|l1_block| {
                l1_block.operator_fee_constant = Some(U256::ZERO);
                l1_block.operator_fee_scalar = Some(U256::ZERO)
            })
            .modify_cfg_chained(|cfg| cfg.spec = SPEC_ID)
    }

    #[test]
    fn test_halted_tx_call_bls12_381_map_fp2_to_g2_out_of_gas() {
        let ctx = fp2_to_g2_test_tx().modify_tx_chained(|tx| tx.op_tx.base.gas_limit -= 1);

        let mut evm = ctx.build_celo();
        let output = evm.replay().unwrap();

        // assert out of gas
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::OutOfGas(OutOfGasError::Precompile)),
                ..
            }
        ));
    }

    #[test]
    fn test_halted_tx_call_bls12_381_map_fp2_to_g2_input_wrong_size() {
        let ctx = fp2_to_g2_test_tx()
            .modify_tx_chained(|tx| tx.op_tx.base.data = tx.op_tx.base.data.slice(1..));

        let mut evm = ctx.build_celo();
        let output = evm.replay().unwrap();

        // assert fails post gas check, because input is wrong size
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::PrecompileErrorWithContext(ref msg)),
                ..
            } if msg == "bls12-381 map fp2 to g2 input length error"
        ));
    }
}
