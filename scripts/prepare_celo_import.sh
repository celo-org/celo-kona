#!/usr/bin/env bash
# Prepare and run the Celo L1 state import into celo-reth.
# See CELO_MIGRATION.md for details.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CELO_RETH="${CELO_RETH:-$REPO_ROOT/target/release/celo-reth}"
DUMP_URL="https://storage.googleapis.com/cel2-rollup-files/celo/l1-final-state.json.zst"

if [ ! -x "$CELO_RETH" ]; then
    echo "celo-reth binary not found at $CELO_RETH" >&2
    echo "Build it with: cargo build --release -p celo-reth" >&2
    echo "Or set CELO_RETH=<path> to point at an existing binary." >&2
    exit 1
fi

if [ $# -ne 2 ]; then
    echo "Usage: $0 <workdir> <datadir>" >&2
    exit 1
fi

workdir="$1"
datadir="$2"

mkdir -p "$workdir"
dump_zst="$workdir/l1-final-state.json.zst"
dump="$workdir/l1-final-state.json"
dump_allocs="$dump.with-allocs.jsonl"

# Step 0: Download and decompress
if [ ! -f "$dump" ]; then
    if [ ! -f "$dump_zst" ]; then
        echo "==> Downloading L1 state dump..."
        curl -o "$dump_zst" "$DUMP_URL"
    fi
    echo "==> Decompressing..."
    zstd -d "$dump_zst"
else
    echo "==> L1 state dump already exists: $dump"
fi

# Step 1: Append L2 allocs and update state root
if [ ! -f "$dump_allocs" ]; then
    echo "==> Appending L2 allocs..."
    python3 "$SCRIPT_DIR/append_l2_allocs.py" "$dump"
else
    echo "==> Allocs file already exists: $dump_allocs"
fi

# Step 2: Initialize celo-reth
echo "==> Initializing celo-reth (ulimit -n 10240)..."
ulimit -n 10240
"$CELO_RETH" import-celo-state \
    --datadir="$datadir" \
    "$dump_allocs"

echo "==> Done."
