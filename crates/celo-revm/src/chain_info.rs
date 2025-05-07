use lazy_static::lazy_static;
use revm::primitives::{Address, HashMap, address};

pub const CELO_MAINNET_CHAIN_ID: u64 = 42220;
pub const CELO_ALFAJORES_CHAIN_ID: u64 = 44787;
pub const CELO_BAKLAVA_CHAIN_ID: u64 = 62320;

pub struct CeloAddresses {
    pub celo_token: Address,
}

// Static map of chain IDs to their addresses
lazy_static! {
    pub static ref CELO_ADDRESSES: HashMap<u64, CeloAddresses> = {
        let mut m = HashMap::new();

        m.insert(
            CELO_MAINNET_CHAIN_ID,
            CeloAddresses {
                celo_token: address!("0x471ece3750da237f93b8e339c536989b8978a438"),
            },
        );

        m.insert(
            CELO_ALFAJORES_CHAIN_ID,
            CeloAddresses {
                celo_token: address!("0xF194afDf50B03e69Bd7D057c1Aa9e10c9954E4C9"),
            },
        );

        m.insert(
            CELO_BAKLAVA_CHAIN_ID,
            CeloAddresses {
                celo_token: address!("0xdDc9bE57f553fe75752D61606B94CBD7e0264eF8"),
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
