#!/bin/bash
# Exact-output regression tests for celo-reth's call, trace and receipt RPC
# surface, with a focus on the CIP-64 fee-currency paths.
#
# Why this exists
# ---------------
# Every bug we have shipped in the RPC replay paths — the `Cip64Storage`
# double-store, the base-fee-check store gate, the mid-block fee-context
# regression — produced a *successful* response with wrong numbers in it. The
# rest of the suite asserts liveness ("no error field, and `from` matches"),
# which cannot see any of them. Here the committed JSON *is* the assertion:
# every gas value, every fee-currency amount and every call frame is compared
# byte-for-byte.
#
# Determinism
# -----------
# The node is private to this test and starts from `celo-dev-genesis.json`, so
# its whole history is a function of the transactions below.
#
#  * Read-only scenarios are pinned to block `0x0`. Calls resolve their EVM env
#    from the *named* block's header, so base fee, timestamp, coinbase and gas
#    limit are literals from the genesis file rather than runtime values.
#  * Mined scenarios submit transactions strictly one at a time, waiting for
#    each receipt. Bare `--dev` is instant-mining and never mines an empty block
#    (reth `crates/engine/local/src/miner.rs`, `MiningMode::Instant` returns
#    Pending on an empty pool), so this yields exactly one transaction per
#    block, hence a fixed base-fee sequence. Do not add `--dev.block-time`: an
#    interval miner produces empty blocks on every tick and the base fee then
#    depends on wall-clock timing.
#  * Where several transactions must share a block, they are submitted behind a
#    nonce gap so they stay *queued* (which triggers no block build) until the
#    gap-filling transaction promotes all of them at once.
#  * Transactions are signed offline with every field pinned, so their hashes
#    are stable and can be committed in the goldens.
#
# Measured against two independent runs from a fresh datadir, the only response
# fields that vary are the ones derived from reth's dev payload builder, which
# randomizes the fee recipient and prev_randao and stamps a wall-clock
# timestamp: block hashes and roots, timestamps, `miner`, `logsBloom`, and the
# CIP-64 credit log's recipient topic. Those are normalized away here — and the
# credit recipient is asserted structurally instead, since "the tip goes to the
# block's fee recipient" is a real invariant. Everything else, including block
# numbers, transaction indices, cumulative gas, effective gas prices and the
# fee-currency-denominated base fee, is compared exactly.
#
# Regenerating goldens after an intentional change:
#     BLESS=1 e2e_test/test_rpc_golden.sh
# then review `git diff -- e2e_test/rpc_golden`. A bless run exits non-zero on
# purpose — it overwrites the expectations instead of checking them.
#shellcheck disable=SC2034  # RPC_JQ_ARGS is consumed by rpc_assert.sh
# No `set -e`: a failed check must still reach the summary at the bottom, which
# reports the tally and decides the exit status. Setup steps abort explicitly.
set -o pipefail

# By path, not by name: a bare `source shared.sh` only resolves when the caller
# happens to run from e2e_test/, and without `set -e` a miss would leave every
# address below empty instead of aborting.
source "$(dirname "$0")/shared.sh"

if [[ -n "${NETWORK:-}" ]]; then
    echo "SKIP: golden RPC tests only run against the local dev genesis (NETWORK=$NETWORK)"
    exit 0
fi

for cmd in jq curl cast node; do
    command -v "$cmd" &>/dev/null || { echo "FAIL: $cmd is required"; exit 1; }
done

CELO_RETH="${CELO_RETH:-$SCRIPT_DIR/../target/debug/celo-reth}"
if [[ ! -x "$CELO_RETH" ]]; then
    echo "FAIL: celo-reth binary not found at $CELO_RETH"
    exit 1
fi

# Phase 3 onwards signs its transactions with viem. run_all_tests.sh installs
# the js-tests dependencies before its loop, but this test also has to work on
# its own — that is the invocation the bless workflow documents.
prepare_node

# ---------------------------------------------------------------------------
# A private node, on OS-assigned ports so its RPC cannot collide with the shared
# runner node's. Only the *ports* are isolated: SAVED_NODE_LOG below and
# prepare_node's shared js-tests/node_modules are both fixed paths, so two
# copies of this test still must not run concurrently.
# ---------------------------------------------------------------------------

# An explicit template, because BSD `mktemp -d` without one ignores TMPDIR and
# always lands in the OS temp directory. Failure is fatal: an empty DATADIR
# would send the datadir and the log to the filesystem root.
DATADIR=$(mktemp -d "${TMPDIR:-/tmp}/celo-rpc-golden.XXXXXX") || {
    echo "FAIL: could not create a temporary datadir"
    exit 1
}
NODE_LOG="$DATADIR/celo-reth.log"
NODE_PID=

# The node log lives in the datadir, which this function deletes. Anything that
# failed was very likely explained in there, so keep a copy next to the shared
# runner's log — that is the path CI uploads on failure.
SAVED_NODE_LOG="$SCRIPT_DIR/celo-reth-golden.log"

cleanup() {
    if [[ ${RPC_GOLDEN_FAILED:-0} -gt 0 && -f "$NODE_LOG" ]]; then
        cp "$NODE_LOG" "$SAVED_NODE_LOG" 2>/dev/null &&
            echo "rpc_assert: node log saved to ${SAVED_NODE_LOG#"$SCRIPT_DIR/"}"
    fi
    if [[ -n "$NODE_PID" ]]; then
        kill "$NODE_PID" 2>/dev/null || true
        # Bounded: a node wedged on shutdown must not hang the test run.
        for _ in {1..20}; do
            kill -0 "$NODE_PID" 2>/dev/null || break
            sleep 0.5
        done
        kill -9 "$NODE_PID" 2>/dev/null || true
        wait "$NODE_PID" 2>/dev/null || true
    fi
    rm -rf "$DATADIR"
}
trap cleanup EXIT

GENESIS_JSON="$SCRIPT_DIR/celo-dev-genesis.json"

if ! "$CELO_RETH" init --chain "$GENESIS_JSON" --datadir "$DATADIR" &>"$NODE_LOG"; then
    echo "FAIL: celo-reth init failed"
    tail -40 "$NODE_LOG"
    exit 1
fi

# `trace` and `ots` are enabled because their replay paths share the machinery
# the CIP-64 scenarios below exercise; the shared runner node does not enable
# them.
"$CELO_RETH" node --dev \
    --chain "$GENESIS_JSON" \
    --datadir "$DATADIR" \
    --http \
    --http.port 0 \
    --http.api eth,web3,net,debug,trace,ots \
    --authrpc.port 0 \
    --port 0 \
    --disable-discovery \
    --ipcdisable \
    --color never \
    >>"$NODE_LOG" 2>&1 &
NODE_PID=$!

HTTP_PORT=
for _ in {1..60}; do
    if ! kill -0 "$NODE_PID" 2>/dev/null; then
        echo "FAIL: celo-reth exited during startup"
        tail -40 "$NODE_LOG"
        exit 1
    fi
    HTTP_PORT=$(grep -m1 'RPC HTTP server started' "$NODE_LOG" | grep -oE '[0-9]+$' || true)
    [[ -n "$HTTP_PORT" ]] && break
    sleep 0.5
done
if [[ -z "$HTTP_PORT" ]]; then
    echo "FAIL: celo-reth did not report its RPC port"
    tail -40 "$NODE_LOG"
    exit 1
fi

export RPC_URL="http://127.0.0.1:$HTTP_PORT"
export RPC_GOLDEN_DIR="$SCRIPT_DIR/rpc_golden"
# `|| exit`: rpc_assert.sh refuses to load without RPC_GOLDEN_DIR, and a
# `return` from a sourced file only ends that file — without this the run would
# carry on with none of the helpers defined.
source "$SCRIPT_DIR/rpc_assert.sh" || exit 1

for _ in {1..60}; do
    [[ "$(rpc_call eth_blockNumber '[]' | jq -r '.result // empty')" == "0x0" ]] && break
    sleep 0.5
done
if [[ "$(rpc_call eth_blockNumber '[]' | jq -r '.result // empty')" != "0x0" ]]; then
    echo "FAIL: celo-reth did not become ready at the genesis block"
    tail -40 "$NODE_LOG"
    exit 1
fi

# ---------------------------------------------------------------------------
# Shared values
# ---------------------------------------------------------------------------

DEAD=0x000000000000000000000000000000000000dEaD
ERC20_DATA=$(cast calldata 'transfer(address,uint256)' "$DEAD" 100)
# More than the sender owns: the token-duality transfer reverts, so the CIP-64
# transaction is still charged but its main frame fails.
ERC20_OVERDRAFT_DATA=$(cast calldata 'transfer(address,uint256)' "$DEAD" \
    1000000000000000000000000000000)
BALANCE_OF_DATA=$(cast calldata 'balanceOf(address)' "$ACC_ADDR")

CHAIN_ID=$(( $(rpc_call eth_chainId '[]' | jq -r '.result') ))

sign_tx() { # <sign_raw_tx.mjs args...> -> {"hash","raw"}
    (cd "$SCRIPT_DIR/js-tests" && ACC_PRIVKEY="$ACC_PRIVKEY" \
        node sign_raw_tx.mjs --chain-id "$CHAIN_ID" "$@")
}

send_raw() { # <name> <raw> -> tx hash on stdout; reports a rejection as a failure
    local response
    response=$(rpc_call eth_sendRawTransaction "[\"$2\"]")
    if _rpc_has_error "$response"; then
        _rpc_fail "send_$1" "$(_rpc_error_msg "$response")"
        return 1
    fi
    jq -r '.result // empty' <<<"$response"
}

wait_receipt() { # <hash> -> receipt JSON, or empty
    local response
    for _ in {1..80}; do
        response=$(rpc_call eth_getTransactionReceipt "[\"$1\"]")
        if jq -e '.result != null' <<<"$response" >/dev/null 2>&1; then
            jq -c '.result' <<<"$response"
            return 0
        fi
        sleep 0.25
    done
    return 1
}

# Normalization applied to every receipt golden. Only the fields reth's dev
# payload builder randomizes are removed; block number, transaction index,
# cumulative gas, effective gas price and the CIP-64 `baseFee` are all
# deterministic here and are part of the assertion.
#
# `logsBloom` is dropped because it commits to the randomized fee-recipient
# topic; the log list itself is compared in full instead.
RECEIPT_FILTER='del(.blockHash, .logsBloom)
    | .logs |= map(del(.blockHash, .blockTimestamp))'
# The CIP-64 fee credit is paid to the block's fee recipient, which reth's dev
# mode randomizes per block. Replace just that topic with a marker; the real
# recipient is asserted against the block header separately.
CREDIT_TOPIC_FILTER='.logs |= map(
    if (.topics | length) > 2 and (.topics[2] | ascii_downcase) == $minerTopic
    then .topics[2] = "<block-fee-recipient>" else . end)'
TRACE_TX_FILTER='del(.blockHash)'
# One line per opcode step. The full per-step stack would make these goldens tens
# of thousands of lines and unreviewable, while the stack *depth* still moves
# whenever the opcode sequence does. Stack contents are covered by the prestate
# and callTracer goldens.
#
# The step's own `error` is kept: without it a halt at the interpreter boundary
# (out of gas, stack underflow, invalid jump) shows up only as a discontinuity
# in the gas column, and the top-level `failed` flag reports the frame's outcome
# rather than which step produced it.
STRUCTLOG_FILTER='{gas, failed, returnValue, structLogs: [.structLogs[]
    | "\(.pc) \(.op) gas=\(.gas) cost=\(.gasCost) depth=\(.depth) refund=\(.refund) stack=\(.stack | length) err=\(.error // "")"]}'
# The gas ratios are the only JSON floats in the whole corpus, and jq renders
# doubles differently across versions (1.6 prints `0` where 1.7 keeps `0.0`),
# which would show up as a golden diff on a machine with an older jq. Pin them
# as parts per million instead: same signal, integer output.
FEE_HISTORY_FILTER='.gasUsedRatio |= map(. * 1000000 | round)
    | .blobGasUsedRatio |= map(. * 1000000 | round)'

# ---------------------------------------------------------------------------
# Phase 1 — read-only calls at the genesis block
# ---------------------------------------------------------------------------

echo ""
echo "Phase 1: call/trace/simulate at the genesis block"

# Every golden below is a function of the genesis state. Pinning the genesis
# block first means a regenerated `celo-dev-genesis.json` reports itself here,
# instead of as several dozen unexplained diffs further down.
rpc_golden genesis_identity eth_getBlockByNumber '["0x0", false]' \
    '{hash, stateRoot, baseFeePerGas, gasLimit, timestamp, extraData, miner}'

substitute() { # <json> -> json with @PLACEHOLDER@ values filled in
    jq -c \
        --arg acc "$ACC_ADDR" \
        --arg dead "$DEAD" \
        --arg token "$TOKEN_ADDR" \
        --arg fc "$FEE_CURRENCY" \
        --arg fc2 "$FEE_CURRENCY2" \
        --arg erc20 "$ERC20_DATA" \
        --arg balanceof "$BALANCE_OF_DATA" '
        walk(if type == "string" then
            gsub("@ACC@"; $acc)
            | gsub("@DEAD@"; $dead)
            | gsub("@TOKEN@"; $token)
            # @FEE_CURRENCY2@ first: the other pattern is a prefix of it and
            # would otherwise leave a stray "2" behind.
            | gsub("@FEE_CURRENCY2@"; $fc2)
            | gsub("@FEE_CURRENCY@"; $fc)
            | gsub("@ERC20_DATA@"; $erc20)
            | gsub("@BALANCE_OF_DATA@"; $balanceof)
        else . end)' <<<"$1"
}

while IFS= read -r scenario; do
    name=$(jq -r '.name' <<<"$scenario")
    method=$(jq -r '.method' <<<"$scenario")
    params=$(substitute "$(jq -c '.params' <<<"$scenario")")
    filter=$(jq -r '.filter // "."' <<<"$scenario")
    # Scenarios name the shared filters rather than restating them, so the
    # per-step rendering cannot drift between the two structLog goldens.
    filter=${filter//@STRUCTLOG_FILTER@/$STRUCTLOG_FILTER}
    # The note is what tells whoever hits a mismatch whether the new output is a
    # regression or the point of their change, so hand it to the reporter
    # instead of leaving it readable only in the scenario file.
    note=$(jq -r '.note // ""' <<<"$scenario")
    rpc_golden "$name" "$method" "$params" "$filter" "$note"
done < <(jq -c '.[]' "$SCRIPT_DIR/rpc_golden_scenarios.json")

# op-geth binds the fee-currency key case-insensitively and clients rely on it.
# Asserted against the canonical-key estimate rather than pinned as a second
# golden: two files would state the equality only by coincidence, and would let
# one regression be blessed into both at once.
CIP64_ESTIMATE_REQ="\"from\": \"$ACC_ADDR\", \"to\": \"$DEAD\", \"value\": \"0x1\""
rpc_expect_same estimateGas_cip64_lowercase_key_matches_canonical \
    eth_estimateGas "[{$CIP64_ESTIMATE_REQ, \"feecurrency\": \"$FEE_CURRENCY\"}, \"0x0\"]" \
    eth_estimateGas "[{$CIP64_ESTIMATE_REQ, \"feeCurrency\": \"$FEE_CURRENCY\"}, \"0x0\"]"

# `eth_gasPrice` and `eth_maxPriorityFeePerGas` take no block argument; they are
# pinned here because no block has been mined yet, so the head is still genesis.
#
# Both genesis fee currencies are covered: this is the only read-only surface
# where the exchange rate itself is observable. The trace and call goldens all
# render a CIP-64 call as an ordinary call frame with no fee-currency data in
# it, so a wrong rate does not move any of them.
rpc_golden_json gas_price_at_genesis "$(jq -n \
    --arg native "$(rpc_call eth_gasPrice '[]' | jq -r '.result')" \
    --arg fc "$(rpc_call eth_gasPrice "[\"$FEE_CURRENCY\"]" | jq -r '.result')" \
    --arg fc2 "$(rpc_call eth_gasPrice "[\"$FEE_CURRENCY2\"]" | jq -r '.result')" \
    --arg tip "$(rpc_call eth_maxPriorityFeePerGas '[]' | jq -r '.result')" \
    --arg fc_tip "$(rpc_call eth_maxPriorityFeePerGas "[\"$FEE_CURRENCY\"]" | jq -r '.result')" \
    --arg fc2_tip "$(rpc_call eth_maxPriorityFeePerGas "[\"$FEE_CURRENCY2\"]" | jq -r '.result')" \
    '{gasPrice: $native, gasPriceInFeeCurrency: $fc,
      gasPriceInFeeCurrency2: $fc2,
      maxPriorityFeePerGas: $tip, maxPriorityFeePerGasInFeeCurrency: $fc_tip,
      maxPriorityFeePerGasInFeeCurrency2: $fc2_tip}')"

# ---------------------------------------------------------------------------
# Phase 2 — refusals. An error is the contract for each of these, and the exact
# message is part of it: clients branch on these strings.
# ---------------------------------------------------------------------------

echo ""
echo "Phase 2: rejected requests"

rpc_expect_error estimateGas_unregistered_fee_currency eth_estimateGas \
    "[{\"from\": \"$ACC_ADDR\", \"to\": \"$DEAD\", \"value\": \"0x1\", \"feeCurrency\": \"0x00000000000000000000000000000000000000ff\"}, \"0x0\"]" \
    'fee currency not registered'
# A fee currency implies EIP-1559 fee fields, so these two are refused by the
# generic gasPrice/1559 conflict check rather than by a Celo-specific message.
rpc_expect_error call_fee_currency_with_gas_price eth_call \
    "[{\"from\": \"$ACC_ADDR\", \"to\": \"$DEAD\", \"value\": \"0x1\", \"gasPrice\": \"0x1\", \"feeCurrency\": \"$FEE_CURRENCY\"}, \"0x0\"]" \
    'both gasPrice and'
rpc_expect_error traceCall_fee_currency_with_gas_price debug_traceCall \
    "[{\"from\": \"$ACC_ADDR\", \"to\": \"$DEAD\", \"value\": \"0x1\", \"gasPrice\": \"0x1\", \"feeCurrency\": \"$FEE_CURRENCY\"}, \"0x0\", {\"tracer\": \"callTracer\"}]" \
    'both gasPrice and'
rpc_expect_error simulateV1_fee_currency_with_authorization_list eth_simulateV1 \
    "[{\"blockStateCalls\": [{\"calls\": [{\"from\": \"$ACC_ADDR\", \"to\": \"$DEAD\", \"value\": \"0x1\", \"feeCurrency\": \"$FEE_CURRENCY\", \"authorizationList\": []}]}]}, \"0x0\"]" \
    'feeCurrency is not compatible with EIP-7702'
rpc_expect_error traceCall_unknown_tracer debug_traceCall \
    "[{\"from\": \"$ACC_ADDR\", \"to\": \"$DEAD\", \"value\": \"0x1\"}, \"0x0\", {\"tracer\": \"noSuchTracer\"}]" \
    'JS Tracer is not enabled'

# ---------------------------------------------------------------------------
# Phase 3 — one transaction per block
# ---------------------------------------------------------------------------

echo ""
echo "Phase 3: mined transactions, one per block"

# Nonce 0..3 from a fresh chain. Fee fields are pinned so the raw bytes — and
# therefore the hashes embedded in the goldens — are reproducible.
TX_CIP64=$(sign_tx --nonce 0 --to "$DEAD" --value 1 --fee-currency "$FEE_CURRENCY" \
    --gas 100000 --max-fee 100000000000 --max-priority-fee 2000000000)
TX_ERC20=$(sign_tx --nonce 1 --to "$TOKEN_ADDR" --data "$ERC20_DATA" --gas 100000)
TX_PLAIN=$(sign_tx --nonce 2 --to "$DEAD" --value 1 --gas 21000)
# CIP-64 whose main frame reverts: the fee is still debited and credited, so the
# receipt carries a failed status alongside both fee-flow logs.
TX_CIP64_REVERT=$(sign_tx --nonce 3 --to "$TOKEN_ADDR" --data "$ERC20_OVERDRAFT_DATA" \
    --fee-currency "$FEE_CURRENCY" --gas 200000 \
    --max-fee 100000000000 --max-priority-fee 2000000000)

LABELS=(cip64 erc20 plain cip64_reverted)
TXS=("$TX_CIP64" "$TX_ERC20" "$TX_PLAIN" "$TX_CIP64_REVERT")
CIP64_BLOCK=

for i in "${!LABELS[@]}"; do
    label=${LABELS[$i]}
    hash=$(jq -r '.hash' <<<"${TXS[$i]}")
    raw=$(jq -r '.raw' <<<"${TXS[$i]}")

    if [[ "$(send_raw "$label" "$raw")" != "$hash" ]]; then
        continue
    fi
    if ! receipt=$(wait_receipt "$hash"); then
        _rpc_fail "receipt_$label" "no receipt within the timeout"
        continue
    fi

    block=$(jq -r '.blockNumber' <<<"$receipt")
    miner=$(rpc_call eth_getBlockByNumber "[\"$block\", false]" | jq -r '.result.miner')
    # Re-supplied before each consumer: rpc_golden clears RPC_JQ_ARGS as it
    # takes it, so the value cannot survive into a later block's goldens.
    miner_args=(--arg minerTopic "0x000000000000000000000000${miner#0x}")

    filter="$RECEIPT_FILTER"
    [[ "$label" == cip64* ]] && filter="$RECEIPT_FILTER | $CREDIT_TOPIC_FILTER"
    RPC_JQ_ARGS=("${miner_args[@]}")
    rpc_golden "receipt_$label" eth_getTransactionReceipt "[\"$hash\"]" "$filter"
    rpc_golden "tx_$label" eth_getTransactionByHash "[\"$hash\"]" \
        'del(.blockHash, .blockTimestamp)'
    rpc_golden "traceTransaction_$label" debug_traceTransaction \
        "[\"$hash\", {\"tracer\": \"callTracer\"}]"
    rpc_golden "traceBlock_$label" debug_traceBlockByNumber \
        "[\"$block\", {\"tracer\": \"callTracer\"}]"
    rpc_golden "otsTrace_$label" ots_traceTransaction "[\"$hash\"]"
    rpc_golden "parityTrace_$label" trace_transaction "[\"$hash\"]" \
        "map($TRACE_TX_FILTER)"
    RPC_JQ_ARGS=("${miner_args[@]}")
    rpc_golden "blockReceipts_$label" eth_getBlockReceipts "[\"$block\"]" \
        "map($filter)"

    # A canonical block re-submitted as raw RLP must trace identically to the
    # block itself. This is the only path that reaches `debug_traceBlock`.
    # Asserted against the block trace rather than pinned separately: the golden
    # above already fixes the value, and two goldens would let one regression be
    # blessed into both.
    raw_block=$(rpc_call debug_getRawBlock "[\"$block\"]" | jq -r '.result')
    rpc_expect_same "traceRawBlock_matches_traceBlock_$label" \
        debug_traceBlock "[\"$raw_block\", {\"tracer\": \"callTracer\"}]" \
        debug_traceBlockByNumber "[\"$block\", {\"tracer\": \"callTracer\"}]"

    if [[ "$label" == cip64 ]]; then
        CIP64_BLOCK=$block
        # Structural invariants that the goldens deliberately normalize away.
        credit_recipient=$(jq -r '[.logs[] | select(.address == ($fc | ascii_downcase))]
            | last | .topics[2]' --arg fc "$FEE_CURRENCY" <<<"$receipt")
        rpc_expect_eq cip64_fee_credit_goes_to_block_fee_recipient \
            "0x${credit_recipient: -40}" "$(tr '[:upper:]' '[:lower:]' <<<"$miner")"
        # `effectiveGasPrice` on a CIP-64 receipt is recomputed from the
        # fee-currency-denominated base fee, not the native one.
        base_fee_fc=$(jq -r '.baseFee' <<<"$receipt")
        rpc_expect_eq cip64_effective_gas_price_from_fee_currency_base_fee \
            "$(jq -r '.effectiveGasPrice' <<<"$receipt")" \
            "$(cast to-hex $(( $(cast to-dec "$base_fee_fc") + 2000000000 )))"
    fi
    if [[ "$label" == cip64_reverted ]]; then
        rpc_expect_eq cip64_reverted_status "$(jq -r '.status' <<<"$receipt")" "0x0"
        rpc_expect_eq cip64_reverted_still_pays_fees \
            "$(jq --arg fc "$FEE_CURRENCY" \
                '[.logs[] | select(.address == ($fc | ascii_downcase))] | length' \
                <<<"$receipt")" "2"
    fi
done

# `eth_feeHistory`'s Celo override converts a CIP-64 tip back to its native
# equivalent before computing percentiles; with a single transaction per block
# every percentile is that one tip.
if [[ -n "$CIP64_BLOCK" ]]; then
    rpc_golden feeHistory_over_cip64_block eth_feeHistory \
        "[\"0x1\", \"$CIP64_BLOCK\", [25, 50, 75]]" "$FEE_HISTORY_FILTER"
    rpc_golden logs_of_cip64_block eth_getLogs \
        "[{\"fromBlock\": \"$CIP64_BLOCK\", \"toBlock\": \"$CIP64_BLOCK\", \"address\": \"$FEE_CURRENCY\"}]" \
        'map(del(.blockHash, .blockTimestamp, .topics[2]))'
fi

# ---------------------------------------------------------------------------
# Phase 4 — several CIP-64 transactions in one block
#
# Submitted behind a nonce gap so they stay queued (no block is built for a
# queued transaction) until the gap-filling transaction promotes all of them at
# once. Tracing the last one replays a prefix containing the block's deposit
# transaction and three CIP-64 transactions — the path where replaying CIP-64
# receipt data twice used to panic, and where a fee context re-read mid-block
# would price the later transactions at the wrong rate.
# ---------------------------------------------------------------------------

echo ""
echo "Phase 4: several CIP-64 transactions in one block"

NONCE=$(( $(rpc_call eth_getTransactionCount "[\"$ACC_ADDR\", \"latest\"]" | jq -r '.result') ))
BATCH_HASHES=()
for i in 1 2 3; do
    tx=$(sign_tx --nonce $((NONCE + i)) --to "$DEAD" --value "$i" \
        --fee-currency "$FEE_CURRENCY" --gas 100000 \
        --max-fee 100000000000 --max-priority-fee 2000000000)
    BATCH_HASHES+=("$(jq -r '.hash' <<<"$tx")")
    send_raw "batch_$i" "$(jq -r '.raw' <<<"$tx")" >/dev/null
done
# The gap filler pays in the *other* genesis fee currency, so the replayed
# prefix spans two currencies at two different exchange rates. Its fee fields
# are three orders of magnitude smaller than the first currency's: one unit of
# it is worth ~515 wei at the genesis rate, so reusing the same numbers would
# blow the per-transaction fee cap.
filler=$(sign_tx --nonce "$NONCE" --to "$DEAD" --value 1 --fee-currency "$FEE_CURRENCY2" \
    --gas 100000 --max-fee 100000000 --max-priority-fee 1000000)
FILLER_HASH=$(jq -r '.hash' <<<"$filler")
send_raw filler "$(jq -r '.raw' <<<"$filler")" >/dev/null

if ! batch_receipt=$(wait_receipt "${BATCH_HASHES[2]}"); then
    _rpc_fail "batch_mined" "the nonce-gap batch was not mined"
else
    BATCH_BLOCK=$(jq -r '.blockNumber' <<<"$batch_receipt")
    # The point of the batch is the shared block; if it split, every assertion
    # below stops testing what it claims to. Fail rather than warn.
    same_block=yes
    for h in "$FILLER_HASH" "${BATCH_HASHES[@]}"; do
        rcpt=$(rpc_call eth_getTransactionReceipt "[\"$h\"]" | jq -c '.result')
        [[ "$(jq -r '.blockNumber' <<<"$rcpt")" == "$BATCH_BLOCK" ]] || same_block=no
    done
    rpc_expect_eq batch_shares_one_block "$same_block" "yes"
fi

# Everything below is pinned against the four-transaction block. A split batch
# is a promotion race, not a regression in any of these surfaces, so report it
# once above and skip rather than emit eight unexplained golden diffs.
if [[ "${same_block:-no}" == yes ]]; then
    # Index 0 is the block's L1-attributes deposit transaction, so the last
    # CIP-64 transaction sits at index 4 behind three CIP-64 predecessors.
    rpc_expect_eq batch_last_tx_index \
        "$(jq -r '.transactionIndex' <<<"$batch_receipt")" "0x4"

    miner=$(rpc_call eth_getBlockByNumber "[\"$BATCH_BLOCK\", false]" | jq -r '.result.miner')
    RPC_JQ_ARGS=(--arg minerTopic "0x000000000000000000000000${miner#0x}")
    rpc_golden receipt_cip64_last_of_block eth_getTransactionReceipt \
        "[\"${BATCH_HASHES[2]}\"]" "$RECEIPT_FILTER | $CREDIT_TOPIC_FILTER"
    rpc_golden traceBlock_cip64_batch debug_traceBlockByNumber \
        "[\"$BATCH_BLOCK\", {\"tracer\": \"callTracer\"}]"

    # The prefix-replay path, on the transaction with the longest prefix.
    rpc_golden traceTransaction_cip64_last_of_block debug_traceTransaction \
        "[\"${BATCH_HASHES[2]}\", {\"tracer\": \"callTracer\"}]"
    rpc_golden traceTransaction_cip64_last_of_block_structLogs debug_traceTransaction \
        "[\"${BATCH_HASHES[2]}\", {\"disableStorage\": true}]" "$STRUCTLOG_FILTER"
    rpc_golden otsTrace_cip64_last_of_block ots_traceTransaction "[\"${BATCH_HASHES[2]}\"]"
    rpc_golden parityTrace_cip64_last_of_block trace_transaction \
        "[\"${BATCH_HASHES[2]}\"]" "map($TRACE_TX_FILTER)"
    rpc_golden replayBlock_cip64_batch trace_replayBlockTransactions \
        "[\"$BATCH_BLOCK\", [\"trace\"]]"

    # A mid-block simulation must see the fee-currency context as it stood at
    # the start of the block, exactly as a transaction at that index would. The
    # callTracer frame carries no fee-currency data, so these two pin that
    # seeding the context at a mid-block index works at all — a context taken
    # from the wrong point that still executes would not move them.
    rpc_golden traceCall_at_midblock_index debug_traceCall \
        "[{\"from\": \"$ACC_ADDR\", \"to\": \"$DEAD\", \"value\": \"0x1\", \"gas\": \"0x30d40\", \"feeCurrency\": \"$FEE_CURRENCY\", \"maxFeePerGas\": \"0xba43b7400\", \"maxPriorityFeePerGas\": \"0x64\"}, \"$BATCH_BLOCK\", {\"tracer\": \"callTracer\", \"txIndex\": \"0x3\"}]"
    # An unrelated state override must not disable context seeding.
    rpc_golden traceCall_at_midblock_index_with_override debug_traceCall \
        "[{\"from\": \"$ACC_ADDR\", \"to\": \"$DEAD\", \"value\": \"0x1\", \"gas\": \"0x30d40\", \"feeCurrency\": \"$FEE_CURRENCY\", \"maxFeePerGas\": \"0xba43b7400\", \"maxPriorityFeePerGas\": \"0x64\"}, \"$BATCH_BLOCK\", {\"tracer\": \"callTracer\", \"txIndex\": \"0x3\", \"stateOverrides\": {\"0x00000000000000000000000000000000000000d0\": {\"balance\": \"0x1\"}}}]"
fi

# ---------------------------------------------------------------------------
# Phase 5 — refusals that only the transaction pool can produce
#
# These use nonces above the account's next one, so a regression that wrongly
# *accepts* one leaves it queued instead of corrupting the blocks above.
# ---------------------------------------------------------------------------

echo ""
echo "Phase 5: rejected transactions"

NEXT_NONCE=$(( $(rpc_call eth_getTransactionCount "[\"$ACC_ADDR\", \"latest\"]" | jq -r '.result') + 100 ))

# One unit of the second fee currency is worth ~515 wei at the genesis rate, so
# fee fields sized for the first currency convert to a native-equivalent fee
# above the per-transaction cap.
over_cap=$(sign_tx --nonce "$NEXT_NONCE" --to "$DEAD" --value 1 \
    --fee-currency "$FEE_CURRENCY2" --gas 100000 \
    --max-fee 100000000000 --max-priority-fee 2000000000)
rpc_expect_error send_cip64_over_fee_cap eth_sendRawTransaction \
    "[$(jq '.raw' <<<"$over_cap")]" 'exceeds the configured cap'

# The minimum-tip check, in fee-currency units. The message is pinned because an
# unpinned rejection is satisfied by *any* error: this check previously carried
# no regex and was named `send_cip64_below_base_fee`, which is not what it
# tests — the tip comparison fires first and the transaction never reaches the
# base-fee-floor comparison.
#
# There is deliberately no companion case for that floor: `CeloPoolBuilder`
# short-circuits `base_fee_floor_fn` to 0 under --dev
# (crates/celo-reth/src/node.rs:268-270), so `BelowBaseFeeFloor` is unreachable
# from any dev-mode e2e test, and a transaction under-pricing the *current* base
# fee is parked in the base-fee sub-pool rather than refused. That path is
# covered by the pool unit tests, not here.
below_min_tip=$(sign_tx --nonce $((NEXT_NONCE + 1)) --to "$DEAD" --value 1 \
    --fee-currency "$FEE_CURRENCY" --gas 100000 --max-fee 1000 --max-priority-fee 1)
rpc_expect_error send_cip64_below_min_tip eth_sendRawTransaction \
    "[$(jq '.raw' <<<"$below_min_tip")]" 'priority fee 1 below minimum'

# A fee currency that is not in the directory has no rate to price against.
# Note the pool and the call paths word this differently ("unregistered
# fee-currency address" here vs. "fee currency not registered" from
# eth_estimateGas above); both are pinned so a change to either is visible.
unregistered=$(sign_tx --nonce $((NEXT_NONCE + 2)) --to "$DEAD" --value 1 \
    --fee-currency 0x00000000000000000000000000000000000000ff --gas 100000 \
    --max-fee 100000000000 --max-priority-fee 2000000000)
rpc_expect_error send_cip64_unregistered_currency eth_sendRawTransaction \
    "[$(jq '.raw' <<<"$unregistered")]" 'unregistered fee-currency address'

# ---------------------------------------------------------------------------
# Phase 6 — eth_feeHistory across an exchange-rate change
#
# The Celo `eth_feeHistory` override converts each CIP-64 tip from fee-currency
# units back to native before computing percentiles, and it looks the rate up at
# the block whose transactions it is normalizing rather than at the head. That
# distinction is deliberate — the rate cache is keyed by `(block_number,
# fee_currency)` for exactly this reason — but the unit tests only cover the two
# pure halves (converting a tip *given* a rate, and the gas-weighted percentile
# math), not which block's rate gets passed in.
#
# So: one CIP-64 transaction at the current rate, then a rate change, then an
# identical CIP-64 transaction, then one fee history spanning all three blocks.
# Normalizing against the head rate would price the first block's tip at the new
# rate and both rewards would come out equal.
#
# This phase must stay last: it leaves the fee currency's rate changed.
# ---------------------------------------------------------------------------

echo ""
echo "Phase 6: eth_feeHistory across an exchange-rate change"

# `0x…ce16`'s oracle in the dev genesis alloc. Its `setExchangeRate` is ungated,
# so no deploy and no ownership dance is needed — the rate change is one
# ordinary transaction to a fixed address.
FEE_CURRENCY_ORACLE=0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb0001
# 2:1 at genesis -> 4:1 here, so a tip converted at the wrong rate is off by 2x.
RATE_CHANGE_DATA=$(cast calldata 'setExchangeRate(address,uint256,uint256)' \
    "$FEE_CURRENCY" 4000000000000000000 1000000000000000000)

RATE_NONCE=$(( $(rpc_call eth_getTransactionCount "[\"$ACC_ADDR\", \"latest\"]" | jq -r '.result') ))
cip64_before=$(sign_tx --nonce "$RATE_NONCE" --to "$DEAD" --value 1 \
    --fee-currency "$FEE_CURRENCY" --gas 100000 \
    --max-fee 100000000000 --max-priority-fee 2000000000)
rate_change=$(sign_tx --nonce $((RATE_NONCE + 1)) --to "$FEE_CURRENCY_ORACLE" \
    --data "$RATE_CHANGE_DATA" --gas 200000)
cip64_after=$(sign_tx --nonce $((RATE_NONCE + 2)) --to "$DEAD" --value 1 \
    --fee-currency "$FEE_CURRENCY" --gas 100000 \
    --max-fee 100000000000 --max-priority-fee 2000000000)

before_block= ; after_block=
for tx in "$cip64_before" "$rate_change" "$cip64_after"; do
    hash=$(jq -r '.hash' <<<"$tx")
    if [[ "$(send_raw rate_sequence "$(jq -r '.raw' <<<"$tx")")" != "$hash" ]]; then
        break
    fi
    if ! receipt=$(wait_receipt "$hash"); then
        _rpc_fail "rate_sequence_mined" "no receipt for $hash"
        break
    fi
    [[ -z "$before_block" ]] && before_block=$(jq -r '.blockNumber' <<<"$receipt")
    after_block=$(jq -r '.blockNumber' <<<"$receipt")
done

if [[ -n "$before_block" && "$before_block" != "$after_block" ]]; then
    # Checked directly so a silently failed rate change reports itself here
    # rather than as a confusing arithmetic mismatch below.
    rpc_expect_eq rate_change_applied \
        "$(cast call "$FEE_CURRENCY_DIRECTORY_ADDR" \
            'getExchangeRate(address)(uint256,uint256)' "$FEE_CURRENCY" \
            --rpc-url "$RPC_URL" | head -1 | cut -d' ' -f1)" \
        "4000000000000000000"

    rpc_golden feeHistory_across_rate_change eth_feeHistory \
        "[\"0x3\", \"$after_block\", [50]]" "$FEE_HISTORY_FILTER"

    # The discriminating property, stated rather than left implicit in the hex:
    # the same fee-currency tip is worth twice as much native before the change
    # as after it. Head-rate normalization would make these equal.
    rewards=$(rpc_call eth_feeHistory "[\"0x3\", \"$after_block\", [50]]" | jq -r '.result.reward')
    rpc_expect_eq feeHistory_tip_halves_across_rate_change \
        "$(( $(jq -r '.[0][0]' <<<"$rewards" | xargs cast to-dec) ))" \
        "$(( $(jq -r '.[2][0]' <<<"$rewards" | xargs cast to-dec) * 2 ))"
else
    _rpc_fail rate_change_sequence "the rate-change sequence did not mine into distinct blocks"
fi

# ---------------------------------------------------------------------------

echo ""
if ! cast block-number --rpc-url "$RPC_URL" &>/dev/null; then
    _rpc_fail node_still_answering "the node stopped responding during the run"
else
    _rpc_pass node_still_answering
fi

# Only on an otherwise green run: a phase that failed above may have skipped its
# goldens, and reporting those as orphans would bury the real failure.
if [[ $RPC_GOLDEN_FAILED -eq 0 ]]; then
    rpc_golden_check_orphans
fi

rpc_golden_summary
