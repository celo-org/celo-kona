#!/usr/bin/env bash
# Replay one archive N times against one datadir under a defined page-cache regime.
#
# An *arm* is one replay under one condition. Everything the arm varies is an environment variable,
# and everything else is held fixed by construction: the same archive, the same datadir, the same
# binaries, `stage unwind to-block 0` between reps. That is what makes two arms comparable — a claim
# like "the walk has no irreducible cost" needs the pair to differ in exactly one thing, and reusing
# the archive rather than re-mining is how the workload stops being one of the differences.
#
# scripts/perf/bootstrap_dev_replay.sh mines the archive and leaves a workdir (KEEP_DATADIR=1); this
# replays it; scripts/perf/analyse_arms.py compares the arms afterwards.
#
# Env:
#   WORKDIR         Required. A kept bootstrap workdir, holding blocks.jsonl and datadir/.
#   GENESIS_FILE    Chain spec, must be the one the archive was mined against
#                   (default: e2e_test/celo-dev-genesis.json).
#   LABEL           Names the arm. Outputs are t-$LABEL-$rep.jsonl (per-block timings and metric
#                   deltas), s-, raw-, post-, evict-. Analysis selects arms by this name, so make it
#                   mean something.
#   REPS            Replays of the archive (default 5). Reps exist to separate block-driven cost from
#                   machine-driven cost: a spike at the same block number every rep is the state, a
#                   spike that moves is the environment. Note that deterministic counters — trie
#                   seeks, changed entries — are IDENTICAL across reps, so reps add no independent
#                   observations for those; analyse_arms.py drops the duplicates rather than let
#                   them deflate a standard error.
#   PACE            Milliseconds between blocks (default 0, i.e. as fast as it will go). Use 1000 to
#                   replay at Celo's block time; back-to-back blocks are a different regime.
#   SETTLE          Seconds to wait after the node answers RPC, before replaying. eth_blockNumber
#                   answers well before the node's startup work (consistency check, static-file
#                   init, post-unwind bookkeeping) has settled, and that work overlapping block 1
#                   looks exactly like a cold-cache cost.
#   SCRAPE          1 records node counters per block into the timings (needed by analyse_arms.py).
#   POST_SCRAPE     1 (default) reads the cumulative counters once after the replay. This is the only
#                   way to see a counter in the same unperturbed regime the timings were taken in:
#                   per-block scraping is itself slow enough to move the node into another regime.
#
# The cache arms, in increasing order of usefulness:
#
#   EVICT_MS=<ms>   Evict continuously, every <ms>. The node's trie reads become device round trips
#                   instead of RAM hits, which is the regime celo-blockchain-planning#1453 identifies
#                   as the incident's root cause. Destroys path reuse by construction, so it cannot
#                   answer any question about locality — two arms that differ only in path reuse MUST
#                   look alike here, and if they do that is the instrument, not the trie.
#   EVICT_AFTER_S=<s>
#                   Evict ONCE, <s> seconds into the replay, then leave the cache alone. Pages the
#                   node faults in stay resident, so residency accumulates the way it does on a real
#                   node with a finite cache. This is the only regime in which path reuse across
#                   blocks can show up at all, and it adds no periodic noise — unlike EVICT_MS, whose
#                   phase relative to each build is arbitrary and left one regression unfittable at
#                   R2 0.09. Residency accumulating also means cost falls with block index, so
#                   compare entry-matched blocks late in the run, not run-wide medians.
#   EVICT_ONCE=1    Evict once immediately BEFORE the replay. Kept because it is the obvious thing to
#                   reach for and it does not work: after `stage unwind to-block 0` the first builds
#                   reconstruct the whole intermediate trie, which walks every account and pulls the
#                   file straight back into cache, undoing the eviction before block 4. Measured, not
#                   assumed — this arm came out at 23.7-31.3 us per changed entry against a fully
#                   warm arm's 22.1-34.0, i.e. indistinguishable from never having evicted. Use
#                   EVICT_AFTER_S.
#
#   No eviction variable set = the warm arm, and it is not a throwaway control: it is the floor that
#   turns "the root is slow" into "the root is slow because the pages are not there".
#
# Also honoured: CELO_PAYLOAD_TRIE_PREFETCH_THREADS is inherited by the node, so the prefetch is an
# arm like any other. CELO_RETH / ENGINE_REPLAY / CACHETOOL / CACHETOOL_TARGET / HTTP_PORT /
# AUTH_PORT / METRICS_PORT / NODE_EXTRA_ARGS override the defaults below.
#
# Recipes. Warm floor and prefetch A/B (five arms over one archive):
#
#   for arm in "warm-off::" "cold-off:50:" "cold-pf1:50:1" "cold-pf8:50:8" "cold-pf20:50:20"; do
#       IFS=: read -r label evict threads <<<"$arm"
#       CELO_PAYLOAD_TRIE_PREFETCH_THREADS="$threads" EVICT_MS="$evict" \
#           LABEL="$label" REPS=2 PACE=1000 SCRAPE=1 WORKDIR="$W" scripts/perf/replay_arms.sh
#   done
#   scripts/perf/analyse_arms.py phases --workdir "$W" \
#       --arm warm-off --arm cold-off --arm cold-pf1 --arm cold-pf8 --arm cold-pf20
#
# Insertion vs update vs locality needs one archive PER recipient mode, since the mode changes what
# was mined (see RECIPIENT_MODE in bootstrap_dev_replay.sh), then one arm per mode:
#
#   EVICT_AFTER_S=7 LABEL="$mode-coldafter" REPS=3 PACE=1000 SCRAPE=1 \
#       WORKDIR="$W_mode" scripts/perf/replay_arms.sh
#   scripts/perf/analyse_arms.py slopes --regime coldafter --y root --first-block 10 \
#       --arm "existing-hot=$W_hot" --arm "existing-scattered=$W_scat" --arm "fresh=$W_fresh"
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORKDIR="${WORKDIR:?set WORKDIR to a kept bootstrap workdir (KEEP_DATADIR=1)}"
# Release, not debug: a debug build's own overhead is larger than most of the effects being measured.
CELO_RETH="${CELO_RETH:-$REPO_ROOT/target/release/celo-reth}"
ENGINE_REPLAY="${ENGINE_REPLAY:-$REPO_ROOT/target/release/engine-replay}"
GENESIS="${GENESIS_FILE:-$REPO_ROOT/e2e_test/celo-dev-genesis.json}"
CACHETOOL="${CACHETOOL:-$REPO_ROOT/target/cachetool}"
DATADIR="$WORKDIR/datadir"
ARCHIVE="$WORKDIR/blocks.jsonl"
CACHETOOL_TARGET="${CACHETOOL_TARGET:-$DATADIR/db/mdbx.dat}"
REPS="${REPS:-5}"
LABEL="${LABEL:-run}"
HTTP_PORT="${HTTP_PORT:-18545}"
AUTH_PORT="${AUTH_PORT:-18551}"
METRICS_PORT="${METRICS_PORT:-19001}"
EXTRA="${NODE_EXTRA_ARGS:-}"

for f in "$ARCHIVE" "$CELO_RETH" "$ENGINE_REPLAY" "$GENESIS"; do
    [[ -e "$f" ]] || { echo "missing: $f" >&2; exit 1; }
done

# Build cachetool on demand, and only when an arm actually evicts. A cold arm that failed to evict is
# a warm arm wearing a cold arm's label, and it reads as a null result rather than as a broken run —
# so every failure below is fatal or loudly flagged, never swallowed.
EVICTS="${EVICT_MS:-}${EVICT_AFTER_S:-}${EVICT_ONCE:-}"
if [[ -n "$EVICTS" ]]; then
    if [[ ! -x "$CACHETOOL" ]]; then
        echo "==> building cachetool"
        cc -O2 -o "$CACHETOOL" "$REPO_ROOT/scripts/perf/cachetool.c" || {
            echo "cannot build cachetool, but this arm evicts — refusing to run a fake cold arm" >&2
            exit 1; }
    fi
    [[ -f "$CACHETOOL_TARGET" ]] || {
        echo "no such eviction target: $CACHETOOL_TARGET" >&2; exit 1; }
fi

NODE_PID=
cleanup() { [[ -n "$NODE_PID" ]] && { kill "$NODE_PID" 2>/dev/null; wait "$NODE_PID" 2>/dev/null; }; }
trap cleanup EXIT

for rep in $(seq 1 "$REPS"); do
    evict_log="$WORKDIR/evict-$LABEL-$rep.log"
    failed_marker="$WORKDIR/.evict-failed-$LABEL-$rep"
    rm -f "$failed_marker"

    "$CELO_RETH" stage unwind --chain "$GENESIS" --datadir "$DATADIR" to-block 0 >/dev/null 2>&1

    # shellcheck disable=SC2086
    "$CELO_RETH" node --chain "$GENESIS" --datadir "$DATADIR" \
        --http --http.port "$HTTP_PORT" --http.api eth,debug \
        --authrpc.addr 127.0.0.1 --authrpc.port "$AUTH_PORT" \
        --builder.deadline 600 \
        --disable-discovery --max-peers 0 --no-persist-peers --port 0 \
        --rollup.disable-tx-pool-gossip \
        --metrics "127.0.0.1:$METRICS_PORT" \
        $EXTRA >"$WORKDIR/rep-$LABEL-$rep.log" 2>&1 &
    NODE_PID=$!

    ready=0
    for _ in $(seq 1 120); do
        if curl -s -m 2 -X POST -H 'content-type: application/json' \
            --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' \
            "http://127.0.0.1:$HTTP_PORT" 2>/dev/null | grep -q '"result"'; then ready=1; break; fi
        kill -0 "$NODE_PID" 2>/dev/null || break
        sleep 0.5
    done
    if [[ "$ready" != 1 ]]; then
        echo "rep$rep: node never became ready" >&2
        tail -20 "$WORKDIR/rep-$LABEL-$rep.log" >&2
        exit 1
    fi
    [[ -n "${SETTLE:-}" ]] && sleep "$SETTLE"

    if [[ -n "${EVICT_ONCE:-}" ]]; then
        "$CACHETOOL" evict "$CACHETOOL_TARGET" >"$evict_log" 2>&1 || {
            echo "rep$rep: eviction failed; this arm is not cold" >&2; exit 1; }
    fi

    # Backgrounded, so it fires mid-replay. It cannot abort the run from a subshell, so it leaves a
    # marker and the rep is reported as suspect rather than quietly mislabelled.
    EVICT_AFTER_PID=
    if [[ -n "${EVICT_AFTER_S:-}" ]]; then
        ( sleep "$EVICT_AFTER_S"
          "$CACHETOOL" evict "$CACHETOOL_TARGET" >>"$evict_log" 2>&1 || touch "$failed_marker" ) &
        EVICT_AFTER_PID=$!
    fi

    EVICT_PID=
    if [[ -n "${EVICT_MS:-}" ]]; then
        "$CACHETOOL" loop "$CACHETOOL_TARGET" "$EVICT_MS" >>"$evict_log" 2>&1 &
        EVICT_PID=$!
    fi

    metrics_replay=()
    if [[ "${SCRAPE:-0}" == "1" ]]; then
        metrics_replay=(
            --metrics-url "http://127.0.0.1:$METRICS_PORT"
            --metrics-raw "$WORKDIR/raw-$LABEL-$rep.txt"
            --metrics-filter backpressure --metrics-filter persistence
            --metrics-filter trie --metrics-filter payload
        )
    fi
    "$ENGINE_REPLAY" replay --archive "$ARCHIVE" \
        --engine-url "http://127.0.0.1:$AUTH_PORT" --jwt "$DATADIR/jwt.hex" \
        --pace-ms "${PACE:-0}" "${metrics_replay[@]}" \
        --out "$WORKDIR/t-$LABEL-$rep.jsonl" >"$WORKDIR/s-$LABEL-$rep.json" 2>/dev/null
    status=$?

    if [[ -n "$EVICT_PID" ]]; then
        # The loop runs until killed, so a non-zero status here is the kill, not a failure. Its log
        # reports residency each pass; that is where to check the arm was actually cold.
        kill "$EVICT_PID" 2>/dev/null; wait "$EVICT_PID" 2>/dev/null
    fi
    if [[ -n "$EVICT_AFTER_PID" ]]; then
        wait "$EVICT_AFTER_PID" 2>/dev/null
    fi
    if [[ -e "$failed_marker" ]]; then
        echo "rep$rep: WARNING the mid-replay eviction failed — treat this rep as warm, not cold" >&2
        tail -3 "$evict_log" >&2
    fi

    if [[ "${POST_SCRAPE:-1}" == "1" ]]; then
        curl -s -m 5 "http://127.0.0.1:$METRICS_PORT" >"$WORKDIR/post-$LABEL-$rep.txt" || true
    fi

    kill "$NODE_PID" 2>/dev/null; wait "$NODE_PID" 2>/dev/null; NODE_PID=

    if [[ $status -ne 0 ]]; then echo "rep$rep: replay exited $status" >&2; continue; fi
    python3 "$REPO_ROOT/scripts/perf/analyse_arms.py" rep \
        --timings "$WORKDIR/t-$LABEL-$rep.jsonl" \
        --post "$WORKDIR/post-$LABEL-$rep.txt" --label "$LABEL" --rep "$rep"
done
