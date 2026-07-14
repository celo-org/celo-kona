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
#      transition applies exactly once;
#   5. with op-batcher posting calldata batches to L1, op-node *derives* the
#      boundary from L1 data — the safe head crosses the migration;
#   6. celo-executor (the proof-path executor) replays the boundary blocks to
#      the same block hashes, configured from the proof-side rollup.json —
#      the variant that carries the four param overrides op-node's does not;
#   7. the full proof pipeline: celo-host runs the FPVM client program
#      (natively) to prove the boundary block from L1 batch data — boot-info
#      carriage of the Upgrade 18 settings through the preimage oracle,
#      derivation inside the client, and the witness-fed oracle executor.
#
# Topology: anvil as a bare L1 (no OP contracts — with zero deposits, the
# deposit-contract and system-config addresses are never read), celo-reth as
# the L2 EL, op-node as the sequencing CL, op-batcher for the derivation leg.
#
# Requirements:
#   anvil + cast (foundry), jq, python3
#   celo-reth           target/debug/celo-reth (or $CELO_RETH)
#   execution-verifier  target/debug/execution-verifier (or $EXECUTION_VERIFIER)
#   celo-host           target/debug/celo-host (or $CELO_HOST)
#   op-node, op-batcher $OP_NODE_BIN / $OP_BATCHER_BIN, or built from
#                       $CELO_OPTIMISM_DIR (branch with Upgrade 18 fork
#                       plumbing; needs go on PATH)
set -eo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
CELO_RETH="${CELO_RETH:-$REPO_ROOT/target/debug/celo-reth}"
ARTIFACT="$REPO_ROOT/crates/alloy-celo-evm/res/predeploys.json"
DEV_GENESIS="$REPO_ROOT/e2e_test/celo-dev-genesis.json"

L1_PORT=8945
L2_HTTP_PORT=8947
L2_WS_PORT=8948
L2_AUTH_PORT=8953
OP_NODE_RPC_PORT=9945
BATCHER_RPC_PORT=9947
BEACON_STUB_PORT=9958
L1_RPC="http://127.0.0.1:$L1_PORT"
L2_RPC="http://127.0.0.1:$L2_HTTP_PORT"
L2_WS="ws://127.0.0.1:$L2_WS_PORT"

L1_CHAIN_ID=900
L2_BLOCK_TIME=2

# anvil's default account 0: funds the batcher. Must match the rollup.json
# genesis system_config batcherAddr, or derivation rejects the batches.
BATCHER_ADDR=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266
BATCHER_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
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
BATCHER_PID=
BEACON_PID=

cleanup() {
    for pid in "$BEACON_PID" "$BATCHER_PID" "$OP_NODE_PID" "$RETH_PID" "$ANVIL_PID"; do
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
    for log in op-node op-batcher celo-reth; do
        [[ -s "$WORK/$log.log" ]] || continue
        echo "--- $log log tail ---"; tail -20 "$WORK/$log.log"
    done
    exit 1
}

for port in $L1_PORT $L2_HTTP_PORT $L2_WS_PORT $L2_AUTH_PORT $OP_NODE_RPC_PORT $BATCHER_RPC_PORT $BEACON_STUB_PORT; do
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

if [[ -z "$OP_BATCHER_BIN" ]]; then
    [[ -n "$CELO_OPTIMISM_DIR" ]] ||
        fail "set OP_BATCHER_BIN to an op-batcher, or CELO_OPTIMISM_DIR to build one"
    echo "Building op-batcher from $CELO_OPTIMISM_DIR..."
    OP_BATCHER_BIN="$WORK/op-batcher"
    (cd "$CELO_OPTIMISM_DIR" && go build -o "$OP_BATCHER_BIN" ./op-batcher/cmd)
fi
[[ -x "$OP_BATCHER_BIN" ]] || fail "op-batcher binary $OP_BATCHER_BIN not executable"

EXECUTION_VERIFIER="${EXECUTION_VERIFIER:-$REPO_ROOT/target/debug/execution-verifier}"
[[ -x "$EXECUTION_VERIFIER" ]] ||
    fail "execution-verifier not found at $EXECUTION_VERIFIER (cargo build -p execution-verifier)"

CELO_HOST="${CELO_HOST:-$REPO_ROOT/target/debug/celo-host}"
[[ -x "$CELO_HOST" ]] || fail "celo-host not found at $CELO_HOST (cargo build -p celo-host)"

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

# --rpc.eth-proof-window: execution-verifier's witness preimage source
# supplements debug_executionWitness with a historical eth_getProof (see
# celo-executor's test_utils), which reth only serves within this window.
"$CELO_RETH" init --chain "$GENESIS" --datadir "$DATADIR" &>"$WORK/celo-reth.log"
"$CELO_RETH" node \
    --chain "$GENESIS" \
    --datadir "$DATADIR" \
    --rpc.eth-proof-window 100000 \
    --http \
    --http.port $L2_HTTP_PORT \
    --http.api eth,web3,net,debug \
    --ws \
    --ws.port $L2_WS_PORT \
    --ws.api eth,web3,net,debug \
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
            "batcherAddr": "$BATCHER_ADDR",
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
# op-batcher: posts calldata batches to L1 so op-node *derives* a safe head
# ---------------------------------------------------------------------------

"$OP_BATCHER_BIN" \
    --l1-eth-rpc="$L1_RPC" \
    --l2-eth-rpc="$L2_RPC" \
    --rollup-rpc="http://127.0.0.1:$OP_NODE_RPC_PORT" \
    --private-key="$BATCHER_KEY" \
    --data-availability-type=calldata \
    --max-channel-duration=1 \
    --poll-interval=1s \
    --num-confirmations=1 \
    --throttle.unsafe-da-bytes-lower-threshold=0 \
    --rpc.addr=127.0.0.1 \
    --rpc.port=$BATCHER_RPC_PORT \
    &>"$WORK/op-batcher.log" &
BATCHER_PID=$!

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

# ---------------------------------------------------------------------------
# Assertion 5: the safe head crosses the boundary — op-node *derives* the
# migration block from L1 batch data, not just sequences it
# ---------------------------------------------------------------------------

echo "Waiting for the derived safe head to pass the boundary..."
SAFE=0
for _ in {1..90}; do
    kill -0 "$BATCHER_PID" 2>/dev/null || fail "op-batcher exited early"
    SAFE=$(curl -sf -X POST -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","id":1,"method":"optimism_syncStatus","params":[]}' \
        "http://127.0.0.1:$OP_NODE_RPC_PORT" | jq -r .result.safe_l2.number)
    ((SAFE >= BOUNDARY)) && break
    sleep 1
done
((SAFE >= BOUNDARY)) ||
    fail "derived safe head ($SAFE) never crossed the boundary ($BOUNDARY)"
echo "OK  derivation: safe head at $SAFE (>= boundary $BOUNDARY)"

# ---------------------------------------------------------------------------
# Assertion 6: proof-path executor parity — replay the boundary blocks through
# celo-executor, configured from the proof-side rollup.json (which, unlike
# op-node's, carries the four param overrides)
# ---------------------------------------------------------------------------

ROLLUP_PROOF="$WORK/rollup-proof.json"
python3 - "$ROLLUP" "$ROLLUP_PROOF" "$OWNER" "$TOKEN_L1" "$BRIDGE_L1" "$SEED" <<'EOF'
import json, sys
src, dst, owner, token_l1, bridge_l1, seed = sys.argv[1:7]
config = json.load(open(src))
config.update({
    "upgrade18_liquidity_controller_owner": owner,
    "upgrade18_celo_token_l1": token_l1,
    "upgrade18_celo_gas_bridge_l1": bridge_l1,
    "upgrade18_native_asset_liquidity_amount": hex(int(seed)),
})
json.dump(config, open(dst, "w"))
EOF

# From the boundary onward only: sealing an Isthmus block statelessly reads the
# L2ToL1MessagePasser account for the withdrawals root, and on this dev genesis
# that account only exists once Upgrade 18 installs it — real chains carry it
# pre-fork, so pre-fork replay is a dev-genesis artifact, not migration surface.
"$EXECUTION_VERIFIER" \
    --l2-rpc "$L2_WS" \
    --rollup-config "$ROLLUP_PROOF" \
    --start-block $BOUNDARY \
    --end-block $((BOUNDARY + 2)) \
    &>"$WORK/execution-verifier.log" ||
    { tail -20 "$WORK/execution-verifier.log"; fail "celo-executor replay of the boundary blocks failed"; }
echo "OK  celo-executor replayed blocks $BOUNDARY..$((BOUNDARY + 2)) (proof-side rollup.json)"

# Negative check: with op-node's rollup.json — no param overrides, and chain
# 1337 is unknown to the artifact — the boundary block must fail to replay
# (Upgrade18ParamMissing). Proves the param channel is load-bearing, i.e. the
# positive run above verified something real.
if "$EXECUTION_VERIFIER" \
    --l2-rpc "$L2_WS" \
    --rollup-config "$ROLLUP" \
    --start-block $BOUNDARY \
    --end-block $BOUNDARY \
    &>"$WORK/execution-verifier-neg.log"; then
    fail "boundary replay without the param overrides must fail, but passed"
fi
echo "OK  boundary replay without the param overrides fails as it must"

# ---------------------------------------------------------------------------
# Assertion 7: full proof pipeline — celo-host runs the FPVM client program
# (natively) to prove the boundary block from L1 batch data
# ---------------------------------------------------------------------------

# The layers assertion 6 does not cover: boot-info carriage of the Upgrade 18
# settings through the preimage oracle, derivation inside the client program,
# and the witness-fed oracle executor.

# kona's blob provider insists on a beacon endpoint at startup even though
# calldata batches never touch it afterwards: serve the two static responses
# it reads at init.
python3 - $BEACON_STUB_PORT "$L1_GENESIS_TS" <<'EOF' &>"$WORK/beacon-stub.log" &
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

port, genesis_time = int(sys.argv[1]), sys.argv[2]

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/eth/v1/beacon/genesis"):
            body = {"data": {"genesis_time": genesis_time}}
        elif self.path.startswith("/eth/v1/config/spec"):
            body = {"data": {"SECONDS_PER_SLOT": "2"}}
        else:
            self.send_error(404)
            return
        data = json.dumps(body).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args):
        pass

HTTPServer(("127.0.0.1", port), Handler).serve_forever()
EOF
BEACON_PID=$!

output_at() {
    curl -sf -X POST -H 'Content-Type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"optimism_outputAtBlock\",\"params\":[\"$(printf '0x%x' "$1")\"]}" \
        "http://127.0.0.1:$OP_NODE_RPC_PORT"
}

AGREED=$(output_at $((BOUNDARY - 1)))
AGREED_ROOT=$(jq -r .result.outputRoot <<<"$AGREED")
AGREED_HEAD=$(jq -r .result.blockRef.hash <<<"$AGREED")
CLAIMED_ROOT=$(output_at $BOUNDARY | jq -r .result.outputRoot)
# Any L1 head works as long as the batches covering the claim are under it;
# the safe-head crossing in assertion 5 guarantees that for the current tip.
L1_HEAD=$(cast block latest --rpc-url "$L1_RPC" --field hash)

prove_boundary() { # $1: rollup config for the proof, $2: log file
    "$CELO_HOST" single \
        --native \
        --l1-head "$L1_HEAD" \
        --agreed-l2-head-hash "$AGREED_HEAD" \
        --agreed-l2-output-root "$AGREED_ROOT" \
        --claimed-l2-output-root "$CLAIMED_ROOT" \
        --claimed-l2-block-number "$BOUNDARY" \
        --l1-node-address "$L1_RPC" \
        --l1-beacon-address "http://127.0.0.1:$BEACON_STUB_PORT" \
        --l2-node-address "$L2_RPC" \
        --rollup-config-path "$1" \
        --l1-config-path "$L1_CHAIN_CONFIG" \
        &>"$2"
}

prove_boundary "$ROLLUP_PROOF" "$WORK/celo-host.log" ||
    { tail -30 "$WORK/celo-host.log"; fail "celo-host failed to prove the boundary block"; }
echo "OK  proof pipeline: celo-host proved boundary block $BOUNDARY from L1 data"

# Negative check: with op-node's param-less rollup.json the client program must
# halt at the boundary on the unresolved params, not silently skip the
# migration and disagree about the output root later.
if prove_boundary "$ROLLUP" "$WORK/celo-host-neg.log"; then
    fail "boundary proof without the param overrides must fail, but passed"
fi
grep -q "artifact param .* is unresolved" "$WORK/celo-host-neg.log" ||
    { tail -20 "$WORK/celo-host-neg.log"; fail "negative proof run failed for an unexpected reason"; }
echo "OK  boundary proof without the param overrides halts on the unresolved params"

echo "PASS: op-node drove celo-reth across the Upgrade 18 boundary"
