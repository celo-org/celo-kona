use lazy_static::lazy_static;
use revm::primitives::{Address, HashMap, address};

/// Maximum size of contract code in bytes (64KB == 65536 bytes)
pub const CELO_MAX_CODE_SIZE: usize = 0x10000;

pub const CELO_MAINNET_CHAIN_ID: u64 = 42220;
pub const CELO_ALFAJORES_CHAIN_ID: u64 = 44787;
pub const CELO_BAKLAVA_CHAIN_ID: u64 = 62320;

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
            CELO_ALFAJORES_CHAIN_ID,
            CeloAddresses {
                celo_token: address!("0xF194afDf50B03e69Bd7D057c1Aa9e10c9954E4C9"),
                fee_handler: address!("0xEAaFf71AB67B5d0eF34ba62Ea06Ac3d3E2dAAA38"),
                fee_currency_directory: address!("0x9212Fb72ae65367A7c887eC4Ad9bE310BAC611BF"),
            },
        );

        m.insert(
            CELO_BAKLAVA_CHAIN_ID,
            CeloAddresses {
                celo_token: address!("0xdDc9bE57f553fe75752D61606B94CBD7e0264eF8"),
                fee_handler: address!("0xeed0A69c51079114C280f7b936C79e24bD94013e"),
                fee_currency_directory: address!("0xD59E1599F45e42Eb356202B2C714D6C7b734C034"),
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
