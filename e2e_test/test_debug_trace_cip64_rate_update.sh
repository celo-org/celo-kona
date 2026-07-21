#!/bin/bash
set -eo pipefail

source shared.sh
source debug-fee-currency/lib.sh

# Regression test for debug_trace* on blocks with CIP-64 transactions: a mid-block
# rate update must not affect the replay (block-start rates are a CIP-64 consensus
# rule), and replaying multiple CIP-64 transactions must not double-store receipt
# data. See js-tests/debug_trace_rate_update.mjs for the scenario details.
#
# The shared e2e node instamines one tx per block, so this test starts its own node
# with interval mining (--dev.block-time) to land the rate update and the CIP-64
# txs in the same block.

CELO_RETH="${CELO_RETH:-$SCRIPT_DIR/../target/debug/celo-reth}"
if [[ ! -x "$CELO_RETH" ]]; then
    echo "FAIL: celo-reth binary not found at $CELO_RETH"
    exit 1
fi

# Pick a free port for our own node rather than clearing a fixed one: a fixed
# port could hold the runner's shared node (its HTTP_PORT is overridable), and
# a foreign listener must not be killed nor answer our readiness probe.
SHARED_PORT="${ETH_RPC_URL##*:}"
HTTP_PORT=
for candidate in 8547 8548 8549 8550; do
    if [ "$candidate" != "$SHARED_PORT" ] && ! lsof -ti :"$candidate" &>/dev/null; then
        HTTP_PORT=$candidate
        break
    fi
done
if [ -z "$HTTP_PORT" ]; then
    echo "FAIL: no free HTTP port for the interval-mining node"
    exit 1
fi
AUTH_PORT=$((HTTP_PORT + 200))
P2P_PORT=$((HTTP_PORT + 22000))
DATADIR=$(mktemp -d)
NODE_LOG="$DATADIR/celo-reth.log"
NODE_PID=

cleanup() {
    if [[ -n "$NODE_PID" ]]; then
        kill "$NODE_PID" 2>/dev/null || true
        wait "$NODE_PID" 2>/dev/null || true
    fi
    rm -rf "$DATADIR"
}
trap cleanup EXIT

GENESIS_JSON="$SCRIPT_DIR/celo-dev-genesis.json"

"$CELO_RETH" init --chain "$GENESIS_JSON" --datadir "$DATADIR" &>"$NODE_LOG"

"$CELO_RETH" node --dev \
    --dev.block-time 2s \
    --chain "$GENESIS_JSON" \
    --datadir "$DATADIR" \
    --http \
    --http.port "$HTTP_PORT" \
    --http.api eth,web3,net,admin,debug \
    --authrpc.port "$AUTH_PORT" \
    --port "$P2P_PORT" \
    --disable-discovery \
    >>"$NODE_LOG" 2>&1 &
NODE_PID=$!

export ETH_RPC_URL="http://127.0.0.1:$HTTP_PORT"
for _ in {1..60}; do
    if ! kill -0 "$NODE_PID" 2>/dev/null; then
        echo "FAIL: interval-mining celo-reth exited unexpectedly"
        tail -40 "$NODE_LOG"
        exit 1
    fi
    if cast block-number &>/dev/null; then
        break
    fi
    sleep 0.5
done
if ! cast block-number &>/dev/null; then
    echo "FAIL: interval-mining celo-reth did not become ready"
    tail -40 "$NODE_LOG"
    exit 1
fi

prepare_node

# Deploys the fee currency and wires it to ORACLE3 at a 2:1 rate.
fee_currency=$(deploy_fee_currency false false false)
echo "fee currency: $fee_currency"

result=$(js-tests/debug_trace_rate_update.mjs "$fee_currency" "$ORACLE3" "$FEE_CURRENCY_DIRECTORY_ADDR" || true)
echo "$result"
if [ "$(echo "$result" | jq .success)" != "true" ]; then
    echo "FAIL: debug_trace* returned wrong results for a block with a mid-block rate update"
    exit 1
fi

echo "PASS: debug_trace* uses block-start fee-currency rates"
