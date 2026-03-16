#!/bin/bash
#
# E2E test runner for celo-reth.
#
# Starts celo-reth in --dev mode (auto-mining, chain ID 1337), funds the test
# account, then runs the e2e test scripts from op-geth/e2e_test.
#
# Usage:
#   ./run_all_tests.sh [TEST_GLOB]
#
# Environment:
#   OP_GETH_E2E  Path to op-geth/e2e_test (default: ~/op-geth/e2e_test)
#   CELO_RETH    Path to celo-reth binary (default: auto-built from workspace)
#   SKIP_BUILD   Set to 1 to skip cargo build
#
set -eo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
OP_GETH_E2E="${OP_GETH_E2E:-$HOME/op-geth/e2e_test}"
TEST_GLOB="${1:-}"

# ---------------------------------------------------------------------------
# Preflight checks
# ---------------------------------------------------------------------------

if [[ ! -d "$OP_GETH_E2E" ]]; then
    echo "ERROR: op-geth e2e_test directory not found at $OP_GETH_E2E"
    echo "Set OP_GETH_E2E to the correct path."
    exit 1
fi

for cmd in cast forge; do
    if ! command -v "$cmd" &>/dev/null; then
        echo "ERROR: $cmd (foundry) is required but not found in PATH"
        exit 1
    fi
done

# ---------------------------------------------------------------------------
# Build celo-reth
# ---------------------------------------------------------------------------

if [[ "${SKIP_BUILD:-}" != "1" ]]; then
    echo "Building celo-reth..."
    cargo build -p celo-reth --manifest-path "$REPO_ROOT/Cargo.toml"
fi
CELO_RETH="${CELO_RETH:-$REPO_ROOT/target/debug/celo-reth}"

if [[ ! -x "$CELO_RETH" ]]; then
    echo "ERROR: celo-reth binary not found at $CELO_RETH"
    exit 1
fi

# ---------------------------------------------------------------------------
# Start celo-reth
# ---------------------------------------------------------------------------

DATADIR=$(mktemp -d)
CELO_RETH_PID=

cleanup() {
    if [[ -n "$CELO_RETH_PID" ]]; then
        kill "$CELO_RETH_PID" 2>/dev/null || true
        wait "$CELO_RETH_PID" 2>/dev/null || true
    fi
    rm -rf "$DATADIR"
}
trap cleanup EXIT

# Use non-default ports to avoid conflicts with other running nodes.
HTTP_PORT="${HTTP_PORT:-8545}"
AUTH_PORT="${AUTH_PORT:-8651}"
P2P_PORT="${P2P_PORT:-30403}"

# Kill any stale celo-reth instances on our ports to prevent conflicts.
if lsof -ti :"$HTTP_PORT" &>/dev/null; then
    echo "Killing stale process on port $HTTP_PORT..."
    kill $(lsof -ti :"$HTTP_PORT") 2>/dev/null || true
    sleep 1
fi

GENESIS_JSON="$SCRIPT_DIR/celo-dev-genesis.json"

echo "Starting celo-reth in dev mode (datadir=$DATADIR)..."
"$CELO_RETH" node --dev \
    --chain "$GENESIS_JSON" \
    --datadir "$DATADIR" \
    --http \
    --http.port "$HTTP_PORT" \
    --http.api eth,web3,net,admin \
    --authrpc.port "$AUTH_PORT" \
    --port "$P2P_PORT" \
    --discovery.port "$P2P_PORT" \
    &>"$SCRIPT_DIR/celo-reth.log" &
CELO_RETH_PID=$!

# Wait for readiness
export ETH_RPC_URL="http://127.0.0.1:$HTTP_PORT"
echo "Waiting for celo-reth to be ready..."
for _ in {1..60}; do
    if cast block-number &>/dev/null 2>&1; then
        break
    fi
    if ! kill -0 "$CELO_RETH_PID" 2>/dev/null; then
        echo "ERROR: celo-reth process exited unexpectedly."
        echo "--- last 80 lines of celo-reth.log ---"
        tail -80 "$SCRIPT_DIR/celo-reth.log"
        exit 1
    fi
    sleep 0.5
done

if ! cast block-number &>/dev/null 2>&1; then
    echo "ERROR: celo-reth did not become ready within 30 seconds."
    echo "--- last 80 lines of celo-reth.log ---"
    tail -80 "$SCRIPT_DIR/celo-reth.log"
    exit 1
fi
echo "celo-reth is ready (block $(cast block-number))"

# The test account (0x42cf1bbc...) is pre-funded in the custom genesis.
echo "Test account balance: $(cast balance 0x42cf1bbc38BaAA3c4898ce8790e21eD2738c6A4a) wei"

# ---------------------------------------------------------------------------
# Run tests
# ---------------------------------------------------------------------------

cd "$OP_GETH_E2E"

# Override SCRIPT_DIR before sourcing shared.sh, because shared.sh computes
# it from $0 which points to our runner script, not to op-geth/e2e_test.
export SCRIPT_DIR="$OP_GETH_E2E"

# Source shared env (sets ETH_RPC_URL, ACC_PRIVKEY, TOKEN_ADDR, etc.)
# Use set +e temporarily because shared.sh's readlink -f may fail when
# $0 doesn't resolve relative to the current directory.
set +e
source shared.sh
set -e

# Re-export our RPC URL (shared.sh may have overridden it for the local case)
export ETH_RPC_URL="http://127.0.0.1:$HTTP_PORT"

prepare_node

# Pre-flight tx to work around first-tx issues (non-fatal)
cast send --json --private-key "$ACC_PRIVKEY" "$TOKEN_ADDR" \
    'transfer(address to, uint256 value) returns (bool)' \
    0x000000000000000000000000000000000000dEaD 100 >/dev/null 2>&1 || true

failures=0
passed=0
skipped=0

echo ""
echo "========================================="
echo "Running tests (glob: \"$TEST_GLOB\")"
echo "========================================="

for f in test_*"$TEST_GLOB"*.sh; do
    [[ -f "$f" ]] || continue
    echo -e "\n--- $f ---"
    if "./$f"; then
        tput setaf 2 2>/dev/null || true
        echo "PASS $f"
        tput sgr0 2>/dev/null || true
        ((passed++)) || true
    else
        tput setaf 1 2>/dev/null || true
        echo "FAIL $f"
        tput sgr0 2>/dev/null || true
        ((failures++)) || true
    fi
done

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

total=$((passed + failures))
echo ""
echo "========================================="
echo "Results: $passed passed, $failures failed (out of $total)"
echo "========================================="

if [[ $passed -eq 0 ]]; then
    echo "ERROR: No tests passed!"
    exit 1
fi

if [[ $failures -gt 0 ]]; then
    tput setaf 3 2>/dev/null || true
    echo "Some tests failed. This is expected — not all features are implemented yet."
    tput sgr0 2>/dev/null || true
fi

exit 0
