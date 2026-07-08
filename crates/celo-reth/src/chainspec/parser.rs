use std::{io::Read, sync::Arc};

use alloy_genesis::Genesis;
use alloy_hardforks::ForkCondition;
use celo_alloy_consensus::pre_gingerbread_header_hash;
use eyre::{Context, eyre};
use reth_chainspec::ChainHardforks;
use reth_cli::chainspec::{ChainSpecParser, parse_genesis};
use reth_optimism_chainspec::OpChainSpec;
use reth_primitives_traits::SealedHeader;

use super::CeloHardfork;

/// Embedded zstd dictionary shared by every Celo genesis blob, lifted verbatim
/// from `celo-org/superchain-registry`'s `superchain/extra/dictionary`.
const CELO_GENESIS_DICTIONARY: &[u8] = include_bytes!("../../res/genesis/dictionary");

/// Compressed Celo mainnet genesis JSON (zstd, decoded with the shared dictionary).
const CELO_MAINNET_GENESIS_ZST: &[u8] = include_bytes!("../../res/genesis/mainnet.json.zst");

/// Compressed Celo Sepolia genesis JSON (zstd, decoded with the shared dictionary).
const CELO_SEPOLIA_GENESIS_ZST: &[u8] = include_bytes!("../../res/genesis/sepolia.json.zst");

/// Live mainnet config TOML — the `[hardforks]` table here is authoritative
/// for fork schedules added after the genesis JSON snapshot was frozen
/// (e.g. holocene/isthmus/jovian for Celo mainnet).
const CELO_MAINNET_TOML: &str = include_str!("../../res/genesis/mainnet.toml");

/// Live sepolia config TOML, same role as mainnet's.
const CELO_SEPOLIA_TOML: &str = include_str!("../../res/genesis/sepolia.toml");

/// Chain spec parser for Celo. Mirrors
/// [`reth_optimism_cli::chainspec::OpChainSpecParser`] but produces an
/// [`OpChainSpec`] whose [`ChainHardforks`] include `Gingerbread` and `Cel2` —
/// the two Celo-specific forks the upstream parser drops.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct CeloChainSpecParser;

impl ChainSpecParser for CeloChainSpecParser {
    type ChainSpec = OpChainSpec;

    const SUPPORTED_CHAINS: &'static [&'static str] = &["celo", "celo-sepolia"];

    fn parse(s: &str) -> eyre::Result<Arc<Self::ChainSpec>> {
        chain_value_parser(s)
    }
}

/// The value parser matches one of the embedded named chains, otherwise falls
/// through to [`parse_genesis`] (which accepts a JSON path or inline JSON
/// string — same behaviour as the upstream OP parser).
fn chain_value_parser(s: &str) -> eyre::Result<Arc<OpChainSpec>> {
    let mut genesis = match s {
        "celo" => decompress_genesis(CELO_MAINNET_GENESIS_ZST)
            .wrap_err("decoding embedded celo mainnet genesis")?,
        "celo-sepolia" => decompress_genesis(CELO_SEPOLIA_GENESIS_ZST)
            .wrap_err("decoding embedded celo-sepolia genesis")?,
        path_or_inline => parse_genesis(path_or_inline)?,
    };

    // For the embedded named chains, merge the live TOML's `[hardforks]` table
    // into `extra_fields` before constructing the spec. The genesis JSON
    // snapshot in celo-org/superchain-registry is frozen at the registry
    // export date and lags newer hardforks (e.g. holocene/isthmus/jovian on
    // mainnet). The TOML carries the up-to-date schedule.
    if let Some(toml_src) = match s {
        "celo" => Some(CELO_MAINNET_TOML),
        "celo-sepolia" => Some(CELO_SEPOLIA_TOML),
        _ => None,
    } {
        merge_toml_hardforks(&mut genesis, toml_src)
            .wrap_err("merging celo TOML hardforks into genesis")?;
    }

    let gingerbread_block = read_u64(&genesis, "gingerbreadBlock");
    let cel2_time = read_u64(&genesis, "cel2Time");
    let upgrade18_time = read_u64(&genesis, "upgrade18Time");
    let mut spec: OpChainSpec = genesis.into();
    patch_celo_hardforks(&mut spec.inner.hardforks, gingerbread_block, cel2_time, upgrade18_time);
    reseal_pre_gingerbread_genesis(&mut spec, gingerbread_block);
    Ok(Arc::new(spec))
}

/// If the chain has a Gingerbread fork at a block strictly above genesis,
/// reseal `spec.inner.genesis_header` with the pre-Gingerbread RLP hash so it
/// matches what op-geth peers announce on handshake.
///
/// Celo mainnet's L1 genesis was produced by IBFT consensus with a 10-field
/// header layout (no `ommers_hash` / `difficulty` / `gas_limit` / `mix_hash` /
/// `nonce`). [`alloy_consensus::Header`]'s standard RLP encoding always emits
/// the full Ethereum layout, so the post-merge hash diverges from the chain's
/// canonical block 0 hash. Without this reseal, peers reject us with
/// `mismatched genesis in status message`.
///
/// Celo Sepolia and any custom dev genesis have `gingerbread_block = 0` (or
/// `None`) — the fork is active at genesis, so the standard encoding is
/// authoritative and we leave the SealedHeader untouched.
///
/// **Do not re-run the parsed [`OpChainSpec`] through
/// [`OpChainSpecBuilder::build`](reth_optimism_chainspec::OpChainSpecBuilder)
/// or otherwise re-invoke `make_op_genesis_header` + `SealedHeader::seal_slow`
/// downstream — that recomputes the hash via the post-merge RLP layout and
/// drops the override, breaking peering on celo mainnet.** Mutate
/// `spec.inner` in place if you need to adjust hardforks; the
/// `ChainHardforks::insert` calls above already follow this contract.
fn reseal_pre_gingerbread_genesis(spec: &mut OpChainSpec, gingerbread_block: Option<u64>) {
    let needs_reseal = gingerbread_block.is_some_and(|gb| gb > 0);
    if !needs_reseal {
        return;
    }
    let header = spec.inner.genesis_header.header().clone();
    let hash = pre_gingerbread_header_hash(&header);
    spec.inner.genesis_header = SealedHeader::new(header, hash);
}

/// Reads `[hardforks]` from `toml_src` and copies every entry into
/// `genesis.config.extra_fields` using the camelCase key names
/// `OpChainInfo::extract_from` expects — e.g. `holocene_time = 1752073200`
/// becomes `holoceneTime: 1752073200`. Existing values are overwritten so the
/// TOML wins over a stale genesis JSON.
fn merge_toml_hardforks(genesis: &mut Genesis, toml_src: &str) -> eyre::Result<()> {
    let parsed: toml::Value = toml::from_str(toml_src)?;
    let Some(hardforks) = parsed.get("hardforks").and_then(toml::Value::as_table) else {
        return Ok(());
    };

    for (key, value) in hardforks {
        let Some(num) = value.as_integer().filter(|n| *n >= 0) else { continue };
        genesis
            .config
            .extra_fields
            .insert(snake_to_camel(key), serde_json::Value::Number((num as u64).into()));
    }
    Ok(())
}

/// Convert `snake_case` to `camelCase`. Tiny enough to inline; avoids an extra
/// crate just for this.
fn snake_to_camel(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut upper_next = false;
    for c in input.chars() {
        if c == '_' {
            upper_next = true;
        } else if upper_next {
            out.extend(c.to_uppercase());
            upper_next = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// Decompress one of the embedded `.json.zst` blobs using the bundled dictionary
/// and parse the result as a [`Genesis`].
fn decompress_genesis(compressed: &[u8]) -> eyre::Result<Genesis> {
    let mut decoder =
        zstd::stream::read::Decoder::with_dictionary(compressed, CELO_GENESIS_DICTIONARY)
            .map_err(|e| eyre!("constructing zstd decoder: {e}"))?;
    let mut buf = Vec::with_capacity(compressed.len() * 8);
    decoder.read_to_end(&mut buf).map_err(|e| eyre!("decompressing genesis: {e}"))?;
    Ok(serde_json::from_slice(&buf)?)
}

/// Inserts `Gingerbread`, `Cel2`, and `Upgrade18` into the chain hardfork list
/// at the given activation values. `ChainHardforks::insert` orders by
/// [`ForkCondition`]'s `Ord` impl, so positions are derived from the values
/// themselves — block-based forks slot among the other block forks,
/// timestamp-based forks among the time forks.
///
/// All values are optional. If the genesis omits one, the corresponding fork
/// is left out entirely (matches op-geth's behaviour for chains that never had
/// that fork — e.g. a hand-rolled celo dev genesis without `cel2Time`, or any
/// chain while Upgrade 18 is not yet scheduled).
fn patch_celo_hardforks(
    forks: &mut ChainHardforks,
    gingerbread_block: Option<u64>,
    cel2_time: Option<u64>,
    upgrade18_time: Option<u64>,
) {
    if let Some(block) = gingerbread_block {
        forks.insert(CeloHardfork::Gingerbread, ForkCondition::Block(block));
    }
    if let Some(time) = cel2_time {
        forks.insert(CeloHardfork::Cel2, ForkCondition::Timestamp(time));
    }
    if let Some(time) = upgrade18_time {
        forks.insert(CeloHardfork::Upgrade18, ForkCondition::Timestamp(time));
    }
}

fn read_u64(genesis: &Genesis, key: &str) -> Option<u64> {
    genesis.config.extra_fields.get(key).and_then(serde_json::Value::as_u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_hardforks::Hardfork;

    fn fork_condition(spec: &OpChainSpec, fork: impl Hardfork) -> ForkCondition {
        spec.inner.hardforks.fork(fork)
    }

    #[test]
    fn parses_celo_mainnet() {
        use reth_optimism_forks::OpHardfork;

        let spec = CeloChainSpecParser::parse("celo").expect("parse celo mainnet");
        assert_eq!(spec.inner.chain.id(), 42220);
        assert_eq!(
            fork_condition(&spec, CeloHardfork::Gingerbread),
            ForkCondition::Block(21_616_000),
            "gingerbread must be present at block 21,616,000",
        );
        assert_eq!(
            fork_condition(&spec, CeloHardfork::Cel2),
            ForkCondition::Timestamp(1_742_957_258),
            "cel2 must be present at the L2 cutover timestamp",
        );
        // Hardforks added to the TOML after the genesis JSON snapshot must be
        // merged in, otherwise ForkID misses them and op-geth peers reject us.
        assert_eq!(
            fork_condition(&spec, OpHardfork::Holocene),
            ForkCondition::Timestamp(1_752_073_200),
        );
        assert_eq!(
            fork_condition(&spec, OpHardfork::Isthmus),
            ForkCondition::Timestamp(1_752_073_200),
        );
        assert_eq!(
            fork_condition(&spec, OpHardfork::Jovian),
            ForkCondition::Timestamp(1_774_958_788),
        );
        // Upgrade 18 (CGT v2) is not scheduled yet on any embedded chain. When it is,
        // this assertion must move to a concrete timestamp.
        assert_eq!(fork_condition(&spec, CeloHardfork::Upgrade18), ForkCondition::Never);
        assert_eq!(crate::chainspec::upgrade18_time(spec.as_ref()), None);
    }

    /// `upgrade18_time` follows the same TOML → extra_fields → hardfork-list plumbing as
    /// the other forks and is surfaced through the `upgrade18_time` helper.
    #[test]
    fn patches_upgrade18_when_scheduled() {
        let mut forks = ChainHardforks::default();
        patch_celo_hardforks(&mut forks, None, Some(1_742_957_258), Some(1_900_000_000));
        assert_eq!(forks.fork(CeloHardfork::Cel2), ForkCondition::Timestamp(1_742_957_258));
        assert_eq!(forks.fork(CeloHardfork::Upgrade18), ForkCondition::Timestamp(1_900_000_000),);

        // Unscheduled leaves the fork out entirely.
        let mut forks = ChainHardforks::default();
        patch_celo_hardforks(&mut forks, None, None, None);
        assert_eq!(forks.fork(CeloHardfork::Upgrade18), ForkCondition::Never);
    }

    /// op-geth peers on celo mainnet send this genesis hash in their `eth/Status`
    /// handshake (it's also what `forno.celo.org` returns from
    /// `eth_getBlockByNumber("0x0")`). Without the pre-Gingerbread reseal we
    /// produce a different hash and trigger
    /// `mismatched genesis in status message` peer disconnects.
    #[test]
    fn celo_mainnet_genesis_hash_matches_opgeth() {
        use alloy_primitives::b256;

        let spec = CeloChainSpecParser::parse("celo").unwrap();
        assert_eq!(
            spec.inner.genesis_hash(),
            b256!("0x19ea3339d3c8cda97235bc8293240d5b9dadcdfbb5d4b0b90ee731cac1bd11c3"),
        );
    }

    #[test]
    fn parses_celo_sepolia() {
        use alloy_primitives::b256;

        let spec = CeloChainSpecParser::parse("celo-sepolia").expect("parse celo sepolia");
        assert_eq!(spec.inner.chain.id(), 11_142_220);
        // Sepolia activates cel2 at genesis (timestamp 0); gingerbread did not happen.
        assert!(matches!(fork_condition(&spec, CeloHardfork::Cel2), ForkCondition::Timestamp(_)));
        // Genesis hash matches forno.celo-sepolia.celo-testnet.org. Sepolia is
        // a fresh L2 (no pre-Gingerbread history), so the post-merge encoding
        // produced by `OpChainSpec::from(genesis)` is authoritative.
        assert_eq!(
            spec.inner.genesis_hash(),
            b256!("0xabcc77fb92a7ea1e7d330042d81200585d251355b33408ed1c27fb54175b1fe2"),
        );
    }

    #[test]
    fn unknown_chain_falls_back_to_genesis_parser() {
        let err =
            CeloChainSpecParser::parse("not-a-chain").expect_err("non-existent path must fail");
        // Confirms the fallback was hit (parse_genesis tries fs::read_to_string first).
        let msg = err.to_string();
        assert!(
            msg.contains("No such file") || msg.contains("not-a-chain"),
            "expected fallback error, got: {msg}",
        );
    }

    #[test]
    fn supported_chains_list() {
        assert_eq!(CeloChainSpecParser::SUPPORTED_CHAINS, &["celo", "celo-sepolia"]);
    }

    /// `From<Sealed<TxPostExec>> for CeloTxEnvelope` is `unreachable!` on the
    /// assumption that SDM post-exec txs never activate on Celo. SDM rides the
    /// Interop hardfork, so pin Interop as unscheduled in both embedded specs.
    #[test]
    fn sdm_never_active_on_celo_chains() {
        for chain in CeloChainSpecParser::SUPPORTED_CHAINS {
            let spec = CeloChainSpecParser::parse(chain).unwrap();
            assert!(
                !reth_optimism_evm::is_sdm_active_at_timestamp(spec.as_ref(), u64::MAX),
                "SDM activated on {chain}; revisit the unreachable! in \
                 `From<Sealed<TxPostExec>> for CeloTxEnvelope`",
            );
        }
    }
}
