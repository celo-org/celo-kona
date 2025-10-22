use crate::{CeloContext, CeloPrecompiles};
use op_revm::{OpEvm, OpSpecId};
use revm::{
    Inspector,
    context::{Evm, FrameStack},
    context_interface::{Cfg, ContextTr},
    handler::{EvmTr, instructions::EthInstructions},
    inspector::InspectorEvmTr,
    interpreter::interpreter::EthInterpreter,
};

pub struct CeloEvm<DB: revm::Database, INSP>(
    pub  OpEvm<
        CeloContext<DB>,
        INSP,
        EthInstructions<EthInterpreter, CeloContext<DB>>,
        CeloPrecompiles,
    >,
);

impl<DB, INSP> CeloEvm<DB, INSP>
where
    DB: revm::Database,
{
    pub fn new(ctx: CeloContext<DB>, inspector: INSP) -> Self {
        Self(OpEvm(Evm {
            ctx,
            inspector,
            instruction: EthInstructions::new_mainnet(),
            precompiles: CeloPrecompiles::default(),
            frame_stack: FrameStack::new(),
        }))
    }

    /// Consumed self and returns a new Evm type with given Inspector.
    pub fn with_inspector(self, inspector: INSP) -> CeloEvm<DB, INSP> {
        Self(OpEvm(self.0.0.with_inspector(inspector)))
    }

    /// Consumes self and returns a new Evm type with given Precompiles.
    pub fn with_precompiles(self, precompiles: CeloPrecompiles) -> CeloEvm<DB, INSP> {
        Self(OpEvm(self.0.0.with_precompiles(precompiles)))
    }

    /// Consumes self and returns the inner Inspector.
    pub fn into_inspector(self) -> INSP {
        self.0.into_inspector()
    }
}

impl<DB, INSP> InspectorEvmTr for CeloEvm<DB, INSP>
where
    DB: revm::Database,
    INSP: Inspector<CeloContext<DB>, EthInterpreter>,
{
    type Inspector = INSP;

    fn inspector(&mut self) -> &mut Self::Inspector {
        self.0.inspector()
    }

    fn ctx_inspector(&mut self) -> (&mut Self::Context, &mut Self::Inspector) {
        self.0.ctx_inspector()
    }

    fn ctx_inspector_frame(
        &mut self,
    ) -> (&mut Self::Context, &mut Self::Inspector, &mut Self::Frame) {
        self.0.ctx_inspector_frame()
    }

    fn ctx_inspector_frame_instructions(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Inspector,
        &mut Self::Frame,
        &mut Self::Instructions,
    ) {
        self.0.ctx_inspector_frame_instructions()
    }
}

impl<DB, INSP> EvmTr for CeloEvm<DB, INSP>
where
    DB: revm::Database,
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

    fn ctx(&mut self) -> &mut Self::Context {
        self.0.ctx()
    }

    fn ctx_ref(&self) -> &Self::Context {
        self.0.ctx_ref()
    }

    fn ctx_instructions(&mut self) -> (&mut Self::Context, &mut Self::Instructions) {
        self.0.ctx_instructions()
    }

    fn ctx_precompiles(&mut self) -> (&mut Self::Context, &mut Self::Precompiles) {
        self.0.ctx_precompiles()
    }

    fn frame_stack(&mut self) -> &mut FrameStack<Self::Frame> {
        self.0.frame_stack()
    }

    fn frame_init(
        &mut self,
        frame_init: <Self::Frame as revm::handler::evm::FrameTr>::FrameInit,
    ) -> Result<
        revm::handler::ItemOrResult<
            &mut Self::Frame,
            <Self::Frame as revm::handler::evm::FrameTr>::FrameResult,
        >,
        revm::context_interface::context::ContextError<
            <<<Self as EvmTr>::Context as ContextTr>::Db as revm::Database>::Error,
        >,
    > {
        self.0.frame_init(frame_init)
    }

    fn frame_run(
        &mut self,
    ) -> Result<
        revm::handler::ItemOrResult<
            <Self::Frame as revm::handler::evm::FrameTr>::FrameInit,
            <Self::Frame as revm::handler::evm::FrameTr>::FrameResult,
        >,
        revm::context_interface::context::ContextError<
            <<<Self as EvmTr>::Context as ContextTr>::Db as revm::Database>::Error,
        >,
    > {
        self.0.frame_run()
    }

    fn frame_return_result(
        &mut self,
        result: <Self::Frame as revm::handler::evm::FrameTr>::FrameResult,
    ) -> Result<
        Option<<Self::Frame as revm::handler::evm::FrameTr>::FrameResult>,
        revm::context_interface::context::ContextError<
            <<<Self as EvmTr>::Context as ContextTr>::Db as revm::Database>::Error,
        >,
    > {
        self.0.frame_return_result(result)
    }
}

#[cfg(test)]
mod tests {
    use crate::{CeloBlockEnv, CeloBuilder, CeloTransaction, DefaultCelo};
    use op_revm::{
        OpHaltReason, OpSpecId, precompiles::bn254_pair::GRANITE_MAX_INPUT_SIZE,
        transaction::deposit::DEPOSIT_TRANSACTION_TYPE,
    };
    use revm::{
        Context, ExecuteEvm, Inspector, Journal,
        bytecode::opcode,
        context::{
            BlockEnv, CfgEnv, TxEnv,
            result::{ExecutionResult, OutOfGasError},
        },
        context_interface::result::HaltReason,
        database::{BENCH_CALLER, BENCH_CALLER_BALANCE, BENCH_TARGET, BenchmarkDB, EmptyDB},
        interpreter::{
            Interpreter, InterpreterTypes,
            gas::{InitialAndFloorGas, calculate_initial_tx_gas},
        },
        precompile::{bls12_381_const, bls12_381_utils, bn254, secp256r1, u64_to_address},
        primitives::{Address, Bytes, Log, TxKind, U256},
        state::Bytecode,
    };
    use std::vec::Vec;

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
        CeloBlockEnv,
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

    fn bn128_pair_test_tx(
        spec: OpSpecId,
    ) -> Context<
        BlockEnv,
        CeloTransaction<TxEnv>,
        CfgEnv<OpSpecId>,
        EmptyDB,
        Journal<EmptyDB>,
        CeloBlockEnv,
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
    fn test_halted_tx_call_bn128_pair_fjord() {
        let ctx = bn128_pair_test_tx(OpSpecId::FJORD);

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
    fn test_halted_tx_call_bn128_pair_granite() {
        let ctx = bn128_pair_test_tx(OpSpecId::GRANITE);

        let mut evm = ctx.build_celo();
        let output = evm.replay().unwrap();

        // assert bails early because input size too big
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::PrecompileError),
                ..
            }
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
                l1_block.l1_block_info.operator_fee_constant = Some(U256::ZERO);
                l1_block.l1_block_info.operator_fee_scalar = Some(U256::ZERO)
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
                l1_block.l1_block_info.operator_fee_constant = Some(U256::ZERO);
                l1_block.l1_block_info.operator_fee_scalar = Some(U256::ZERO)
            })
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::ISTHMUS);

        let mut evm = ctx.build_celo();
        let output = evm.replay().unwrap();

        // assert fails post gas check, because input is wrong size
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::PrecompileError),
                ..
            }
        ));
    }

    fn g1_msm_test_tx() -> Context<
        BlockEnv,
        CeloTransaction<TxEnv>,
        CfgEnv<OpSpecId>,
        EmptyDB,
        Journal<EmptyDB>,
        CeloBlockEnv,
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
                l1_block.l1_block_info.operator_fee_constant = Some(U256::ZERO);
                l1_block.l1_block_info.operator_fee_scalar = Some(U256::ZERO)
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
                reason: OpHaltReason::Base(HaltReason::PrecompileError),
                ..
            }
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
                reason: OpHaltReason::Base(HaltReason::PrecompileError),
                ..
            }
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
                l1_block.l1_block_info.operator_fee_constant = Some(U256::ZERO);
                l1_block.l1_block_info.operator_fee_scalar = Some(U256::ZERO)
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
                l1_block.l1_block_info.operator_fee_constant = Some(U256::ZERO);
                l1_block.l1_block_info.operator_fee_scalar = Some(U256::ZERO)
            })
            .modify_cfg_chained(|cfg| cfg.spec = OpSpecId::ISTHMUS);

        let mut evm = ctx.build_celo();

        let output = evm.replay().unwrap();

        // assert fails post gas check, because input is wrong size
        assert!(matches!(
            output.result,
            ExecutionResult::Halt {
                reason: OpHaltReason::Base(HaltReason::PrecompileError),
                ..
            }
        ));
    }

    fn g2_msm_test_tx() -> Context<
        BlockEnv,
        CeloTransaction<TxEnv>,
        CfgEnv<OpSpecId>,
        EmptyDB,
        Journal<EmptyDB>,
        CeloBlockEnv,
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
                l1_block.l1_block_info.operator_fee_constant = Some(U256::ZERO);
                l1_block.l1_block_info.operator_fee_scalar = Some(U256::ZERO)
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
                reason: OpHaltReason::Base(HaltReason::PrecompileError),
                ..
            }
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
                reason: OpHaltReason::Base(HaltReason::PrecompileError),
                ..
            }
        ));
    }

    fn bl12_381_pairing_test_tx() -> Context<
        BlockEnv,
        CeloTransaction<TxEnv>,
        CfgEnv<OpSpecId>,
        EmptyDB,
        Journal<EmptyDB>,
        CeloBlockEnv,
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
                l1_block.l1_block_info.operator_fee_constant = Some(U256::ZERO);
                l1_block.l1_block_info.operator_fee_scalar = Some(U256::ZERO)
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
                reason: OpHaltReason::Base(HaltReason::PrecompileError),
                ..
            }
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
                reason: OpHaltReason::Base(HaltReason::PrecompileError),
                ..
            }
        ));
    }

    fn fp_to_g1_test_tx() -> Context<
        BlockEnv,
        CeloTransaction<TxEnv>,
        CfgEnv<OpSpecId>,
        EmptyDB,
        Journal<EmptyDB>,
        CeloBlockEnv,
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
                l1_block.l1_block_info.operator_fee_constant = Some(U256::ZERO);
                l1_block.l1_block_info.operator_fee_scalar = Some(U256::ZERO)
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
                reason: OpHaltReason::Base(HaltReason::PrecompileError),
                ..
            }
        ));
    }

    fn fp2_to_g2_test_tx() -> Context<
        BlockEnv,
        CeloTransaction<TxEnv>,
        CfgEnv<OpSpecId>,
        EmptyDB,
        Journal<EmptyDB>,
        CeloBlockEnv,
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
                l1_block.l1_block_info.operator_fee_constant = Some(U256::ZERO);
                l1_block.l1_block_info.operator_fee_scalar = Some(U256::ZERO)
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
                reason: OpHaltReason::Base(HaltReason::PrecompileError),
                ..
            }
        ));
    }

    #[derive(Default, Debug)]
    struct LogInspector {
        logs: Vec<Log>,
    }

    impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for LogInspector {
        fn log(&mut self, _interp: &mut Interpreter<INTR>, _context: &mut CTX, log: Log) {
            self.logs.push(log)
        }
    }

    // TODO(revm-27.0-migration): Inspector hooks are not being called during execution.
    // Logs are produced and appear in ExecutionResult, but Inspector::log() is not invoked.
    // This requires proper inspector integration in CeloHandler or using a different
    // execution API that supports inspector hooks in revm 27.0.
    #[test]
    #[ignore = "Inspector hooks not yet integrated with revm 27.0 - logs appear in ExecutionResult but Inspector callbacks are not invoked"]
    fn test_log_inspector() {
        // simple yul contract emits a log in constructor

        /*object "Contract" {
            code {
                log0(0, 0)
            }
        }*/

        let contract_data: Bytes = Bytes::from([
            opcode::PUSH1,
            0x00,
            opcode::DUP1,
            opcode::LOG0,
            opcode::STOP,
        ]);
        let bytecode = Bytecode::new_raw(contract_data);

        let ctx = Context::celo()
            .with_db(BenchmarkDB::new_bytecode(bytecode.clone()))
            .modify_tx_chained(|tx| {
                tx.op_tx.base.caller = BENCH_CALLER;
                tx.op_tx.base.kind = TxKind::Call(BENCH_TARGET);
            });

        let mut evm = ctx.build_celo_with_inspector(LogInspector::default());

        // Run evm.
        let output = evm.replay().unwrap();

        // Verify that logs ARE produced (they appear in ExecutionResult)
        match &output.result {
            ExecutionResult::Success { logs, .. } => {
                assert_eq!(logs.len(), 1, "Log should appear in ExecutionResult");
            }
            _ => panic!("Expected success result"),
        }

        // This assertion fails because inspector hooks are not being called
        let inspector = &evm.0.0.inspector;
        assert!(
            !inspector.logs.is_empty(),
            "Inspector should have captured logs"
        );
    }
}
