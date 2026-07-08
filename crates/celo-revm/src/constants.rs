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

/// Error message prefix used when a CIP-64 transaction's fee currency is not
/// present in the per-block fee-currency context (the directory read failed, or
/// the currency was dropped while loading). It surfaces as an
/// `InvalidTransaction`, which excludes the transaction from the block. The EVM
/// layer matches this prefix to log and meter the otherwise-silent exclusion.
pub const FEE_CURRENCY_NOT_REGISTERED_PREFIX: &str = "fee currency not registered";

/// The Celo EIP-1559 base fee floor in wei (25 Gwei).
///
/// Applied as `max(computed_base_fee, CELO_EIP_1559_BASE_FEE_FLOOR)` for blocks before
/// Jovian activation. After Jovian, `min_base_fee` from the parent block's `extraData` is used.
pub const CELO_EIP_1559_BASE_FEE_FLOOR: u64 = 25_000_000_000;

pub const CELO_MAINNET_CHAIN_ID: u64 = 42220;
pub const CELO_SEPOLIA_CHAIN_ID: u64 = 11142220;
pub const CELO_CHAOS_CHAIN_ID: u64 = 11162320;

#[derive(Debug)]
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

        // Values read from the chaos L2's Celo Registry (0x…ce10): celo_token and fee_handler
        // match Mainnet/Sepolia, and fee_currency_directory matches Mainnet. Listed explicitly
        // so the chain is a table hit rather than relying on the Mainnet fallback.
        m.insert(
            CELO_CHAOS_CHAIN_ID,
            CeloAddresses {
                celo_token: address!("0x471EcE3750Da237f93B8E339c536989b8978a438"),
                fee_handler: address!("0xcD437749E43A154C07F3553504c68fBfD56B8778"),
                fee_currency_directory: address!("0x15F344b9E6c3Cb6F0376A36A64928b13F62C6276"),
            },
        );

        m
    };
}

/// Returns the addresses for the given chain ID, falling back to Celo Mainnet's
/// addresses if the chain is not in the table.
///
/// The fallback mirrors op-geth's `GetAddressesOrDefault(chainID, MainnetAddresses)`
/// and is correct for chains that reuse Mainnet's deterministic system-contract
/// addresses (e.g. dev and internal testnets). It is only wrong on a chain whose
/// addresses genuinely differ, so the miss is logged at `debug` rather than `warn`.
pub fn get_addresses(chain_id: u64) -> &'static CeloAddresses {
    CELO_ADDRESSES.get(&chain_id).unwrap_or_else(|| {
        tracing::debug!(
            target: "celo::constants",
            chain_id,
            "chain ID not in the known address table; using Celo Mainnet \
             system-contract addresses (correct for chains that reuse Mainnet's \
             deterministic addresses, e.g. dev/internal testnets)"
        );
        &CELO_ADDRESSES[&CELO_MAINNET_CHAIN_ID]
    })
}
