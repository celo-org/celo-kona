#!/bin/bash
# rpc_assert.sh — golden-file assertion helpers for celo-reth's JSON-RPC surface.
#
# A "golden" check fires one JSON-RPC request at $RPC_URL, normalizes the
# response with jq (object keys sorted, plus an optional per-scenario filter
# that drops values which legitimately vary between runs) and compares it
# byte-for-byte against a committed expectation under $RPC_GOLDEN_DIR.
#
# Exact-output comparison is the point. A trace that still *succeeds* but
# reports the wrong gas, the wrong fee-currency amount or a differently shaped
# call frame is precisely the regression class that liveness assertions
# ("no error field, and `from` matches") cannot see.
#
# Sourced usage:
#     RPC_URL=http://127.0.0.1:8545 \
#     RPC_GOLDEN_DIR=$SCRIPT_DIR/rpc_golden \
#     source rpc_assert.sh
#
#     rpc_golden <name> <method> <params-json> [jq-filter]
#     rpc_golden_json <name> <json-value>
#     rpc_expect_ok <name> <method> <params-json>
#     rpc_expect_error <name> <method> <params-json> [message-regex]
#     rpc_expect_eq <name> <actual> <expected>
#     rpc_expect_same <name> <method-a> <params-a> <method-b> <params-b> [filter]
#     rpc_golden_check_orphans  # fails if a committed golden went unread
#     rpc_golden_summary        # prints totals; returns 1 on a failure or a bless
#
# Extra jq arguments (for filters that need a runtime value, e.g. the block's
# randomized dev fee recipient) are taken from the RPC_JQ_ARGS array. They apply
# to the *next* check only and are cleared as it consumes them, so a forgotten
# reset cannot leak into an unrelated golden:
#     RPC_JQ_ARGS=(--arg miner "$miner")
#     rpc_golden ... '.logs |= map(...)'
# Leaking would fail *open* rather than closed — a stale --arg minerTopic makes
# the credit-topic filter rewrite a topic using a previous block's fee
# recipient, and the golden still compares equal. A filter that references an
# argument nobody supplied fails loudly instead, as a jq error.
#
# Regenerating goldens after an intentional output change:
#     BLESS=1 e2e_test/test_rpc_golden.sh   # exits non-zero: it asserts nothing
# then read the resulting `git diff` on e2e_test/rpc_golden/. A golden that
# moves without a matching intentional change is the regression this harness
# exists to catch — never bless a diff you cannot explain.
#
# A missing golden is a failure, not an auto-create: silently writing one on the
# first run would make a brand-new scenario pass unconditionally.
#
# Note on ordering: `jq -S` sorts *object keys* only. Array order is preserved
# and is semantic here (`logs`, `structLogs`, `calls`, per-tx trace lists), so a
# reordering regression stays visible.

# Required, not defaulted: guessing `$SCRIPT_DIR/rpc_golden` would make this
# file depend on a variable it neither sets nor documents, and a wrong guess
# surfaces as "no golden at ..." for every check rather than as a setup error.
if [[ -z "${RPC_GOLDEN_DIR:-}" ]]; then
    echo "rpc_assert: RPC_GOLDEN_DIR must be set before sourcing rpc_assert.sh" >&2
    # shellcheck disable=SC2317  # the exit is the fallback when not sourced
    return 1 2>/dev/null || exit 1
fi

RPC_GOLDEN_PASSED=0
RPC_GOLDEN_FAILED=0
RPC_GOLDEN_BLESSED=0
RPC_GOLDEN_VISITED=()
RPC_JQ_ARGS=()

rpc_call() { # <method> <params-json> -> full JSON-RPC response on stdout
    local method=$1 params=$2
    curl -sS --max-time 60 -H 'Content-Type: application/json' \
        --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$method\",\"params\":$params}" \
        "$RPC_URL"
}

_rpc_color() { tput setaf "$1" 2>/dev/null || true; }
_rpc_reset() { tput sgr0 2>/dev/null || true; }

_rpc_pass() {
    RPC_GOLDEN_PASSED=$((RPC_GOLDEN_PASSED + 1))
    _rpc_color 2; echo "  ok    $1"; _rpc_reset
}

_rpc_fail() {
    RPC_GOLDEN_FAILED=$((RPC_GOLDEN_FAILED + 1))
    _rpc_color 1; echo "  FAIL  $1: $2"; _rpc_reset
}

_rpc_blessed() {
    RPC_GOLDEN_BLESSED=$((RPC_GOLDEN_BLESSED + 1))
    _rpc_color 3; echo "  bless $1"; _rpc_reset
}

# An HTTP error page or a truncated body is neither a result nor a JSON-RPC
# error; classify it as such instead of letting `has("error")` report "no error".
_rpc_is_json() { jq -e . >/dev/null 2>&1 <<<"$1"; }

_rpc_has_error() { jq -e 'has("error")' >/dev/null 2>&1 <<<"$1"; }

_rpc_error_msg() {
    jq -r '.error | "\(.code // "?"): \(.message // "<no message>")"' <<<"$1" 2>/dev/null
}

# Sorted, filtered, pretty-printed `.result`. Pretty-printing keeps the committed
# goldens reviewable line-by-line in a pull request rather than as one long line.
_rpc_normalize() { # <response> <jq-filter> [jq-args...]
    local response=$1 filter=$2
    shift 2
    jq -S --indent 2 "$@" ".result | ($filter)" <<<"$response"
}

# Take RPC_JQ_ARGS for the check that is starting and clear it, so the args
# cannot survive into the next one. Callers use the `_rpc_jq_args` array.
_rpc_take_jq_args() {
    _rpc_jq_args=("${RPC_JQ_ARGS[@]}")
    RPC_JQ_ARGS=()
}

# Compare $2 against the committed golden for $1, or (re)write it under BLESS=1.
_rpc_compare_golden() { # <name> <normalized-json>
    local name=$1 normalized=$2
    local golden="$RPC_GOLDEN_DIR/$name.json"
    RPC_GOLDEN_VISITED+=("$name")

    if [[ "${BLESS:-}" == "1" ]]; then
        mkdir -p "$RPC_GOLDEN_DIR"
        printf '%s\n' "$normalized" >"$golden"
        _rpc_blessed "$name"
        return 0
    fi
    if [[ ! -f "$golden" ]]; then
        _rpc_fail "$name" "no golden at ${golden##*/} (create it with BLESS=1)"
        return 0
    fi

    local diff_output
    if diff_output=$(diff -u --label "golden/$name" "$golden" \
        --label "actual/$name" <(printf '%s\n' "$normalized")); then
        _rpc_pass "$name"
    else
        _rpc_fail "$name" "output differs from the committed golden"
        local lines
        lines=$(wc -l <<<"$diff_output")
        head -60 <<<"$diff_output" | sed 's/^/        /'
        if [[ $lines -gt 60 ]]; then
            echo "        ... diff truncated ($lines lines total)"
        fi
    fi
    return 0
}

# rpc_golden <name> <method> <params-json> [jq-filter]
# Never aborts the calling script; failures are counted and reported by
# rpc_golden_summary.
rpc_golden() {
    local name=$1 method=$2 params=$3 filter=${4:-.}
    local response normalized _rpc_jq_args=()
    _rpc_take_jq_args

    if ! response=$(rpc_call "$method" "$params"); then
        _rpc_fail "$name" "transport error talking to $RPC_URL (node down?)"
        return 0
    fi
    if ! _rpc_is_json "$response"; then
        _rpc_fail "$name" "response is not JSON: $(head -c 200 <<<"$response")"
        return 0
    fi
    if _rpc_has_error "$response"; then
        _rpc_fail "$name" "unexpected JSON-RPC error: $(_rpc_error_msg "$response")"
        return 0
    fi
    if ! normalized=$(_rpc_normalize "$response" "$filter" "${_rpc_jq_args[@]}" 2>&1); then
        _rpc_fail "$name" "jq filter failed: $normalized"
        return 0
    fi
    _rpc_compare_golden "$name" "$normalized"
}

# rpc_golden_json <name> <json-value>
# Same comparison against a value the caller assembled itself — an aggregate
# over several calls, a derived expectation — rather than one RPC response.
rpc_golden_json() {
    local name=$1 value=$2
    local normalized _rpc_jq_args=()
    _rpc_take_jq_args
    if ! normalized=$(jq -S --indent 2 "${_rpc_jq_args[@]}" . <<<"$value" 2>&1); then
        _rpc_fail "$name" "value is not valid JSON: $normalized"
        return 0
    fi
    _rpc_compare_golden "$name" "$normalized"
}

# rpc_expect_ok <name> <method> <params-json>
# Asserts the call returns a result. For surfaces whose response shape is not
# stable enough to pin, but where "the endpoint is registered and does not
# error" is still worth guarding.
rpc_expect_ok() {
    local name=$1 method=$2 params=$3
    local response
    if ! response=$(rpc_call "$method" "$params"); then
        _rpc_fail "$name" "transport error talking to $RPC_URL"
        return 0
    fi
    if ! _rpc_is_json "$response"; then
        _rpc_fail "$name" "response is not JSON: $(head -c 200 <<<"$response")"
        return 0
    fi
    if _rpc_has_error "$response"; then
        _rpc_fail "$name" "unexpected JSON-RPC error: $(_rpc_error_msg "$response")"
        return 0
    fi
    _rpc_pass "$name"
    return 0
}

# rpc_expect_error <name> <method> <params-json> [message-regex]
# Asserts the call is rejected with a *clean* JSON-RPC error: the node answers
# and refuses, rather than panicking or silently returning a wrong result.
rpc_expect_error() {
    local name=$1 method=$2 params=$3 regex=${4:-}
    local response message
    if ! response=$(rpc_call "$method" "$params"); then
        _rpc_fail "$name" "transport error (node dead instead of a clean RPC error?)"
        return 0
    fi
    if ! _rpc_is_json "$response"; then
        _rpc_fail "$name" "response is not JSON: $(head -c 200 <<<"$response")"
        return 0
    fi
    if ! _rpc_has_error "$response"; then
        _rpc_fail "$name" "expected a JSON-RPC error, but the call succeeded"
        return 0
    fi
    message=$(_rpc_error_msg "$response")
    if [[ -n "$regex" ]] && ! grep -qE "$regex" <<<"$message"; then
        _rpc_fail "$name" "error '$message' does not match /$regex/"
        return 0
    fi
    _rpc_pass "$name ($message)"
    return 0
}

# rpc_expect_same <name> <method-a> <params-a> <method-b> <params-b> [jq-filter]
# Asserts two calls normalize to the same value. For invariants of the form
# "these two paths must agree": pinning both as separate goldens states the
# invariant only by coincidence, and lets a shared regression be blessed into
# both files at once. One of the two paths still needs its own golden to pin
# what the agreed-upon value *is*.
rpc_expect_same() {
    local name=$1 method_a=$2 params_a=$3 method_b=$4 params_b=$5 filter=${6:-.}
    local response normalized normalized_a normalized_b method params side
    local _rpc_jq_args=()
    _rpc_take_jq_args

    for side in a b; do
        if [[ $side == a ]]; then
            method=$method_a params=$params_a
        else
            method=$method_b params=$params_b
        fi
        if ! response=$(rpc_call "$method" "$params"); then
            _rpc_fail "$name" "transport error talking to $RPC_URL (node down?)"
            return 0
        fi
        if ! _rpc_is_json "$response"; then
            _rpc_fail "$name" "$method: response is not JSON: $(head -c 200 <<<"$response")"
            return 0
        fi
        if _rpc_has_error "$response"; then
            _rpc_fail "$name" "$method: unexpected JSON-RPC error: $(_rpc_error_msg "$response")"
            return 0
        fi
        if ! normalized=$(_rpc_normalize "$response" "$filter" "${_rpc_jq_args[@]}" 2>&1); then
            _rpc_fail "$name" "$method: jq filter failed: $normalized"
            return 0
        fi
        if [[ $side == a ]]; then
            normalized_a=$normalized
        else
            normalized_b=$normalized
        fi
    done

    if [[ "$normalized_a" == "$normalized_b" ]]; then
        _rpc_pass "$name"
    else
        _rpc_fail "$name" "$method_a and $method_b disagree"
        diff -u --label "$method_a" <(printf '%s\n' "$normalized_a") \
            --label "$method_b" <(printf '%s\n' "$normalized_b") |
            head -40 | sed 's/^/        /'
    fi
    return 0
}

# rpc_expect_eq <name> <actual> <expected>
rpc_expect_eq() {
    local name=$1 actual=$2 expected=$3
    if [[ "$actual" == "$expected" ]]; then
        _rpc_pass "$name ($actual)"
    else
        _rpc_fail "$name" "expected '$expected', got '$actual'"
    fi
    return 0
}

# rpc_golden_check_orphans
# Reports goldens that no check looked at. Renaming or deleting a scenario
# otherwise leaves its file behind forever, and a stale golden reads exactly
# like a covered case. Only meaningful after a complete run — call it once, at
# the end, and not when the run bailed out early.
rpc_golden_check_orphans() {
    local golden name orphans=()
    for golden in "$RPC_GOLDEN_DIR"/*.json; do
        [[ -f "$golden" ]] || continue
        name=${golden##*/}
        name=${name%.json}
        # shellcheck disable=SC2076  # literal match is the point
        [[ " ${RPC_GOLDEN_VISITED[*]} " == *" $name "* ]] || orphans+=("$name")
    done
    if [[ ${#orphans[@]} -gt 0 ]]; then
        _rpc_fail golden_files_all_used \
            "no check reads: ${orphans[*]} (delete them, or the scenario was renamed)"
    else
        _rpc_pass golden_files_all_used
    fi
}

rpc_golden_summary() {
    local total=$((RPC_GOLDEN_PASSED + RPC_GOLDEN_FAILED))
    echo ""
    echo "rpc_assert: $RPC_GOLDEN_PASSED/$total checks passed"
    # A bless run asserts nothing about the goldens it just overwrote, so it must
    # not be able to report success — otherwise a stray BLESS in the environment
    # turns the whole suite into a no-op that CI reads as green.
    if [[ $RPC_GOLDEN_BLESSED -gt 0 ]]; then
        echo "rpc_assert: $RPC_GOLDEN_BLESSED goldens (re)written in $RPC_GOLDEN_DIR"
        echo "            review 'git diff -- e2e_test/rpc_golden', then re-run without BLESS"
        return 1
    fi
    [[ $RPC_GOLDEN_FAILED -eq 0 ]]
}
