#!/bin/bash
#
# Upgrade 18 byte-identity gate: asserts the CGT v2 activation artifact
# (`crates/alloy-celo-evm/res/predeploys.json`) produces the same post-fork predeploy
# state as a *fresh* v6.0.0 genesis (celo-blockchain-planning#1417, closing #1413's
# byte-identity checkbox).
#
# Mechanism: check out celo-org/optimism at the artifact's pinned `build.gitCommit`,
# run the canonical `L2Genesis.s.sol` with `useCustomGasToken = true` through the
# `L2GenesisCGTDump.s.sol` wrapper (vm.dumpState), and compare the six migrated
# predeploys byte-for-byte with `compare.py` — code, storage, balances, and the
# nonce-0 assumption. See compare.py's docstring for the two documented exception
# sets (live pre-fork state, implementation-init deviations).
#
# Checkout source:
#   CELO_OPTIMISM_DIR=/path/to/celo-org/optimism  use a temporary worktree of an
#                                                 existing local clone (fast, offline)
#   unset                                         shallow-clone the pinned commit from
#                                                 https://github.com/celo-org/optimism
#
# Requires: git, python3, forge (any version; the bytecode is pinned by the
# per-contract solc versions and `bytecode_hash = 'none'`).
set -eo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
ARTIFACT="$REPO_ROOT/crates/alloy-celo-evm/res/predeploys.json"

PINNED_COMMIT=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['build']['gitCommit'])" "$ARTIFACT")

# The artifact's `param:` placeholders. Arbitrary distinguishable values — they are fed
# to the genesis run and used to resolve the artifact, so both sides move together.
# Deliberately the same placeholders as e2e_test/test_upgrade18_migration.sh.
export CGT_LIQUIDITY_CONTROLLER_OWNER=0x00000000000000000000000000000000000000aa
export CGT_CELO_TOKEN_L1=0x00000000000000000000000000000000000000bb
export CGT_CELO_GAS_BRIDGE_L1=0x00000000000000000000000000000000000000cc
export CGT_NATIVE_ASSET_LIQUIDITY_AMOUNT=1000000000000000000000000 # 1M CELO in wei

# Sequencer fee vault config — lands in the vault proxy's live-state slots (compare.py
# resolves them from these values). Network 1 = L2; L1 reverts under CGT.
export CGT_SEQ_VAULT_RECIPIENT=0x0000000000000000000000000000000000009905
export CGT_SEQ_VAULT_MIN_WITHDRAWAL=10000000000000000000 # 10 ether
export CGT_SEQ_VAULT_NETWORK=1

TMP="${TMPDIR:-/tmp}"
WORK=$(mktemp -d "$TMP/upgrade18-genesis-parity.XXXXXX")
export CGT_STATE_DUMP_PATH="$WORK/state-dump.json"
CHECKOUT="$WORK/optimism"
USED_WORKTREE=

cleanup() {
    if [[ -n "$USED_WORKTREE" ]]; then
        git -C "$CELO_OPTIMISM_DIR" worktree remove --force "$CHECKOUT" 2>/dev/null || true
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT

fail() { echo "FAIL: $1"; exit 1; }

echo "Artifact pinned to celo-org/optimism $PINNED_COMMIT"

# ---------------------------------------------------------------------------
# Materialize the pinned checkout
# ---------------------------------------------------------------------------

if [[ -n "$CELO_OPTIMISM_DIR" ]]; then
    git -C "$CELO_OPTIMISM_DIR" rev-parse --verify -q "$PINNED_COMMIT^{commit}" >/dev/null ||
        git -C "$CELO_OPTIMISM_DIR" fetch origin "$PINNED_COMMIT" ||
        fail "commit $PINNED_COMMIT not found in $CELO_OPTIMISM_DIR"
    git -C "$CELO_OPTIMISM_DIR" worktree add --detach "$CHECKOUT" "$PINNED_COMMIT"
    USED_WORKTREE=1
else
    git init -q "$CHECKOUT"
    git -C "$CHECKOUT" remote add origin https://github.com/celo-org/optimism.git
    git -C "$CHECKOUT" fetch --depth 1 origin "$PINNED_COMMIT"
    git -C "$CHECKOUT" checkout -q FETCH_HEAD
fi

echo "Initializing contracts-bedrock submodules..."
git -C "$CHECKOUT" submodule update --init --depth 1 -- packages/contracts-bedrock/lib/ ||
    git -C "$CHECKOUT" submodule update --init -- packages/contracts-bedrock/lib/

# ---------------------------------------------------------------------------
# Build and run the genesis dump
# ---------------------------------------------------------------------------

CONTRACTS="$CHECKOUT/packages/contracts-bedrock"
cp "$SCRIPT_DIR/L2GenesisCGTDump.s.sol" "$CONTRACTS/scripts/"

echo "Building L2Genesis at the pinned commit..."
(cd "$CONTRACTS" && forge build scripts/L2GenesisCGTDump.s.sol)

echo "Running L2Genesis (useCustomGasToken = true)..."
(cd "$CONTRACTS" && forge script scripts/L2GenesisCGTDump.s.sol --sig 'dump()') >/dev/null
[[ -s "$CGT_STATE_DUMP_PATH" ]] || fail "L2Genesis produced no state dump"

# ---------------------------------------------------------------------------
# Compare
# ---------------------------------------------------------------------------

python3 "$SCRIPT_DIR/compare.py" "$ARTIFACT" "$CGT_STATE_DUMP_PATH"
