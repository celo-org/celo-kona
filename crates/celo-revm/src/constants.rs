use lazy_static::lazy_static;
use revm::primitives::{Address, HashMap, address};

/// Maximum size of contract code in bytes (64KB == 65536 bytes)
pub const CELO_MAX_CODE_SIZE: usize = 0x10000;

/// The system address used for Celo system calls.
pub const CELO_SYSTEM_ADDRESS: Address = Address::ZERO;

/// Error message prefix for CIP-64 fee currency debit failures.
pub const FEE_DEBIT_ERROR_PREFIX: &str = "Failed to debit gas fees";
/// Error message prefix for CIP-64 fee currency credit failures.
pub const FEE_CREDIT_ERROR_PREFIX: &str = "Failed to credit gas fees";

pub const CELO_MAINNET_CHAIN_ID: u64 = 42220;
pub const CELO_SEPOLIA_CHAIN_ID: u64 = 11142220;

pub struct CeloAddresses {
    pub celo_token: Address,
    pub fee_handler: Address,
    pub fee_currency_directory: Address,
}

// Static map of chain IDs to their addresses
lazy_static! {
    pub static ref CELO_ADDRESSES: HashMap<u64, CeloAddresses> = {
        let mut m = HashMap::default();

        m.insert(
            CELO_MAINNET_CHAIN_ID,
            CeloAddresses {
                celo_token: address!("0x471ece3750da237f93b8e339c536989b8978a438"),
                fee_handler: address!("0xcd437749e43a154c07f3553504c68fbfd56b8778"),
                fee_currency_directory: address!("0x15F344b9E6c3Cb6F0376A36A64928b13F62C6276"),
            },
        );

        m.insert(
            CELO_SEPOLIA_CHAIN_ID,
            CeloAddresses {
                celo_token: address!("0x471EcE3750Da237f93B8E339c536989b8978a438"),
                fee_handler: address!("0xcD437749E43A154C07F3553504c68fBfD56B8778"),
                fee_currency_directory: address!("0x9212Fb72ae65367A7c887eC4Ad9bE310BAC611BF"),
            },
        );

        m
    };
}

/// Returns the addresses for the given chain ID, or Celo Mainnet addresses if not found.
///
/// Logs a warning for unknown chain IDs since the Celo Mainnet addresses are almost
/// certainly wrong on other chains and will cause fee debit/credit to target
/// non-existent or incorrect contracts.
pub fn get_addresses(chain_id: u64) -> &'static CeloAddresses {
    CELO_ADDRESSES.get(&chain_id).unwrap_or_else(|| {
        tracing::warn!(
            target: "celo::constants",
            chain_id,
            "Unknown chain ID — falling back to Celo Mainnet contract addresses. \
             Fee currency operations will likely fail on this chain."
        );
        &CELO_ADDRESSES[&CELO_MAINNET_CHAIN_ID]
    })
}
