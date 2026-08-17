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

/// Marker distinguishing a [`FEE_DEBIT_ERROR_PREFIX`] error raised by the CIP-64 max-fee
/// check's `balanceOf` read from one raised by the `debitGasFees` call itself.
///
/// Both deliberately carry the debit prefix so the sequencing blocklist classifies them
/// alike, which makes this the only thing telling the two apart. It is a diagnostic, never a
/// classifier input, and it is `pub` only because the debit-fault tests that assert its
/// *absence* — pinning that they still fault in the debit, not in the pre-check that now runs
/// ahead of it — live in `alloy-celo-evm`.
///
/// The value is deliberately not the bare word `balanceOf`: the flattened error embeds a
/// fee currency's revert text, and `"ERC20: balanceOf query for the zero address"` is a real
/// message a token can revert with. A marker a contract can reproduce would make those
/// absence assertions — and any operator reading the log — unable to tell the two apart.
pub const FEE_BALANCE_READ_MARKER: &str = "max-fee balanceOf read";

/// Error message prefix used when a CIP-64 transaction's fee currency is not
/// present in the per-block fee-currency context (the directory read failed, or
/// the currency was dropped while loading). It surfaces as an
/// `InvalidTransaction`, which excludes the transaction from the block. The EVM
/// layer matches this prefix to log and meter the otherwise-silent exclusion.
pub const FEE_CURRENCY_NOT_REGISTERED_PREFIX: &str = "fee currency not registered";

/// Marker present in the `Display` output of a debit/credit failure caused by
/// the fee-currency contract *reverting* — as opposed to halting (e.g. running
/// out of its gas budget) or an EVM-level call failure.
///
/// The full rendering is `CoreContractError::ExecutionFailed`'s
/// `"core contract execution failed: "` prefix followed by the `"revert: …"`
/// arm built in `process_call_result` (`contracts/core_contracts.rs`), nested
/// under [`FEE_DEBIT_ERROR_PREFIX`]/[`FEE_CREDIT_ERROR_PREFIX`] by the
/// handler. The sequencing blocklist matches this marker to identify
/// ambiguous revert failures (canonically an underfunded sender's `ERC20:
/// transfer amount exceeds balance`) — a revert never blocklists the
/// currency; only a halt ([`FEE_CURRENCY_HALT_MARKER`]) does.
pub const FEE_CURRENCY_REVERT_MARKER: &str = "core contract execution failed: revert:";

/// Marker present in the `Display` output of a debit/credit failure caused by
/// the fee-currency contract *halting* — e.g. exhausting the debit/credit
/// call's gas budget or executing invalid bytecode.
///
/// The full rendering is `CoreContractError::ExecutionFailed`'s
/// `"core contract execution failed: "` prefix followed by the `"halt: …"`
/// arm built in `process_call_result` (`contracts/core_contracts.rs`), nested
/// under [`FEE_DEBIT_ERROR_PREFIX`]/[`FEE_CREDIT_ERROR_PREFIX`] by the
/// handler. The sequencing blocklist matches this marker to positively
/// identify an unambiguous currency fault: only halts blocklist. Failures
/// carrying neither this marker nor [`FEE_CURRENCY_REVERT_MARKER`] are
/// EVM-infrastructure errors (e.g. a database read failing mid-call,
/// `CoreContractError::Evm`) — the node's fault, not the currency's — and
/// must not blocklist either.
pub const FEE_CURRENCY_HALT_MARKER: &str = "core contract execution failed: halt:";

/// Marker present in the `Display` output of a fee-currency failure caused by the
/// contract returning data that does not ABI-decode as the expected type — e.g. a
/// `balanceOf` answering with fewer than 32 bytes.
///
/// The full rendering is `CoreContractError::ExecutionFailed`'s
/// `"core contract execution failed: "` prefix followed by the
/// `"malformed return data: …"` message built in `erc20::get_balance`
/// (`contracts/erc20.rs`), nested under [`FEE_DEBIT_ERROR_PREFIX`] by the
/// handler's max-fee balance check. A contract fully controls its return data and
/// the call itself *succeeded*, so — like a halt ([`FEE_CURRENCY_HALT_MARKER`])
/// and unlike a revert or an EVM-infrastructure error — this is unambiguously the
/// currency's fault, and the sequencing blocklist matches this marker to
/// blocklist it. Spoof safety relies on the classifier checking
/// [`FEE_CURRENCY_REVERT_MARKER`] first: revert payloads are the only
/// attacker-controlled bytes in the flattened error, and a revert can never reach
/// the decode path.
pub const FEE_CURRENCY_MALFORMED_RETURN_MARKER: &str =
    "core contract execution failed: malformed return data:";

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
