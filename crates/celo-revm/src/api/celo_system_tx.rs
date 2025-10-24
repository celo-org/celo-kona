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
    fn new_system_tx_with_caller(
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Self
    where
        Self: Sized,
    {
        Self::new_system_tx_with_gas_limit(caller, system_contract_address, data, 30_000_000)
    }

    /// Creates new transaction for system call with custom gas limit.
    fn new_system_tx_with_gas_limit(
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
        gas_limit: u64,
    ) -> Self;
}

impl CeloSystemCallTx for TxEnv {
    fn new_system_tx_with_gas_limit(
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
        gas_limit: u64,
    ) -> Self {
        TxEnv {
            caller,
            data,
            kind: TxKind::Call(system_contract_address),
            gas_limit,
            ..Default::default()
        }
    }
}
