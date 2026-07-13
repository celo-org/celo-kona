#!/bin/bash
#
# Upgrade 18 cross-client smoke test: op-node driving celo-reth across the
# CGT v2 migration boundary through the engine API
# (celo-blockchain-planning#1417's op-node leg, manual precursor to the op-e2e
# fixture that is blocked on #1415).
#
# This exercises the one interface the in-repo tests cannot: the op-node ↔
# celo-reth contract. The dev-mode e2e test (e2e_test/test_upgrade18_migration.sh)
# already proves the migration itself byte-for-byte against the artifact, so the
# assertions here focus on the cross-client surface:
#
#   1. op-node parses a rollup.json carrying upgrade18_time (ParseRollupConfig
#      rejects unknown fields, so this is the shared-config contract; the four
#      param overrides are deliberately NOT op-node keys — they are a
#      proof-host/EL channel);
#   2. op-node sequences celo-reth blocks through the engine API;
#   3. the first L2 block with timestamp >= upgrade18_time — as derived by
#      op-node, not by dev-mode wall-clock mining — carries the migration
#      (spot-checked: bridge proxy installed, reserve seeded, owner set);
#   4. op-node keeps sequencing past the irregular state transition, and the
#      transition applies exactly once.
#
# Topology: anvil as a bare L1 (no OP contracts — with zero deposits and no
# batcher, the deposit-contract and system-config addresses are never read,
# which is fine for unsafe-head sequencing), celo-reth as the L2 EL, op-node
# as the sequencing CL.
#
# Requirements:
#   anvil + cast (foundry), jq, python3
#   celo-reth        target/debug/celo-reth (or $CELO_RETH)
#   op-node          $OP_NODE_BIN, or built from $CELO_OPTIMISM_DIR
#                    (branch with Upgrade 18 fork plumbing; needs go on PATH)
set -eo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
CELO_RETH="${CELO_RETH:-$REPO_ROOT/target/debug/celo-reth}"
ARTIFACT="$REPO_ROOT/crates/alloy-celo-evm/res/predeploys.json"
DEV_GENESIS="$REPO_ROOT/e2e_test/celo-dev-genesis.json"

L1_PORT=8945
L2_HTTP_PORT=8947
L2_AUTH_PORT=8953
OP_NODE_RPC_PORT=9945
L1_RPC="http://127.0.0.1:$L1_PORT"
L2_RPC="http://127.0.0.1:$L2_HTTP_PORT"

L1_CHAIN_ID=900
L2_BLOCK_TIME=2
# L2 blocks between genesis and the fork: enough for pre-fork assertions, small
# enough that the catching-up sequencer reaches it in seconds.
FORK_DELAY_BLOCKS=12

# Artifact param overrides — same placeholders as the dev-mode e2e test and the
# genesis parity gate (chain 1337 is unknown to the artifact, so all four are
# required). They go into the celo-reth genesis config (camelCase extras) only:
# op-node deliberately does not know the param keys (they are a proof-host
# channel — celo-kona's host would read them from its own rollup.json), while
# upgrade18_time itself is in op-node's rollup.json, mirroring a real
# deployment.
OWNER=0x00000000000000000000000000000000000000aa
TOKEN_L1=0x00000000000000000000000000000000000000bb
BRIDGE_L1=0x00000000000000000000000000000000000000cc
SEED=1000000000000000000000000 # 1M CELO in wei

GAS_BRIDGE_L2=0x4200000000000000000000000000000000001023
LIQUIDITY_CONTROLLER=0x420000000000000000000000000000000000002a
RESERVE=0x4200000000000000000000000000000000000029

WORK=$(mktemp -d "${TMPDIR:-/tmp}/upgrade18-opnode-smoke.XXXXXX")
GENESIS="$WORK/genesis.json"
ROLLUP="$WORK/rollup.json"
JWT="$WORK/jwt.txt"
DATADIR="$WORK/reth-data"
ANVIL_PID=
RETH_PID=
OP_NODE_PID=

cleanup() {
    for pid in "$OP_NODE_PID" "$RETH_PID" "$ANVIL_PID"; do
        if [[ -n "$pid" ]]; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
    echo "FAIL: $1"
    echo "--- op-node log tail ---"; tail -20 "$WORK/op-node.log" 2>/dev/null || true
    echo "--- celo-reth log tail ---"; tail -20 "$WORK/celo-reth.log" 2>/dev/null || true
    exit 1
}

for port in $L1_PORT $L2_HTTP_PORT $L2_AUTH_PORT $OP_NODE_RPC_PORT; do
    if lsof -ti :"$port" &>/dev/null; then
        echo "Killing stale process on port $port..."
        kill "$(lsof -ti :"$port")" 2>/dev/null || true
        sleep 1
    fi
done

# ---------------------------------------------------------------------------
# Binaries
# ---------------------------------------------------------------------------

[[ -x "$CELO_RETH" ]] || fail "celo-reth not found at $CELO_RETH (cargo build -p celo-reth)"

if [[ -z "$OP_NODE_BIN" ]]; then
    [[ -n "$CELO_OPTIMISM_DIR" ]] ||
        fail "set OP_NODE_BIN to an op-node with Upgrade 18 plumbing, or CELO_OPTIMISM_DIR to build one"
    echo "Building op-node from $CELO_OPTIMISM_DIR..."
    OP_NODE_BIN="$WORK/op-node"
    (cd "$CELO_OPTIMISM_DIR" && go build -o "$OP_NODE_BIN" ./op-node/cmd)
fi
[[ -x "$OP_NODE_BIN" ]] || fail "op-node binary $OP_NODE_BIN not executable"

python3 -c 'import secrets; print(secrets.token_hex(32))' > "$JWT"

# ---------------------------------------------------------------------------
# L1: bare anvil chain
# ---------------------------------------------------------------------------

anvil --chain-id $L1_CHAIN_ID --block-time 2 --port $L1_PORT --silent &>"$WORK/anvil.log" &
ANVIL_PID=$!
for _ in {1..30}; do
    if cast block-number --rpc-url "$L1_RPC" &>/dev/null; then break; fi
    sleep 0.5
done
L1_GENESIS_HASH=$(cast block 0 --rpc-url "$L1_RPC" --field hash)
L1_GENESIS_TS=$(cast block 0 --rpc-url "$L1_RPC" --field timestamp)
echo "L1 up: genesis $L1_GENESIS_HASH @ $L1_GENESIS_TS"

# ---------------------------------------------------------------------------
# L2 genesis: dev genesis, re-timed to L1 genesis, Upgrade 18 scheduled
# ---------------------------------------------------------------------------

# op-node derives L2 timestamps as l2_time + n*block_time and requires them to
# be >= the L1 origin timestamp, so unlike the wall-clock dev-mode test the L2
# genesis must be re-timed to the (current) L1 genesis and the fork scheduled
# in L2-time, not wall-clock time.
FORK_TS=$((L1_GENESIS_TS + FORK_DELAY_BLOCKS * L2_BLOCK_TIME))
python3 - "$DEV_GENESIS" "$GENESIS" "$L1_GENESIS_TS" "$FORK_TS" "$OWNER" "$TOKEN_L1" "$BRIDGE_L1" "$SEED" <<'EOF'
import json, sys
src, dst, l2_time, fork_ts, owner, token_l1, bridge_l1, seed = sys.argv[1:9]
genesis = json.load(open(src))
genesis["timestamp"] = hex(int(l2_time))
genesis["config"].update({
    "upgrade18Time": int(fork_ts),
    "upgrade18LiquidityControllerOwner": owner,
    "upgrade18CeloTokenL1": token_l1,
    "upgrade18CeloGasBridgeL1": bridge_l1,
    "upgrade18NativeAssetLiquidityAmount": seed,
})
json.dump(genesis, open(dst, "w"))
EOF
echo "Scheduled upgrade18Time=$FORK_TS (L2 genesis + $((FORK_DELAY_BLOCKS * L2_BLOCK_TIME))s)"

# ---------------------------------------------------------------------------
# celo-reth (no --dev: op-node drives it through the engine API)
# ---------------------------------------------------------------------------

"$CELO_RETH" init --chain "$GENESIS" --datadir "$DATADIR" &>"$WORK/celo-reth.log"
"$CELO_RETH" node \
    --chain "$GENESIS" \
    --datadir "$DATADIR" \
    --http \
    --http.port $L2_HTTP_PORT \
    --http.api eth,web3,net,debug \
    --authrpc.addr 127.0.0.1 \
    --authrpc.port $L2_AUTH_PORT \
    --authrpc.jwtsecret "$JWT" \
    --port 30393 \
    --disable-discovery \
    >>"$WORK/celo-reth.log" 2>&1 &
RETH_PID=$!
for _ in {1..60}; do
    if cast block-number --rpc-url "$L2_RPC" &>/dev/null; then break; fi
    kill -0 "$RETH_PID" 2>/dev/null || fail "celo-reth exited early"
    sleep 0.5
done
L2_GENESIS_HASH=$(cast block 0 --rpc-url "$L2_RPC" --field hash)
echo "celo-reth up: L2 genesis $L2_GENESIS_HASH"

# ---------------------------------------------------------------------------
# rollup.json — the shared config, exactly as celo-kona's proof host reads it
# ---------------------------------------------------------------------------

GAS_LIMIT=$(python3 -c "import json;print(int(json.load(open('$GENESIS'))['gasLimit'],16))")
python3 - "$ROLLUP" <<EOF
import json, sys
json.dump({
    "genesis": {
        "l1": {"hash": "$L1_GENESIS_HASH", "number": 0},
        "l2": {"hash": "$L2_GENESIS_HASH", "number": 0},
        "l2_time": $L1_GENESIS_TS,
        "system_config": {
            "batcherAddr": "0x0000000000000000000000000000000000009985",
            "overhead": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "scalar": "0x00000000000000000000000000000000000000000000000000000000000f4240",
            "gasLimit": $GAS_LIMIT,
        },
    },
    "block_time": $L2_BLOCK_TIME,
    "max_sequencer_drift": 600,
    "seq_window_size": 3600,
    "channel_timeout": 300,
    "l1_chain_id": $L1_CHAIN_ID,
    "l2_chain_id": 1337,
    "regolith_time": 0,
    "canyon_time": 0,
    "delta_time": 0,
    "ecotone_time": 0,
    "fjord_time": 0,
    "granite_time": 0,
    "holocene_time": 0,
    "isthmus_time": 0,
    "jovian_time": 0,
    "cel2_time": 0,
    "upgrade18_time": $FORK_TS,
    "batch_inbox_address": "0xff00000000000000000000000000000000042069",
    "deposit_contract_address": "0x0000000000000000000000000000000000009986",
    "l1_system_config_address": "0x0000000000000000000000000000000000009987",
    "chain_op_config": {
        "eip1559Elasticity": 6,
        "eip1559Denominator": 50,
        "eip1559DenominatorCanyon": 250,
    },
}, open("$ROLLUP", "w"), indent=1)
EOF

# ---------------------------------------------------------------------------
# op-node, sequencing
# ---------------------------------------------------------------------------

# Anvil's chain id is not a known L1, so op-node needs an explicit L1 chain
# config (geth params.ChainConfig shape; the blob schedule is mandatory for
# non-OP L1s).
L1_CHAIN_CONFIG="$WORK/l1-chain-config.json"
cat > "$L1_CHAIN_CONFIG" <<EOF
{
  "chainId": $L1_CHAIN_ID,
  "homesteadBlock": 0,
  "eip150Block": 0,
  "eip155Block": 0,
  "eip158Block": 0,
  "byzantiumBlock": 0,
  "constantinopleBlock": 0,
  "petersburgBlock": 0,
  "istanbulBlock": 0,
  "berlinBlock": 0,
  "londonBlock": 0,
  "mergeNetsplitBlock": 0,
  "terminalTotalDifficulty": 0,
  "shanghaiTime": 0,
  "cancunTime": 0,
  "pragueTime": 0,
  "blobSchedule": {
    "cancun": { "target": 3, "max": 6, "baseFeeUpdateFraction": 3338477 },
    "prague": { "target": 6, "max": 9, "baseFeeUpdateFraction": 5007716 }
  }
}
EOF

"$OP_NODE_BIN" \
    --l1="$L1_RPC" \
    --l1.rpckind=standard \
    --l1.trustrpc \
    --l1.beacon.ignore \
    --rollup.l1-chain-config="$L1_CHAIN_CONFIG" \
    --l2="http://127.0.0.1:$L2_AUTH_PORT" \
    --l2.jwt-secret="$JWT" \
    --rollup.config="$ROLLUP" \
    --sequencer.enabled \
    --sequencer.l1-confs=0 \
    --verifier.l1-confs=0 \
    --p2p.disable \
    --rpc.addr=127.0.0.1 \
    --rpc.port=$OP_NODE_RPC_PORT \
    &>"$WORK/op-node.log" &
OP_NODE_PID=$!

# Assertion 1 (implicit): op-node accepted the rollup.json. It exits on a
# config parse error, so surviving to a moving unsafe head covers it.

# ---------------------------------------------------------------------------
# Assertion 2: op-node sequences celo-reth blocks past the boundary
# ---------------------------------------------------------------------------

# Two blocks past the fork: proves sequencing continued after the migration.
TARGET_TS=$((FORK_TS + 2 * L2_BLOCK_TIME))
echo "Waiting for the sequencer to pass the boundary (head ts >= $TARGET_TS)..."
HEAD=0
for _ in {1..90}; do
    kill -0 "$OP_NODE_PID" 2>/dev/null || fail "op-node exited early (rollup.json rejected?)"
    HEAD=$(cast block-number --rpc-url "$L2_RPC" 2>/dev/null || echo 0)
    if ((HEAD > 0)); then
        HEAD_TS=$(cast block "$HEAD" --rpc-url "$L2_RPC" --field timestamp)
        ((HEAD_TS >= TARGET_TS)) && break
    fi
    sleep 1
done
((HEAD > 0)) || fail "op-node never produced an L2 block"
((HEAD_TS >= TARGET_TS)) || fail "sequencer did not pass the boundary (head $HEAD ts $HEAD_TS)"
echo "OK  op-node sequenced to block $HEAD (ts $HEAD_TS) via the engine API"

# ---------------------------------------------------------------------------
# Assertion 3: the migration landed exactly at the boundary block
# ---------------------------------------------------------------------------

# op-node's timestamps are genesis-relative, so the boundary block number is
# exact: the first block with ts >= FORK_TS.
BOUNDARY=$FORK_DELAY_BLOCKS
BOUNDARY_TS=$(cast block $BOUNDARY --rpc-url "$L2_RPC" --field timestamp)
((BOUNDARY_TS == FORK_TS)) || fail "block $BOUNDARY ts $BOUNDARY_TS != scheduled fork ts $FORK_TS"

[[ "$(cast code $GAS_BRIDGE_L2 --block $((BOUNDARY - 1)) --rpc-url "$L2_RPC")" == "0x" ]] ||
    fail "CeloGasBridgeL2 must be codeless in the pre-fork block $((BOUNDARY - 1))"
echo "OK  pre-fork block $((BOUNDARY - 1)): predeploys untouched"

SHELL_CODE=$(python3 -c "import json;print(json.load(open('$ARTIFACT'))['proxyShell']['bytecode'].lower())")
[[ "$(cast code $GAS_BRIDGE_L2 --block $BOUNDARY --rpc-url "$L2_RPC")" == "$SHELL_CODE" ]] ||
    fail "CeloGasBridgeL2 proxy shell not installed at boundary block $BOUNDARY"
[[ "$(cast balance $RESERVE --block $BOUNDARY --rpc-url "$L2_RPC")" == "$SEED" ]] ||
    fail "reserve not seeded at boundary block $BOUNDARY"
[[ "$(cast call $LIQUIDITY_CONTROLLER 'owner()(address)' --rpc-url "$L2_RPC")" == \
   "$(cast to-check-sum-address $OWNER)" ]] || fail "LiquidityController owner mismatch"
echo "OK  boundary block $BOUNDARY (ts $BOUNDARY_TS): migration applied, params resolved"

# ---------------------------------------------------------------------------
# Assertion 4: exactly once, and op-node's own view agrees
# ---------------------------------------------------------------------------

[[ "$(cast balance $RESERVE --rpc-url "$L2_RPC")" == "$SEED" ]] ||
    fail "reserve balance changed after the boundary (double mint?)"
echo "OK  transition applied exactly once ($((HEAD - BOUNDARY)) blocks past the boundary)"

UNSAFE=$(curl -sf -X POST -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"optimism_syncStatus","params":[]}' \
    "http://127.0.0.1:$OP_NODE_RPC_PORT" | jq -r .result.unsafe_l2.number)
((UNSAFE >= BOUNDARY)) || fail "op-node unsafe head ($UNSAFE) never reached the boundary"
echo "OK  op-node syncStatus unsafe head at $UNSAFE (>= boundary $BOUNDARY)"

echo "PASS: op-node drove celo-reth across the Upgrade 18 boundary"
