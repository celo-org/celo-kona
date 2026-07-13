#!/usr/bin/env python3
"""Assert the Upgrade 18 activation artifact is byte-identical to a fresh CGT v2 genesis.

Compares `crates/alloy-celo-evm/res/predeploys.json` against a `vm.dumpState` capture of
the pinned `L2Genesis.s.sol` run with `useCustomGasToken = true` (see `run.sh`), for the
six migrated predeploys — proxy and implementation accounts:

  * code must match the artifact byte-for-byte;
  * every artifact storage write (with `param:` placeholders resolved to the same values
    the genesis run received) must appear with the same value;
  * balances: the NativeAssetLiquidity reserve seed on the proxy, zero everywhere else;
  * nonces must be zero — the artifact carries no nonces and the migration leaves them
    untouched, which is only byte-identical while the reference genesis keeps its
    `vm.etch`-style zero-nonce allocs (the "nonce-0 assumption",
    celo-blockchain-planning#1413/#1417);

and classifies every genesis slot the artifact does *not* write into two closed,
value-asserted sets (`live_state` and `impl_init` below):

  * live_state — slots on the three *upgraded* proxies (SequencerFeeVault 0x…11,
    L1Block 0x…15, L2ToL1MessagePasser 0x…16). Those proxies already exist on the live
    chain — the migration only swaps their implementation — so the artifact rightly
    omits state they already carry: the EIP-1967 admin slot, and the fee vault's
    operating config (written by its v1 initialization, carried across the upgrade).
    That the live chain really does carry these slots is established by the live-chain
    audit (#1404), not here; this script only pins what a fresh genesis puts there.
  * impl_init — storage that `L2Genesis` writes to *implementation* accounts and the
    migration does not. The `initializer` guard lives in per-account storage, and
    initializing a proxy marks only the proxy — so an implementation's own
    `initialize()` stays publicly callable unless someone burns it. Constructed
    contracts use `_disableInitializers()` for that, but predeploy implementations are
    `vm.etch`ed and never run a constructor, so `L2Genesis` instead calls
    `initialize()` directly on each implementation with neutralizing values (the
    Parity-multisig / uninitialized-UUPS lesson). The migration artifact only captured
    proxy storage, so a migrated chain's fresh implementation accounts stay
    uninitialized. That is a hygiene gap, not an exploit: none of these contracts have
    owner-gated selfdestruct/delegatecall paths, and everything value-bearing
    authorizes by *proxy* address (e.g. `NativeAssetLiquidity` hard-checks
    `msg.sender == LIQUIDITY_CONTROLLER`). Pinned as a documented deviation; if the
    artifact grows implementation storage (#1413), this set must shrink to empty.

Anything outside these sets — a missing artifact write, a value mismatch, an unexpected
extra slot, a non-zero nonce — fails the check.

Zero-valued dump slots are discarded before comparing: `vm.dumpState` records writes,
but a zero-valued slot does not exist in the state trie, so writing zero and never
touching the slot are the same post-state.

Usage: CGT_* env vars set as in run.sh, then
  compare.py <predeploys.json> <state-dump.json>
"""

import json
import os
import sys

# EIP-1967 admin slot, keccak256("eip1967.proxy.admin") - 1.
ADMIN_SLOT = 0xB53127684A568B3173AE13B9F8A6016E243E63B6E8EE1178D6A717850B5D6103
# ERC-7201 storage location of OpenZeppelin v5 `Initializable` (used by `FeeVault` at the
# pinned commit; the other predeploys use the older OZ pattern with `_initialized` in slot 0).
INITIALIZABLE_SLOT = 0xF0C57E16840DF040F15088DC2F81FE391C3923BEC73E23A9662EFC9C229C6A00
PROXY_ADMIN = 0x4200000000000000000000000000000000000018
L2_CROSS_DOMAIN_MESSENGER = 0x4200000000000000000000000000000000000007

SEQUENCER_FEE_VAULT = "0x4200000000000000000000000000000000000011"
L1_BLOCK = "0x4200000000000000000000000000000000000015"
L2_TO_L1_MESSAGE_PASSER = "0x4200000000000000000000000000000000000016"


def env_addr(name: str) -> int:
    return int(os.environ[name], 16)


def env_int(name: str) -> int:
    return int(os.environ[name])


def main() -> int:
    artifact_path, dump_path = sys.argv[1], sys.argv[2]
    artifact = json.load(open(artifact_path))
    dump = {k.lower(): v for k, v in json.load(open(dump_path)).items()}

    params = {
        "liquidityControllerOwner": env_addr("CGT_LIQUIDITY_CONTROLLER_OWNER"),
        "celoTokenL1": env_addr("CGT_CELO_TOKEN_L1"),
        "celoGasBridgeL1": env_addr("CGT_CELO_GAS_BRIDGE_L1"),
        "nativeAssetLiquidityAmount": env_int("CGT_NATIVE_ASSET_LIQUIDITY_AMOUNT"),
    }
    seq_recipient = env_addr("CGT_SEQ_VAULT_RECIPIENT")
    seq_min = env_int("CGT_SEQ_VAULT_MIN_WITHDRAWAL")
    seq_network = env_int("CGT_SEQ_VAULT_NETWORK")

    def resolve(value: str) -> int:
        if value.startswith("param:"):
            return params[value.removeprefix("param:")]
        return int(value, 16)

    # Slots the migration expects the live pre-fork chain to already carry (the artifact
    # omits them on purpose — see the docstring), keyed by proxy address. The values are
    # derived from the same env inputs `run.sh` fed to the genesis run, so a fresh
    # genesis is fully pinned even where the live chain is the real source of truth.
    live_state = {
        SEQUENCER_FEE_VAULT: {
            ADMIN_SLOT: PROXY_ADMIN,
            0x1: seq_min,  # FeeVault.minWithdrawalAmount
            0x2: (seq_network << 160) | seq_recipient,  # withdrawalNetwork packed above recipient
            INITIALIZABLE_SLOT: 1,
        },
        L1_BLOCK: {ADMIN_SLOT: PROXY_ADMIN},
        L2_TO_L1_MESSAGE_PASSER: {ADMIN_SLOT: PROXY_ADMIN},
    }

    # `L2Genesis`'s direct-on-the-implementation `initialize()` writes (see the
    # docstring), keyed by predeploy name; the migration does not perform them. At the
    # pinned commit the call sites are `setCeloGasBridgeL2`, `setLiquidityController`,
    # and `_setFeeVault` in `L2Genesis.s.sol`. Only the non-zero writes appear here —
    # e.g. the zero addresses and empty strings the impl inits pass vanish with the
    # zero-slot normalization below.
    impl_init = {
        # initialize(_owner, "", "") — L2Genesis hands the *real* owner to the impl too,
        # so the impl's `_owner` (slot 0x33) resolves to the same param as the proxy's.
        "LiquidityController": {0x0: 1, 0x33: params["liquidityControllerOwner"]},
        # initialize(otherBridge=0, celoTokenL1=0) — StandardBridge's init always sets
        # `messenger` to the L2CrossDomainMessenger predeploy, even in the zeroed call.
        "CeloGasBridgeL2": {0x0: 1, 0x3: L2_CROSS_DOMAIN_MESSENGER},
        # initialize(recipient=0, minWithdrawalAmount=uint256.max, network=L1) —
        # "max value ... to make it unusable" per the L2Genesis comment.
        "CeloSequencerFeeVault": {0x1: (1 << 256) - 1, INITIALIZABLE_SLOT: 1},
    }

    failures = []

    def check(desc: str, ok: bool, detail: str = "") -> None:
        if ok:
            print(f"OK   {desc}")
        else:
            failures.append(f"{desc}{': ' + detail if detail else ''}")
            print(f"FAIL {desc}{': ' + detail if detail else ''}")

    shell = artifact["proxyShell"]["bytecode"].lower()
    for p in artifact["predeploys"]:
        name = p["name"]
        expected_accounts = [
            # (label, address, expected code, expected balance, expected extra slots)
            (
                f"{name} proxy",
                p["address"].lower(),
                shell,
                resolve(p["balance"]) if "balance" in p else 0,
                {s["slot"].lower(): (resolve(s["value"]), s.get("note", "")) for s in p["storage"]},
                live_state.get(p["address"].lower(), {}),
            ),
            (
                f"{name} impl",
                p["impl"]["address"].lower(),
                p["impl"]["bytecode"].lower(),
                0,
                {},
                impl_init.get(name, {}),
            ),
        ]
        for label, addr, code, balance, art_slots, extra_slots in expected_accounts:
            acc = dump.get(addr)
            if acc is None:
                check(f"{label} present in genesis", False, f"{addr} missing from state dump")
                continue

            check(
                f"{label}: code byte-identical ({len(code) // 2 - 1} bytes)",
                acc["code"].lower() == code,
                f"genesis has {len(acc['code']) // 2 - 1} bytes / different bytes",
            )
            check(f"{label}: nonce 0", int(acc["nonce"], 16) == 0, f"nonce {acc['nonce']}")
            check(
                f"{label}: balance {balance}",
                int(acc["balance"], 16) == balance,
                f"genesis balance {int(acc['balance'], 16)}",
            )

            # Zero-valued slots do not exist in the state trie; drop them.
            genesis_slots = {
                int(k, 16): int(v, 16) for k, v in acc.get("storage", {}).items() if int(v, 16) != 0
            }
            for slot_hex, (want, note) in art_slots.items():
                got = genesis_slots.pop(int(slot_hex, 16), None)
                check(
                    f"{label}: artifact slot ({note or slot_hex})",
                    got == want,
                    f"genesis {hex(got) if got is not None else 'absent'}, artifact {hex(want)}",
                )
            for slot, want in extra_slots.items():
                kind = "live-state" if label.endswith("proxy") else "impl-init deviation"
                got = genesis_slots.pop(slot, None)
                check(
                    f"{label}: {kind} slot {hex(slot)[:12]}…",
                    got == want,
                    f"genesis {hex(got) if got is not None else 'absent'}, expected {hex(want)}",
                )
            for slot, got in sorted(genesis_slots.items()):
                check(f"{label}: unclassified genesis slot {hex(slot)}", False, f"value {hex(got)}")

    if failures:
        print(f"\nFAIL: {len(failures)} mismatch(es) between the artifact and a fresh CGT v2 genesis")
        return 1
    print("\nPASS: artifact matches a fresh CGT v2 genesis (modulo the documented sets above)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
