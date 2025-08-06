use revm::{
    context::TxEnv,
    primitives::{Address, Bytes, TxKind},
};

pub const CELO_SYSTEM_ADDRESS: Address = Address::ZERO;

/// Creates the system transaction with default values and set data and tx call target to system contract address
/// that is going to be called.
///
/// The caller is set to be [`CELO_SYSTEM_ADDRESS`].
///
/// It is used inside [`SystemCallEvm`](revm::SystemCallEvm) trait to prepare EVM for system call execution.
pub trait CeloSystemCallTx {
    /// Creates new transaction for system call.
    fn new_system_tx(data: Bytes, system_contract_address: Address) -> Self;
}

impl CeloSystemCallTx for TxEnv {
    fn new_system_tx(data: Bytes, system_contract_address: Address) -> Self {
        TxEnv {
            caller: CELO_SYSTEM_ADDRESS,
            data,
            kind: TxKind::Call(system_contract_address),
            gas_limit: 30_000_000,
            ..Default::default()
        }
    }
}
