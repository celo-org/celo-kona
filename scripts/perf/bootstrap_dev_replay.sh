#!/usr/bin/env bash
#
# End-to-end validation of the engine-replay driver against a locally built dev chain.
#
# This is the cheapest way to prove every moving part of the rig works — JWT transport, engine
# method-version selection, attribute reconstruction from extraData, the archive/unwind ordering,
# the block-hash gate, the timings writer — with no snapshot, no network and no Linux-only load
# shaping. It takes about a minute and runs on macOS.
#
# What it does NOT do: reproduce the sequencer's state-root tail. The dev chain's trie is a few
# hundred accounts, so every read is a cache hit. That is what components 2-4 of
# sequencer-perf-test-rig-plan.md are for. This script's job is to make the driver trustworthy
# before it is pointed at a real snapshot.
#
# Do not read a tail out of this script's output. At the honest default pacing the dev chain is flat
# — p50 ~790 us, max ~1.2 ms over 30 blocks — and any tail you see at PACE_MS=0 is the driver
# outrunning MDBX persistence, not a state-root cost. See DEFAULT_PACE_MS in
# bin/engine-replay/src/replay.rs for the ablation that establishes this.
#
# Usage:
#   scripts/perf/bootstrap_dev_replay.sh [BLOCKS]
#
# Environment:
#   CELO_RETH       Path to the celo-reth binary (default: target/debug/celo-reth)
#   ENGINE_REPLAY   Path to the engine-replay binary (default: target/debug/engine-replay)
#   GENESIS_FILE    Genesis to use (default: e2e_test/celo-dev-genesis.json, 31 accounts). Point
#                   this at the output of scripts/perf/make_state.py to get a trie with realistic
#                   depth — the stock dev trie is ~1.5 levels and every measurement on it is a
#                   floor value.
#   TXS             Transactions to submit, each to a fresh uniformly spread address (default 10).
#                   New leaves in distinct subtries are what make a state root cost anything.
#   SKIP_BUILD      Set to 1 to skip cargo build
#   KEEP_DATADIR    Set to 1 to leave the datadir and artifacts in place for inspection
#   WORK_BASE       Directory to create the scratch workdir under (default: $TMPDIR, then /tmp)
#   FINALIZED_LAG   Passed to `engine-replay replay --finalized-lag` (default: the driver's own)
#   METRICS_PORT    Prometheus port for the replay node (default 19001)
#   SCRAPE          1 (default) records node counters per block; 0 skips the scrape
#   PACE_MS         Idle time between blocks. Unset uses the driver's default, which is Celo's
#                   1 s block time. PACE_MS=0 replays flat out, which is ~50x faster but measures
#                   a queueing regime no sequencer is ever in: it manufactures a ~150 ms tail out
#                   of persistence overlapping every build. Fine for a functional check, useless
#                   as a latency number. Hold it constant across any comparison.
#   HTTP_PORT       Public RPC port (default 18545)
#   AUTH_PORT       Engine API port (default 18551)
#   NODE_LAUNCHER   Prefix for the replay node's command line. Empty by default. On Linux this is
#                   where load shaping goes, so that none of it leaks into the driver, e.g.
#                     NODE_LAUNCHER="systemd-run --scope -p MemoryMax=1G --"
#   NODE_EXTRA_ARGS Extra flags for the replay node, word-split. This is where a matrix cell varies
#                   the node under test, e.g.
#                     NODE_EXTRA_ARGS="--engine.persistence-threshold 1000000"   # ablate persistence
#                     NODE_EXTRA_ARGS="--engine.suppress-persistence-during-build"
#   EVICT_MS        The cache arm. Continuously drop the page cache for the node's mdbx.dat every
#                   EVICT_MS milliseconds during the replay, via scripts/perf/cachetool.c, so trie
#                   reads become page faults against the device. This is the condition
#                   celo-blockchain-planning#1453 names as the incident's root cause. Measured
#                   effect on a 2M-account state at EVICT_MS=50: node build mean 0.35 ms -> 25.2 ms
#                   and state root 0.15 ms -> 11.6 ms, i.e. ~72x and ~77x. Needs a C compiler.
set -euo pipefail

BLOCKS="${1:-30}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
GENESIS="${GENESIS_FILE:-$REPO_ROOT/e2e_test/celo-dev-genesis.json}"
[[ -f "$GENESIS" ]] || { echo "ERROR: genesis $GENESIS not found"; exit 1; }

HTTP_PORT="${HTTP_PORT:-18545}"
AUTH_PORT="${AUTH_PORT:-18551}"
METRICS_PORT="${METRICS_PORT:-19001}"
RPC_URL="http://127.0.0.1:$HTTP_PORT"
ENGINE_URL="http://127.0.0.1:$AUTH_PORT"
METRICS_URL="http://127.0.0.1:$METRICS_PORT"

# Pre-funded in the dev genesis; see e2e_test/shared.sh. make_state.py preserves the base alloc, so
# this key is funded under a generated genesis too.
ACC_PRIVKEY=0x2771aff413cac48d9f8c114fabddd9195a2129f3c2c436caa07e27bb7f58ead5

# --------------------------------------------------------------------------------------------
# Build
# --------------------------------------------------------------------------------------------

if [[ "${SKIP_BUILD:-}" != "1" ]]; then
    echo "==> Building celo-reth and engine-replay"
    cargo build --manifest-path "$REPO_ROOT/Cargo.toml" -p celo-reth -p engine-replay
fi
CELO_RETH="${CELO_RETH:-$REPO_ROOT/target/debug/celo-reth}"
ENGINE_REPLAY="${ENGINE_REPLAY:-$REPO_ROOT/target/debug/engine-replay}"
for bin in "$CELO_RETH" "$ENGINE_REPLAY"; do
    [[ -x "$bin" ]] || { echo "ERROR: $bin not found or not executable"; exit 1; }
done

# --------------------------------------------------------------------------------------------
# Port preflight
# --------------------------------------------------------------------------------------------

# A node left behind by an interrupted run holds these ports, and reth's failure ("address already
# in use") arrives ~15 lines into a log the script would otherwise report as a build problem. Fail
# up front with the fix instead. Deliberately does not kill anything: on a shared host the squatter
# may be someone else's node.
for port in "$HTTP_PORT" "$AUTH_PORT" "$METRICS_PORT"; do
    if command -v lsof >/dev/null 2>&1 && lsof -ti :"$port" >/dev/null 2>&1; then
        echo "ERROR: port $port is already in use, probably by a leaked node from an earlier run."
        echo "       Inspect it with:  lsof -ti :$port"
        echo "       Then either kill it, or re-run with HTTP_PORT/AUTH_PORT/METRICS_PORT set to"
        echo "       free ports."
        exit 1
    fi
done

# --------------------------------------------------------------------------------------------
# Workspace
# --------------------------------------------------------------------------------------------

WORK_BASE="${WORK_BASE:-${TMPDIR:-/tmp}}"
if ! WORKDIR="$(mktemp -d "${WORK_BASE%/}/engine-replay.XXXXXX")"; then
    echo "ERROR: could not create a scratch directory under $WORK_BASE. Set WORK_BASE to a"
    echo "       writable directory and retry."
    exit 1
fi
DATADIR="$WORKDIR/datadir"
ARCHIVE="$WORKDIR/blocks.jsonl"
TIMINGS="$WORKDIR/timings.jsonl"
MINE_LOG="$WORKDIR/node-mine.log"
REPLAY_LOG="$WORKDIR/node-replay.log"
NODE_PID=

cleanup() {
    if [[ -n "$NODE_PID" ]]; then
        kill "$NODE_PID" 2>/dev/null || true
        wait "$NODE_PID" 2>/dev/null || true
    fi
    if [[ "${KEEP_DATADIR:-}" == "1" ]]; then
        echo "Artifacts kept in $WORKDIR"
    else
        rm -rf "$WORKDIR"
    fi
}
trap cleanup EXIT

# Wait until the public RPC answers, or the node dies.
wait_for_rpc() {
    for _ in $(seq 1 120); do
        if curl -s -m 2 -X POST -H 'content-type: application/json' \
            --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' \
            "$RPC_URL" 2>/dev/null | grep -q '"result"'; then
            return 0
        fi
        if ! kill -0 "$NODE_PID" 2>/dev/null; then
            echo "ERROR: node exited early. Last 40 lines:"
            tail -40 "$1"
            exit 1
        fi
        sleep 0.5
    done
    echo "ERROR: node did not become ready. Last 40 lines:"
    tail -40 "$1"
    exit 1
}

# Current head height over the public RPC.
block_number() {
    curl -s -X POST -H 'content-type: application/json' \
        --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' "$RPC_URL" |
        sed -E 's/.*"result":"0x([0-9a-fA-F]*)".*/\1/' |
        (read -r hex; printf '%d\n' "$((16#${hex:-0}))")
}

# --------------------------------------------------------------------------------------------
# Phase 1: mine a dev chain with real transactions in it
# --------------------------------------------------------------------------------------------

echo "==> Initialising datadir ($DATADIR)"
"$CELO_RETH" init --chain "$GENESIS" --datadir "$DATADIR" >"$MINE_LOG" 2>&1

# `--dev.block-time 1s` is not cosmetic. The default dev mining mode is instant, which can mint
# several blocks inside the same second, and reth rejects payload attributes whose timestamp is
# not strictly greater than the head's — making those blocks permanently unreplayable.
echo "==> Mining $BLOCKS dev blocks"
"$CELO_RETH" node --dev --dev.block-time 1s \
    --chain "$GENESIS" \
    --datadir "$DATADIR" \
    --http --http.port "$HTTP_PORT" --http.api eth,debug \
    --authrpc.port "$AUTH_PORT" \
    --disable-discovery --port 0 \
    >>"$MINE_LOG" 2>&1 &
NODE_PID=$!
wait_for_rpc "$MINE_LOG"

# Put real transactions in the chain if foundry is available. Without them the archive is
# deposit-only, which still exercises the whole loop but not the EIP-2718 round trip of ordinary
# transactions.
#
# Recipients are fresh, uniformly spread addresses rather than one burn address, because what makes
# a state root cost anything is *new leaves in distinct subtries*. Repeated transfers to a single
# recipient touch one leaf and tell you nothing about trie work. Submitted with explicit nonces and
# `--async`, in parallel batches, so they land in the pool faster than one-at-a-time receipt waits
# would allow.
#
# Known gap: this does not produce CIP-64 (type 0x7b) transactions — cast cannot sign them. The
# e2e suite's viem tests can; archiving a range from a node that has run e2e_test/run_all_tests.sh
# is the way to cover the 0x7b encoding path.
if command -v cast >/dev/null 2>&1; then
    echo "==> Sending ${TXS:-10} transactions to fresh addresses"
    start_nonce="$(cast nonce --rpc-url "$RPC_URL" \
        "$(cast wallet address --private-key "$ACC_PRIVKEY")" 2>/dev/null || echo 0)"
    # Same derivation as scripts/perf/make_state.py, different domain, so recipients are new
    # accounts spread across the trie rather than neighbours sharing a long prefix.
    python3 -c "
import hashlib, sys
for i in range(int(sys.argv[1])):
    print('0x' + hashlib.sha3_256(b'recipient' + i.to_bytes(8, 'big')).digest()[:20].hex())
" "${TXS:-10}" >"$WORKDIR/recipients.txt"

    # Wait only on the cast PIDs collected so far. A bare `wait` would also wait on the mining node,
    # which is a background child of this same shell and never exits — that deadlocks the script
    # after mining has already succeeded.
    i=0
    batch=""
    while read -r recipient; do
        cast send --rpc-url "$RPC_URL" --private-key "$ACC_PRIVKEY" --async \
            --nonce "$((start_nonce + i))" --value 1000 "$recipient" >/dev/null 2>&1 &
        batch="$batch $!"
        i=$((i + 1))
        # Bounded fan-out: a few hundred simultaneous cast processes is its own load test.
        if [[ $((i % 25)) -eq 0 ]]; then
            # shellcheck disable=SC2086  # deliberate word split over the collected PIDs
            wait $batch 2>/dev/null || true
            batch=""
        fi
    done <"$WORKDIR/recipients.txt"
    if [[ -n "$batch" ]]; then
        # shellcheck disable=SC2086
        wait $batch 2>/dev/null || true
    fi
    echo "    submitted $i transactions from nonce $start_nonce"
else
    echo "WARNING: cast not found; the archive will contain deposit-only blocks"
fi

echo "==> Waiting for $BLOCKS blocks"
for _ in $(seq 1 240); do
    height="$(block_number)"
    [[ "$height" -ge "$BLOCKS" ]] && break
    sleep 0.5
done
height="$(block_number)"
if [[ "$height" -lt "$BLOCKS" ]]; then
    echo "ERROR: only reached block $height of $BLOCKS"
    tail -40 "$MINE_LOG"
    exit 1
fi
echo "    head is block $height"

# --------------------------------------------------------------------------------------------
# Phase 2: archive, then unwind. This order is mandatory.
# --------------------------------------------------------------------------------------------

echo "==> Archiving blocks 1..$BLOCKS"
"$ENGINE_REPLAY" archive --rpc-url "$RPC_URL" --from 1 --to "$BLOCKS" --out "$ARCHIVE"

echo "==> Stopping the miner"
kill "$NODE_PID" 2>/dev/null || true
wait "$NODE_PID" 2>/dev/null || true
NODE_PID=

# No `--offline`: that variant skips FinishStage, leaving the stage checkpoint reth reads on
# startup at the pre-unwind tip.
echo "==> Unwinding to genesis"
"$CELO_RETH" stage unwind --chain "$GENESIS" --datadir "$DATADIR" to-block 0 >>"$MINE_LOG" 2>&1

# --------------------------------------------------------------------------------------------
# Phase 3: replay
# --------------------------------------------------------------------------------------------

# No `--dev` here: reth's local miner would drive forkchoice itself and race the driver.
# `--builder.deadline` is raised because a build that outruns it is dropped, and getPayload then
# fails — which under load shaping would hit the slow configuration and not the fast one.
echo "==> Restarting the node for replay (no --dev)"
# shellcheck disable=SC2086  # NODE_LAUNCHER and NODE_EXTRA_ARGS are intentionally word-split
${NODE_LAUNCHER:-} "$CELO_RETH" node \
    --chain "$GENESIS" \
    --datadir "$DATADIR" \
    --http --http.port "$HTTP_PORT" --http.api eth,debug \
    --authrpc.addr 127.0.0.1 --authrpc.port "$AUTH_PORT" \
    --builder.deadline 600 \
    --metrics "127.0.0.1:$METRICS_PORT" \
    --disable-discovery --max-peers 0 --no-persist-peers --port 0 \
    --rollup.disable-tx-pool-gossip \
    ${NODE_EXTRA_ARGS:-} \
    >"$REPLAY_LOG" 2>&1 &
NODE_PID=$!
wait_for_rpc "$REPLAY_LOG"

head_after_unwind="$(block_number)"
if [[ "$head_after_unwind" -ne 0 ]]; then
    echo "ERROR: expected the head at block 0 after unwinding, got $head_after_unwind"
    exit 1
fi
echo "    head is block 0, as expected"

echo "==> Replaying"
# Built as one array that is never empty: expanding an empty array under `set -u` is a fatal error
# in bash <= 4.3, i.e. in the /bin/bash 3.2 that stock macOS ships. That would abort here — after
# mining, archiving and unwinding have all succeeded — and read as a failure of a rig that is fine.
replay_args=(
    --archive "$ARCHIVE"
    --engine-url "$ENGINE_URL"
    --jwt "$DATADIR/jwt.hex"
    --out "$TIMINGS"
)
# Only override the driver's own default when asked, so the honest default (a real block time) is
# what a bare run reports.
if [[ -n "${PACE_MS:-}" ]]; then
    replay_args+=(--pace-ms "$PACE_MS")
fi
# Scraping is on by default because the trie read count is the point of the exercise, but it costs
# two ~800 KB exposition renders per block on the node under test. SCRAPE=0 measures without it.
if [[ "${SCRAPE:-1}" == "1" ]]; then
    replay_args+=(--metrics-url "$METRICS_URL" --metrics-raw "$WORKDIR/metrics-raw.txt")
fi
if [[ -n "${FINALIZED_LAG:-}" ]]; then
    replay_args+=(--finalized-lag "$FINALIZED_LAG")
fi
# The cache arm runs only for the duration of the replay, so the mine and archive phases are not
# slowed by it and only the measured window is shaped.
EVICT_PID=
if [[ -n "${EVICT_MS:-}" ]]; then
    if cc -O2 -o "$WORKDIR/cachetool" "$SCRIPT_DIR/cachetool.c" 2>"$WORKDIR/cachetool-build.log"; then
        echo "==> Evicting the page cache for mdbx.dat every ${EVICT_MS} ms during the replay"
        "$WORKDIR/cachetool" loop "$DATADIR/db/mdbx.dat" "$EVICT_MS" \
            >"$WORKDIR/cachetool.log" 2>&1 &
        EVICT_PID=$!
    else
        echo "ERROR: could not build cachetool; see $WORKDIR/cachetool-build.log"
        exit 1
    fi
fi

set +e
"$ENGINE_REPLAY" replay "${replay_args[@]}" >"$WORKDIR/summary.json"
replay_status=$?
set -e

if [[ -n "$EVICT_PID" ]]; then
    kill "$EVICT_PID" 2>/dev/null || true
    wait "$EVICT_PID" 2>/dev/null || true
fi

# --------------------------------------------------------------------------------------------
# Verdict
# --------------------------------------------------------------------------------------------

if [[ $replay_status -ne 0 ]]; then
    echo "FAIL: engine-replay exited $replay_status (3 = hash mismatch, 4 = not replayable)"
    echo "--- last 40 lines of the node log ---"
    tail -40 "$REPLAY_LOG"
    exit 1
fi

replayed="$(wc -l <"$TIMINGS" | tr -d ' ')"
matched="$(grep -c '"hash_match":true' "$TIMINGS" || true)"
if [[ "$replayed" -ne "$BLOCKS" || "$matched" -ne "$BLOCKS" ]]; then
    echo "FAIL: expected $BLOCKS replayed blocks all matching, got $replayed replayed / $matched matched"
    exit 1
fi

echo
echo "PASS: $BLOCKS blocks replayed, every block hash reproduced"
cat "$WORKDIR/summary.json"
echo
