#!/bin/bash
#
# Regenerates the two checked-in Upgrade 18 (CGT v2) executor fixtures under
# `crates/kona/executor/testdata/`:
#
#   devnet-upgrade18-boundary_block-2.tar.gz       the activation block
#   devnet-upgrade18-post-boundary_block-3.tar.gz  the block after it
#
# They pin the CGT v2 irregular state transition in the *stateless* (fault-proof)
# executor: the boundary block's state writes must be reproducible from nothing but
# an MPT witness, and the block after it must not re-apply the transition (the
# completion-marker read comes from the witness too).
#
# Recipe: start a celo-reth dev node whose genesis schedules `upgrade18Time` a few
# seconds out (with the four artifact param overrides, since chain 1337 is unknown
# to the artifact), mine one pre-fork block, cross the boundary, mine one more, then
# point `examples/execution-fixture` at blocks 2 and 3. The generator collects the
# trie witness via `debug_executionWitness` and asserts the statelessly rebuilt
# header matches the node's.
#
# Run this whenever `crates/alloy-celo-evm/res/predeploys.json` changes (e.g. the
# real `celoGasBridgeL1` and reserve seed land), or if the activation trigger
# decision (celo-blockchain-planning#1407) renames the rollup config keys.
#
#   ./e2e_test/generate_upgrade18_fixtures.sh
#
set -eo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CELO_RETH="${CELO_RETH:-$REPO_ROOT/target/debug/celo-reth}"
FIXTURE_GEN="${FIXTURE_GEN:-$REPO_ROOT/target/release/execution-fixture}"
TESTDATA="$REPO_ROOT/crates/kona/executor/testdata"

HTTP_PORT=8549
AUTH_PORT=8655
RPC="http://127.0.0.1:$HTTP_PORT"

# Dev account pre-funded in celo-dev-genesis.json.
ACC_PRIVKEY=0x2771aff413cac48d9f8c114fabddd9195a2129f3c2c436caa07e27bb7f58ead5

# Artifact param overrides. These are the same placeholder values the migration e2e
# test uses; they are what the fixtures pin, so both must move together.
OWNER=0x00000000000000000000000000000000000000aa
TOKEN_L1=0x00000000000000000000000000000000000000bb
BRIDGE_L1=0x00000000000000000000000000000000000000cc
SEED=1000000000000000000000000 # 1M CELO in wei

GAS_BRIDGE_L2=0x4200000000000000000000000000000000001023

# The dev node mines one block per transaction, so these are deterministic: block 1 is
# the pre-fork block, block 2 crosses the boundary, block 3 follows it.
BOUNDARY_BLOCK=2
POST_BOUNDARY_BLOCK=3

TMP="${TMPDIR:-/tmp}"
DATADIR=$(mktemp -d "$TMP/upgrade18-datadir.XXXXXX")
GENESIS=$(mktemp "$TMP/upgrade18-genesis.XXXXXX.json")
ROLLUP_CONFIG=$(mktemp "$TMP/upgrade18-rollup.XXXXXX.json")
NODE_LOG="$SCRIPT_DIR/celo-reth-upgrade18-fixtures.log"
NODE_PID=

cleanup() {
    if [[ -n "$NODE_PID" ]]; then
        kill "$NODE_PID" 2>/dev/null || true
        wait "$NODE_PID" 2>/dev/null || true
    fi
    rm -rf "$DATADIR" "$GENESIS" "$ROLLUP_CONFIG"
}
trap cleanup EXIT

fail() { echo "FAIL: $1"; exit 1; }

[[ -x "$CELO_RETH" ]] || fail "celo-reth not found at $CELO_RETH (cargo build -p celo-reth)"
[[ -x "$FIXTURE_GEN" ]] ||
    fail "execution-fixture not found at $FIXTURE_GEN (cargo build --release -p execution-fixture)"

if lsof -ti :"$HTTP_PORT" &>/dev/null; then
    echo "Killing stale process on port $HTTP_PORT..."
    kill "$(lsof -ti :"$HTTP_PORT")" 2>/dev/null || true
    sleep 1
fi

# ---------------------------------------------------------------------------
# Genesis: schedule the fork shortly in the future, with param overrides
# ---------------------------------------------------------------------------

FORK_TS=$(($(date +%s) + 8))
python3 - "$SCRIPT_DIR/celo-dev-genesis.json" "$GENESIS" "$FORK_TS" "$OWNER" "$TOKEN_L1" "$BRIDGE_L1" "$SEED" <<'EOF'
import json, sys
src, dst, fork_ts, owner, token_l1, bridge_l1, seed = sys.argv[1:8]
genesis = json.load(open(src))
genesis["config"].update({
    "upgrade18Time": int(fork_ts),
    "upgrade18LiquidityControllerOwner": owner,
    "upgrade18CeloTokenL1": token_l1,
    "upgrade18CeloGasBridgeL1": bridge_l1,
    "upgrade18NativeAssetLiquidityAmount": seed,
})
json.dump(genesis, open(dst, "w"))
EOF
echo "Scheduled upgrade18Time=$FORK_TS ($((FORK_TS - $(date +%s)))s from now)"

# ---------------------------------------------------------------------------
# Start the node. `debug` is required: the fixture generator pulls the trie
# witness from `debug_executionWitness` and raw txs from `debug_getRawTransaction`.
#
# `--rpc.eth-proof-window` is required too: the generator supplements the witness with a
# historical `eth_getProof` for the `L2ToL1MessagePasser` account, which the witness does not
# cover on its own (see `fetch_message_passer_account_proof`). reth's default window of 0 serves
# proofs only at the tip, and the proof is taken at the target block's *parent*.
# ---------------------------------------------------------------------------

"$CELO_RETH" init --chain "$GENESIS" --datadir "$DATADIR" &>"$NODE_LOG"
"$CELO_RETH" node --dev \
    --chain "$GENESIS" \
    --datadir "$DATADIR" \
    --http \
    --http.port "$HTTP_PORT" \
    --http.api eth,web3,net,debug \
    --authrpc.port "$AUTH_PORT" \
    --rpc.eth-proof-window 1000 \
    --disable-discovery \
    >>"$NODE_LOG" 2>&1 &
NODE_PID=$!

echo "Waiting for celo-reth to be ready..."
for _ in {1..60}; do
    if cast block-number --rpc-url "$RPC" &>/dev/null; then break; fi
    if ! kill -0 "$NODE_PID" 2>/dev/null; then
        echo "ERROR: node exited early"; tail -40 "$NODE_LOG"; exit 1
    fi
    sleep 0.5
done
cast block-number --rpc-url "$RPC" >/dev/null ||
    { echo "ERROR: node not ready"; tail -40 "$NODE_LOG"; exit 1; }

# Mines exactly one block (dev mode seals per transaction); prints its number.
mine_block() {
    cast send --rpc-url "$RPC" --private-key "$ACC_PRIVKEY" --json \
        0x000000000000000000000000000000000000dEaD --value 1 |
        python3 -c 'import json,sys; print(int(json.load(sys.stdin)["blockNumber"], 0))'
}

# ---------------------------------------------------------------------------
# Mine across the boundary
# ---------------------------------------------------------------------------

PRE=$(mine_block)
[[ "$PRE" == "1" ]] || fail "expected the pre-fork block to be #1, got #$PRE"
PRE_TS=$(cast block "$PRE" --rpc-url "$RPC" --field timestamp)
((PRE_TS < FORK_TS)) || fail "block #1 (ts $PRE_TS) already crossed the fork (ts $FORK_TS)"
[[ "$(cast code $GAS_BRIDGE_L2 --rpc-url "$RPC")" == "0x" ]] ||
    fail "predeploys must be untouched before the fork"

NOW=$(date +%s)
if ((NOW <= FORK_TS)); then sleep $((FORK_TS - NOW + 1)); fi

BOUNDARY=$(mine_block)
[[ "$BOUNDARY" == "$BOUNDARY_BLOCK" ]] ||
    fail "expected the boundary block to be #$BOUNDARY_BLOCK, got #$BOUNDARY"
[[ "$(cast code $GAS_BRIDGE_L2 --rpc-url "$RPC")" != "0x" ]] ||
    fail "the transition did not apply at block #$BOUNDARY"
echo "OK  block #$BOUNDARY applied the CGT v2 transition"

POST=$(mine_block)
[[ "$POST" == "$POST_BOUNDARY_BLOCK" ]] ||
    fail "expected the post-boundary block to be #$POST_BOUNDARY_BLOCK, got #$POST"

# ---------------------------------------------------------------------------
# The dev chain's rollup.json. `celo-registry` only knows the production chains,
# so the generator needs this passed explicitly — and it is the channel through
# which `upgrade18_time` + the four param overrides reach the proof-path executor.
# The `upgrade18_*` keys are the snake_case twins of the genesis `config` keys above.
# ---------------------------------------------------------------------------

GENESIS_HASH=$(cast block 0 --rpc-url "$RPC" --field hash)
GENESIS_TS=$(cast block 0 --rpc-url "$RPC" --field timestamp)
python3 - "$ROLLUP_CONFIG" "$GENESIS_HASH" "$GENESIS_TS" "$FORK_TS" "$OWNER" "$TOKEN_L1" "$BRIDGE_L1" "$SEED" <<'EOF'
import json, sys
dst, genesis_hash, genesis_ts, fork_ts, owner, token_l1, bridge_l1, seed = sys.argv[1:9]
zero = "0x" + "00" * 20
json.dump({
    "genesis": {
        "l1": {"hash": "0x" + "00" * 32, "number": 0},
        "l2": {"hash": genesis_hash, "number": 0},
        "l2_time": int(genesis_ts),
        "system_config": {
            "batcherAddr": zero,
            "overhead": "0x" + "00" * 32,
            "scalar": "0x" + "00" * 32,
            "gasLimit": 30000000,
        },
    },
    "block_time": 1,
    "max_sequencer_drift": 600,
    "seq_window_size": 3600,
    "channel_timeout": 300,
    "l1_chain_id": 900,
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
    "batch_inbox_address": zero,
    "deposit_contract_address": zero,
    "l1_system_config_address": zero,
    "chain_op_config": {
        "eip1559Elasticity": 6,
        "eip1559Denominator": 50,
        "eip1559DenominatorCanyon": 250,
    },
    "upgrade18_time": int(fork_ts),
    "upgrade18_liquidity_controller_owner": owner,
    "upgrade18_celo_token_l1": token_l1,
    "upgrade18_celo_gas_bridge_l1": bridge_l1,
    "upgrade18_native_asset_liquidity_amount": seed,
}, open(dst, "w"), indent=2)
EOF

# ---------------------------------------------------------------------------
# Generate. The creator asserts the statelessly produced header matches the node's,
# so a green run is already a parity check between celo-reth and celo-executor.
# ---------------------------------------------------------------------------

generate() {
    local block=$1 name=$2
    "$FIXTURE_GEN" \
        --l2-rpc "$RPC" \
        --block-number "$block" \
        --rollup-config "$ROLLUP_CONFIG" \
        --preimage-source witness \
        --output-dir "$TESTDATA"
    mv "$TESTDATA/block-$block.tar.gz" "$TESTDATA/$name"
    echo "OK  wrote testdata/$name"
}

generate "$BOUNDARY_BLOCK" "devnet-upgrade18-boundary_block-$BOUNDARY_BLOCK.tar.gz"
generate "$POST_BOUNDARY_BLOCK" "devnet-upgrade18-post-boundary_block-$POST_BOUNDARY_BLOCK.tar.gz"

echo "PASS: regenerated the Upgrade 18 executor fixtures"
