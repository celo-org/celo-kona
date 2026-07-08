#!/bin/bash
#
# E2E test for the Upgrade 18 (CGT v2) migration boundary.
#
# Starts a dedicated celo-reth dev node whose genesis schedules `upgrade18Time`
# a few seconds in the future (with the four artifact param overrides set via
# genesis extra fields), mines across the boundary, and asserts:
#
#   1. pre-fork blocks leave the predeploys untouched;
#   2. the activation block installs every predeploy byte-identical to the
#      pinned artifact (crates/alloy-celo-evm/res/predeploys.json): impl code,
#      proxy shell code, storage slots (params resolved to the overrides), and
#      the NativeAssetLiquidity reserve seed;
#   3. the transition applies exactly once (no double-mint on later blocks);
#   4. the installed contracts behave: LiquidityController owner/name/minters,
#      GoldToken duality sees the reserve, and CeloGasBridgeL2.withdraw()
#      locks native CELO into the reserve.
#
# Self-contained: uses its own ports/datadir and does not touch the shared
# dev node started by run_all_tests.sh.
set -eo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CELO_RETH="${CELO_RETH:-$REPO_ROOT/target/debug/celo-reth}"
ARTIFACT="$REPO_ROOT/crates/alloy-celo-evm/res/predeploys.json"

HTTP_PORT=8547
AUTH_PORT=8653
RPC="http://127.0.0.1:$HTTP_PORT"

# Dev account pre-funded in celo-dev-genesis.json (same as shared.sh).
ACC_ADDR=0x42cf1bbc38BaAA3c4898ce8790e21eD2738c6A4a
ACC_PRIVKEY=0x2771aff413cac48d9f8c114fabddd9195a2129f3c2c436caa07e27bb7f58ead5
TOKEN_ADDR=0x471ece3750da237f93b8e339c536989b8978a438 # GoldToken (token duality)

# Artifact param overrides (chain 1337 is unknown to the artifact, so all four
# are required — this also exercises the override path end-to-end).
OWNER=0x00000000000000000000000000000000000000aa
TOKEN_L1=0x00000000000000000000000000000000000000bb
BRIDGE_L1=0x00000000000000000000000000000000000000cc
SEED=1000000000000000000000000 # 1M CELO in wei

GAS_BRIDGE_L2=0x4200000000000000000000000000000000001023
LIQUIDITY_CONTROLLER=0x420000000000000000000000000000000000002a
RESERVE=0x4200000000000000000000000000000000000029

DATADIR=$(mktemp -d)
GENESIS=$(mktemp -t upgrade18-genesis.XXXXXX.json)
NODE_LOG="$SCRIPT_DIR/celo-reth-upgrade18.log"
NODE_PID=

cleanup() {
    if [[ -n "$NODE_PID" ]]; then
        kill "$NODE_PID" 2>/dev/null || true
        wait "$NODE_PID" 2>/dev/null || true
    fi
    rm -rf "$DATADIR" "$GENESIS"
}
trap cleanup EXIT

if lsof -ti :"$HTTP_PORT" &>/dev/null; then
    echo "Killing stale process on port $HTTP_PORT..."
    kill "$(lsof -ti :"$HTTP_PORT")" 2>/dev/null || true
    sleep 1
fi

# ---------------------------------------------------------------------------
# Genesis: schedule the fork shortly in the future, with param overrides
# ---------------------------------------------------------------------------

FORK_TS=$(($(date +%s) + 10))
python3 - "$SCRIPT_DIR/celo-dev-genesis.json" "$GENESIS" "$FORK_TS" <<'EOF'
import json, sys
src, dst, fork_ts = sys.argv[1], sys.argv[2], int(sys.argv[3])
genesis = json.load(open(src))
genesis["config"].update({
    "upgrade18Time": fork_ts,
    "upgrade18LiquidityControllerOwner": "0x00000000000000000000000000000000000000aa",
    "upgrade18CeloTokenL1": "0x00000000000000000000000000000000000000bb",
    "upgrade18CeloGasBridgeL1": "0x00000000000000000000000000000000000000cc",
    "upgrade18NativeAssetLiquidityAmount": "1000000000000000000000000",
})
# The dev genesis has no L2CrossDomainMessenger; withdraw()'s void
# `messenger.sendMessage(...)` call would fail solc's extcodesize existence
# check. Plant a STOP stub — the cross-domain transport is not under test.
genesis["alloc"]["0x4200000000000000000000000000000000000007"] = {
    "balance": "0x0",
    "code": "0x00",
}
json.dump(genesis, open(dst, "w"))
EOF
echo "Scheduled upgrade18Time=$FORK_TS ($(($FORK_TS - $(date +%s)))s from now)"

# ---------------------------------------------------------------------------
# Start the node
# ---------------------------------------------------------------------------

"$CELO_RETH" init --chain "$GENESIS" --datadir "$DATADIR" &>"$NODE_LOG"
"$CELO_RETH" node --dev \
    --chain "$GENESIS" \
    --datadir "$DATADIR" \
    --http \
    --http.port "$HTTP_PORT" \
    --http.api eth,web3,net,debug \
    --authrpc.port "$AUTH_PORT" \
    --disable-discovery \
    >>"$NODE_LOG" 2>&1 &
NODE_PID=$!

echo "Waiting for celo-reth (upgrade18 instance) to be ready..."
for _ in {1..60}; do
    if cast block-number --rpc-url "$RPC" &>/dev/null; then break; fi
    if ! kill -0 "$NODE_PID" 2>/dev/null; then
        echo "ERROR: node exited early"; tail -40 "$NODE_LOG"; exit 1
    fi
    sleep 0.5
done
cast block-number --rpc-url "$RPC" >/dev/null || { echo "ERROR: node not ready"; tail -40 "$NODE_LOG"; exit 1; }

# Mines one block (dev mode seals per transaction); prints its timestamp.
mine_block() {
    cast send --rpc-url "$RPC" --private-key "$ACC_PRIVKEY" --json \
        0x000000000000000000000000000000000000dEaD --value 0 |
        python3 -c 'import json,sys; print(int(json.load(sys.stdin)["blockNumber"], 0))' |
        xargs -I{} cast block {} --rpc-url "$RPC" --field timestamp
}

fail() { echo "FAIL: $1"; exit 1; }

# ---------------------------------------------------------------------------
# Pre-fork: predeploys untouched
# ---------------------------------------------------------------------------

[[ "$(cast code $GAS_BRIDGE_L2 --rpc-url "$RPC")" == "0x" ]] ||
    fail "CeloGasBridgeL2 must have no code at genesis"

PRE_TS=$(mine_block)
if ((PRE_TS < FORK_TS)); then
    [[ "$(cast code $GAS_BRIDGE_L2 --rpc-url "$RPC")" == "0x" ]] ||
        fail "predeploys must stay untouched before the fork (block ts $PRE_TS)"
    [[ "$(cast balance $RESERVE --rpc-url "$RPC")" == "0" ]] ||
        fail "reserve must be unfunded before the fork"
    echo "OK  pre-fork block ($PRE_TS < $FORK_TS): predeploys untouched"
else
    echo "WARN: first block already past the fork ($PRE_TS >= $FORK_TS); skipping pre-fork assertions"
fi

# ---------------------------------------------------------------------------
# Cross the boundary
# ---------------------------------------------------------------------------

NOW=$(date +%s)
if ((NOW <= FORK_TS)); then sleep $((FORK_TS - NOW + 1)); fi
BOUNDARY_TS=$(mine_block)
((BOUNDARY_TS >= FORK_TS)) || fail "boundary block ts $BOUNDARY_TS < fork ts $FORK_TS"
echo "OK  mined activation block (ts $BOUNDARY_TS >= $FORK_TS)"

# ---------------------------------------------------------------------------
# Artifact-driven state assertions: byte-identical to predeploys.json
# ---------------------------------------------------------------------------

python3 - "$ARTIFACT" "$RPC" "$OWNER" "$TOKEN_L1" "$BRIDGE_L1" "$SEED" <<'EOF'
import json, sys, urllib.request

artifact_path, rpc, owner, token_l1, bridge_l1, seed = sys.argv[1:7]
artifact = json.load(open(artifact_path))
overrides = {
    "liquidityControllerOwner": int(owner, 16),
    "celoTokenL1": int(token_l1, 16),
    "celoGasBridgeL1": int(bridge_l1, 16),
    "nativeAssetLiquidityAmount": int(seed),
}
req_id = 0

def rpc_call(method, params):
    global req_id
    req_id += 1
    body = json.dumps({"jsonrpc": "2.0", "id": req_id, "method": method, "params": params})
    req = urllib.request.Request(rpc, body.encode(), {"Content-Type": "application/json"})
    resp = json.load(urllib.request.urlopen(req))
    assert "error" not in resp, f"{method}{params}: {resp.get('error')}"
    return resp["result"]

def check(desc, got, want):
    assert got == want, f"{desc}: got {got}, want {want}"
    print(f"OK  {desc}")

shell = artifact["proxyShell"]["bytecode"].lower()
for p in artifact["predeploys"]:
    name = p["name"]
    # Implementation code, byte-identical to the artifact.
    code = rpc_call("eth_getCode", [p["impl"]["address"], "latest"]).lower()
    check(f"{name}: impl code bytes", code, p["impl"]["bytecode"].lower())
    # Proxy shell code.
    code = rpc_call("eth_getCode", [p["address"], "latest"]).lower()
    check(f"{name}: proxy shell bytes", code, shell)
    # Storage, with params resolved to the overrides.
    for entry in p["storage"]:
        value = entry["value"]
        want = overrides[value.removeprefix("param:")] if value.startswith("param:") else int(value, 16)
        got = int(rpc_call("eth_getStorageAt", [p["address"], entry["slot"], "latest"]), 16)
        check(f"{name}: slot {entry['slot'][:10]}… ({entry.get('note', '')})", got, want)

reserve = "0x4200000000000000000000000000000000000029"
got = int(rpc_call("eth_getBalance", [reserve, "latest"]), 16)
check("NativeAssetLiquidity: reserve seed", got, int(seed))
EOF

# ---------------------------------------------------------------------------
# Exactly-once: later blocks must not re-apply (no double mint)
# ---------------------------------------------------------------------------

mine_block >/dev/null
mine_block >/dev/null
[[ "$(cast balance $RESERVE --rpc-url "$RPC")" == "$SEED" ]] ||
    fail "reserve balance changed after the activation block (double mint?)"
echo "OK  transition applied exactly once across subsequent blocks"

# ---------------------------------------------------------------------------
# Behavior of the installed contracts
# ---------------------------------------------------------------------------

[[ "$(cast call $LIQUIDITY_CONTROLLER 'owner()(address)' --rpc-url "$RPC")" == \
   "$(cast to-check-sum-address $OWNER)" ]] || fail "LiquidityController owner mismatch"
NAME=$(cast call $LIQUIDITY_CONTROLLER 'gasPayingTokenName()(string)' --rpc-url "$RPC")
[[ "${NAME//\"/}" == "Celo" ]] || fail "gasPayingTokenName mismatch (got $NAME)"
[[ "$(cast call $LIQUIDITY_CONTROLLER "minters(address)(bool)" $GAS_BRIDGE_L2 --rpc-url "$RPC")" == "true" ]] ||
    fail "CeloGasBridgeL2 must be an authorized minter"
echo "OK  LiquidityController initialized (owner, token name, minter grant)"

# Token duality: GoldToken.balanceOf mirrors the native reserve balance.
[[ "$(cast call $TOKEN_ADDR 'balanceOf(address)(uint256)' $RESERVE --rpc-url "$RPC" | awk '{print $1}')" == "$SEED" ]] ||
    fail "GoldToken duality must see the seeded reserve"
echo "OK  token duality sees the reserve seed"

# Withdraw: locks native CELO into the reserve via LiquidityController.burn.
# (The cross-domain message part no-ops here: the dev genesis has no messenger
# predeploy, and a CALL to a codeless account succeeds.)
AMOUNT=12345
EXPECTED=$(python3 -c "print($SEED + $AMOUNT)") # SEED exceeds bash 64-bit arithmetic
cast send --rpc-url "$RPC" --private-key "$ACC_PRIVKEY" --json \
    $GAS_BRIDGE_L2 "withdraw(address,uint256,uint32,bytes)" \
    "$ACC_ADDR" "$AMOUNT" 200000 0x --value "$AMOUNT" >/dev/null ||
    fail "CeloGasBridgeL2.withdraw reverted"
[[ "$(cast balance $RESERVE --rpc-url "$RPC")" == "$EXPECTED" ]] ||
    fail "withdraw must lock native CELO into the reserve"
echo "OK  CeloGasBridgeL2.withdraw locks native CELO into the reserve"

echo "PASS: Upgrade 18 migration boundary"
