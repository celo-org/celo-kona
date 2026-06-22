#!/bin/bash
#
# E2E regression test for the `admin` RPC namespace gate.
#
# celo_reth.rs installs the Celo fee-currency blocklist mutators
# (admin_disableBlocklistFeeCurrencies / admin_enableBlocklistFeeCurrencies /
# admin_unblockFeeCurrency) with reth's
# merge_if_module_configured(RethRpcModule::Admin, ..), so they are exposed ONLY when
# the operator puts `admin` in --http.api. If that ever regressed to the ungated
# merge_configured, these mutators would be reachable by any unauthenticated RPC
# client. This test asserts both arms of the gate against a live node:
#   * admin in --http.api     -> the method is present (dispatched, not -32601)
#   * admin NOT in --http.api  -> the method returns JSON-RPC -32601 (method not found)
#
# The first arm is a positive control: it pins the method name so the negative arm
# cannot pass for the wrong reason (a typo would make the method absent in BOTH arms).
#
# Self-contained: starts its own node(s) on their own ports and does NOT use the
# shared run_all_tests.sh node (which always has `admin` on). Safe to run either
# standalone or via run_all_tests.sh.
set -eo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CELO_RETH="${CELO_RETH:-$REPO_ROOT/target/debug/celo-reth}"
GENESIS_JSON="$SCRIPT_DIR/celo-dev-genesis.json"
LOG="$SCRIPT_DIR/celo-reth.admin-gating.log"

# Own ports, distinct from the shared run_all_tests.sh node (8545 http / 8651 authrpc /
# 30303 p2p) so this test can run alongside it. The p2p listener binds even with
# --disable-discovery, so it needs its own --port too (otherwise it collides on 30303).
HTTP_PORT="${ADMIN_GATING_HTTP_PORT:-18545}"
AUTH_PORT="${ADMIN_GATING_AUTH_PORT:-18661}"
P2P_PORT="${ADMIN_GATING_P2P_PORT:-30399}"
RPC="http://127.0.0.1:$HTTP_PORT"

# A Celo-specific admin method (installed by celo_admin_module, gated by our call).
# Probing a standard reth admin method instead would not catch a regression in our
# gating, since reth installs those through its own machinery.
ADMIN_METHOD="admin_disableBlocklistFeeCurrencies"

if ! command -v cast &>/dev/null; then
    echo "ERROR: cast (foundry) is required but not found in PATH"
    exit 1
fi

if [[ ! -x "$CELO_RETH" ]]; then
    echo "Building celo-reth..."
    cargo build -p celo-reth --manifest-path "$REPO_ROOT/Cargo.toml"
fi

NODE_PID=
DATADIR=
cleanup() {
    if [[ -n "$NODE_PID" ]]; then
        kill "$NODE_PID" 2>/dev/null || true
        wait "$NODE_PID" 2>/dev/null || true
    fi
    [[ -n "$DATADIR" ]] && rm -rf "$DATADIR"
    return 0
}
trap cleanup EXIT

stop_node() {
    if [[ -n "$NODE_PID" ]]; then
        kill "$NODE_PID" 2>/dev/null || true
        wait "$NODE_PID" 2>/dev/null || true
    fi
    NODE_PID=
    [[ -n "$DATADIR" ]] && rm -rf "$DATADIR"
    DATADIR=
}

# start_node <http.api csv>: boots a fresh --dev node and waits for RPC readiness.
start_node() {
    local api="$1" p
    DATADIR=$(mktemp -d)

    # Clear any stale listener on our ports (never touches the shared node's 8545/8651).
    for p in "$HTTP_PORT" "$AUTH_PORT"; do
        if lsof -ti :"$p" &>/dev/null; then
            kill "$(lsof -ti :"$p")" 2>/dev/null || true
            sleep 1
        fi
    done

    if ! "$CELO_RETH" init --chain "$GENESIS_JSON" --datadir "$DATADIR" &>"$LOG"; then
        echo "ERROR: celo-reth init failed"
        tail -40 "$LOG"
        exit 1
    fi

    "$CELO_RETH" node --dev \
        --chain "$GENESIS_JSON" \
        --datadir "$DATADIR" \
        --http --http.port "$HTTP_PORT" --http.api "$api" \
        --authrpc.port "$AUTH_PORT" \
        --port "$P2P_PORT" \
        --disable-discovery \
        >>"$LOG" 2>&1 &
    NODE_PID=$!

    local _
    for _ in {1..60}; do
        if cast chain-id -r "$RPC" &>/dev/null; then
            return 0
        fi
        if ! kill -0 "$NODE_PID" 2>/dev/null; then
            echo "ERROR: node exited during startup (--http.api $api)"
            tail -80 "$LOG"
            exit 1
        fi
        sleep 0.5
    done
    echo "ERROR: node did not become ready (--http.api $api)"
    tail -80 "$LOG"
    exit 1
}

# method_present <method>: returns 0 if the RPC method is dispatched, 1 if the server
# answers -32601 (method not found). No params are sent: method-not-found is decided
# before param parsing, and a present method merely answers with an invalid-params
# error instead (still "present").
method_present() {
    local method="$1" out rc
    set +e
    out=$(cast rpc "$method" -r "$RPC" 2>&1)
    rc=$?
    set -e
    if [[ $rc -eq 0 ]]; then
        return 0
    fi
    if grep -qiE '(-32601|method not found)' <<<"$out"; then
        return 1
    fi
    # Any other error (e.g. -32602 invalid params) still means the method exists.
    return 0
}

# --- Arm 1 (positive control): admin ON -> method must be present. ---
echo "[1/2] admin enabled:  expecting $ADMIN_METHOD to be present"
start_node "eth,web3,net,admin"
if ! method_present "$ADMIN_METHOD"; then
    echo "FAIL: $ADMIN_METHOD is missing even though 'admin' is in --http.api"
    exit 1
fi
echo "      OK: present when 'admin' is configured"
stop_node

# --- Arm 2 (the regression guard): admin OFF -> method must be gated (-32601). ---
echo "[2/2] admin disabled: expecting $ADMIN_METHOD to be gated (-32601)"
start_node "eth,web3,net"
# Setup sanity: the standard reth admin_nodeInfo must also be gone; if it is present,
# 'admin' leaked into the config and the negative result below would be meaningless.
if method_present "admin_nodeInfo"; then
    echo "FAIL (setup): admin_nodeInfo is present -> 'admin' namespace unexpectedly enabled; test misconfigured"
    exit 1
fi
if method_present "$ADMIN_METHOD"; then
    echo "FAIL: $ADMIN_METHOD is reachable without 'admin' in --http.api (gate regressed to merge_configured?)"
    exit 1
fi
echo "      OK: gated (method not found) when 'admin' is absent"
stop_node

echo "PASS: Celo admin RPC methods are exposed only when the 'admin' namespace is configured"
