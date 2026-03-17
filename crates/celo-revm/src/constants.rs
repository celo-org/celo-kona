use lazy_static::lazy_static;
use revm::primitives::{Address, HashMap, address};

/// Maximum size of contract code in bytes (64KB == 65536 bytes)
pub const CELO_MAX_CODE_SIZE: usize = 0x10000;

/// The system address used for Celo system calls.
pub const CELO_SYSTEM_ADDRESS: Address = Address::ZERO;

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

/// Returns the addresses for the given chain ID, or Mainnet addresses if not found.
pub fn get_addresses(chain_id: u64) -> &'static CeloAddresses {
    CELO_ADDRESSES
        .get(&chain_id)
        .unwrap_or(&CELO_ADDRESSES[&CELO_MAINNET_CHAIN_ID])
}
