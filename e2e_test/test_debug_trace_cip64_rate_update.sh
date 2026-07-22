#!/bin/bash
set -eo pipefail

source shared.sh
source debug-fee-currency/lib.sh

# Regression test for debug_trace* on blocks with CIP-64 transactions: block
# replay and mid-block call simulation must use the block-start fee-currency
# context (a CIP-64 consensus rule), and replaying multiple CIP-64 transactions
# must not double-store receipt data. See js-tests/debug_trace_rate_update.mjs
# for the three scenarios.
#
# The shared e2e node instamines one tx per block, so this test starts its own node
# with interval mining (--dev.block-time) to land the rate update and the CIP-64
# txs in the same block.

CELO_RETH="${CELO_RETH:-$SCRIPT_DIR/../target/debug/celo-reth}"
if [[ ! -x "$CELO_RETH" ]]; then
    echo "FAIL: celo-reth binary not found at $CELO_RETH"
    exit 1
fi

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

# All ports are OS-assigned (port 0), so this node cannot collide with the
# runner's shared node or any other listener. The HTTP port is read back from
# the node log.
"$CELO_RETH" node --dev \
    --dev.block-time 2s \
    --chain "$GENESIS_JSON" \
    --datadir "$DATADIR" \
    --http \
    --http.port 0 \
    --http.api eth,web3,net,admin,debug \
    --authrpc.port 0 \
    --port 0 \
    --disable-discovery \
    --color never \
    >>"$NODE_LOG" 2>&1 &
NODE_PID=$!

HTTP_PORT=
for _ in {1..60}; do
    if ! kill -0 "$NODE_PID" 2>/dev/null; then
        echo "FAIL: interval-mining celo-reth exited unexpectedly"
        tail -40 "$NODE_LOG"
        exit 1
    fi
    HTTP_PORT=$(grep -m1 'RPC HTTP server started' "$NODE_LOG" | grep -oE '[0-9]+$' || true)
    if [ -n "$HTTP_PORT" ]; then
        break
    fi
    sleep 0.5
done
if [ -z "$HTTP_PORT" ]; then
    echo "FAIL: interval-mining celo-reth did not report its RPC port"
    tail -40 "$NODE_LOG"
    exit 1
fi

export ETH_RPC_URL="http://127.0.0.1:$HTTP_PORT"
for _ in {1..60}; do
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
