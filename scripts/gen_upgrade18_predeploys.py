#!/usr/bin/env python3
"""Generate `crates/alloy-celo-evm/src/block/upgrade18_data.rs` from the pinned
CGT v2 predeploy activation artifact `crates/alloy-celo-evm/res/predeploys.json`
(produced by `celo-predeploys/generate.sh` in celo-org/optimism, tag
`celo-contracts/v6.0.0`).

Usage: python3 scripts/gen_upgrade18_predeploys.py

The output is a checked-in generated file: rerun this script whenever the JSON
artifact is updated (e.g. once the `celoGasBridgeL1` / `nativeAssetLiquidityAmount`
placeholders are finalized) and commit both files together.
"""

import json
import pathlib
import subprocess

ROOT = pathlib.Path(__file__).resolve().parent.parent
SRC = ROOT / "crates/alloy-celo-evm/res/predeploys.json"
DST = ROOT / "crates/alloy-celo-evm/src/block/upgrade18_data.rs"

PARAM_VARIANTS = {
    "liquidityControllerOwner": "LiquidityControllerOwner",
    "celoTokenL1": "CeloTokenL1",
    "celoGasBridgeL1": "CeloGasBridgeL1",
    "nativeAssetLiquidityAmount": "NativeAssetLiquidityAmount",
}

# Networks whose param values ship in the artifact, keyed by L2 chain id
# (see `celo_revm::constants::CELO_*_CHAIN_ID`).
NETWORKS = {"mainnet": 42_220, "sepolia": 11_142_220, "chaos": 11_162_320}


def u256_limbs(value: int) -> str:
    limbs = [(value >> (64 * i)) & 0xFFFF_FFFF_FFFF_FFFF for i in range(4)]
    return "U256::from_limbs([" + ", ".join(f"0x{l:x}" for l in limbs) + "])"


def slot_value(raw: str) -> str:
    if raw.startswith("param:"):
        return f"SlotValue::Param(Upgrade18Param::{PARAM_VARIANTS[raw.removeprefix('param:')]})"
    return f"SlotValue::Literal({u256_limbs(int(raw, 16))})"


def main() -> None:
    data = json.loads(SRC.read_text())
    build = data["build"]

    out = []
    w = out.append
    w("//! Upgrade 18 (CGT v2) predeploy activation data — GENERATED, DO NOT EDIT.")
    w("//!")
    w("//! Source artifact: `res/predeploys.json`, produced in celo-org/optimism at")
    w(f"//! `{build['gitBranch']}` (commit `{build['gitCommit']}`),")
    w(f"//! {build['forge']}; {build['settings']}.")
    w("//! Regenerate with `python3 scripts/gen_upgrade18_predeploys.py` and commit both files.")
    w("//!")
    w("//! Integrity: `super::upgrade18`'s tests assert `keccak256(code) == code_hash` for")
    w("//! every embedded bytecode, so a codegen or artifact mismatch fails the test suite.")
    w("")
    w("use super::upgrade18::Upgrade18Param;")
    w("use alloy_primitives::{Address, B256, U256, address, b256, hex};")
    w("")
    w("/// One 32-byte storage write; `Param` values are placeholders resolved at")
    w("/// activation time (override > per-network constant).")
    w("#[derive(Debug, Clone, Copy)]")
    w("pub(super) enum SlotValue {")
    w("    /// A literal word, identical on every network.")
    w("    Literal(U256),")
    w("    /// A network-specific placeholder.")
    w("    Param(Upgrade18Param),")
    w("}")
    w("")
    w("/// Target activation state for one predeploy: implementation code planted at the")
    w("/// deterministic `0xc0d3…` address, plus proxy-account storage (and balance).")
    w("#[derive(Debug, Clone, Copy)]")
    w("pub(super) struct RawPredeploy {")
    w("    /// Contract name — kept for tests and debugging output.")
    w("    #[allow(dead_code)]")
    w("    pub(super) name: &'static str,")
    w("    /// The proxy account (the predeploy address users interact with).")
    w("    pub(super) proxy: Address,")
    w("    /// Deterministic implementation address the proxy points at.")
    w("    pub(super) impl_address: Address,")
    w("    /// Implementation runtime bytecode.")
    w("    pub(super) impl_code: &'static [u8],")
    w("    /// `keccak256(impl_code)`, from the artifact.")
    w("    pub(super) impl_code_hash: B256,")
    w("    /// Native balance credited to the proxy (the NativeAssetLiquidity seed).")
    w("    pub(super) balance: Option<Upgrade18Param>,")
    w("    /// Storage writes applied to the proxy account.")
    w("    pub(super) storage: &'static [(U256, SlotValue)],")
    w("}")
    w("")
    shell = data["proxyShell"]
    w("/// `src/universal/Proxy.sol` runtime bytecode — planted on every predeploy proxy")
    w("/// account (idempotent for the proxies that already exist on-chain).")
    w(f"pub(super) const PROXY_SHELL_BYTECODE: &[u8] = &hex!(\"{shell['bytecode'].removeprefix('0x')}\");")
    w("")
    w("/// `keccak256(PROXY_SHELL_BYTECODE)`.")
    w(f"pub(super) const PROXY_SHELL_CODE_HASH: B256 = b256!(\"{shell['codeHash'].removeprefix('0x')}\");")
    w("")
    w("/// The full predeploy set, in artifact order.")
    w("pub(super) const RAW_PREDEPLOYS: &[RawPredeploy] = &[")
    for p in data["predeploys"]:
        imp = p["impl"]
        w("    RawPredeploy {")
        w(f"        name: \"{p['name']}\",")
        w(f"        proxy: address!(\"{p['address'].removeprefix('0x')}\"),")
        w(f"        impl_address: address!(\"{imp['address'].removeprefix('0x')}\"),")
        w(f"        impl_code: &hex!(\"{imp['bytecode'].removeprefix('0x')}\"),")
        w(f"        impl_code_hash: b256!(\"{imp['codeHash'].removeprefix('0x')}\"),")
        if "balance" in p:
            variant = PARAM_VARIANTS[p["balance"].removeprefix("param:")]
            w(f"        balance: Some(Upgrade18Param::{variant}),")
        else:
            w("        balance: None,")
        w("        storage: &[")
        for s in p["storage"]:
            note = s.get("note", "")
            comment = f" // {note}" if note else ""
            w(f"            ({u256_limbs(int(s['slot'], 16))}, {slot_value(s['value'])}),{comment}")
        w("        ],")
        w("    },")
    w("];")
    w("")
    w("/// Param values shipped in the artifact for known networks, keyed by L2 chain id.")
    w("/// The `celoGasBridgeL1` / `nativeAssetLiquidityAmount` placeholders are not yet")
    w("/// final on any network and must arrive via overrides (or a regenerated artifact).")
    w("pub(super) const fn known_param(chain_id: u64, param: Upgrade18Param) -> Option<U256> {")
    w("    match (chain_id, param) {")
    for pname, variant in PARAM_VARIANTS.items():
        values = data["params"][pname]
        if not isinstance(values, dict):
            continue  # unresolved placeholder — no known values
        for network, chain_id in NETWORKS.items():
            value = values.get(network)
            if value is None or not value.startswith("0x"):
                continue
            w(
                f"        ({chain_id}, Upgrade18Param::{variant}) => "
                f"Some({u256_limbs(int(value, 16))}), // {network}: {values.get('note', '')}"
            )
    w("        _ => None,")
    w("    }")
    w("}")
    w("")

    DST.write_text("\n".join(out))
    # Normalize to the repo's rustfmt style so the checked-in file is fmt-clean.
    subprocess.run(
        ["rustfmt", "+nightly", "--edition", "2024", str(DST)],
        check=True,
        cwd=ROOT,
    )
    print(f"wrote {DST} ({DST.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
