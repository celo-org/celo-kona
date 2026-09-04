#!/bin/bash
#shellcheck disable=SC2086
set -eo pipefail

source shared.sh

# Native CELO is expressed only by omitting `feeCurrency`. The zero address is looked up like
# any other fee currency and fails as unregistered on every layer, the verdict op-geth has
# always reached, so a celo-reth sequencer must never admit or build a CIP-64 tx carrying it:
# op-geth marks such a block invalid. The pool, the fee RPCs and the EVM each reject it with
# the failure they give any unregistered currency.

DEAD=0x00000000000000000000000000000000DeaDBeef

rpc_body() {
	curl -s -X POST -H 'Content-Type: application/json' --data "$1" "$ETH_RPC_URL"
}

# Fail unless the response is a JSON-RPC error whose body contains `expected`.
assert_error_contains() {
	local what="$1" resp="$2" expected="$3"
	if [ "$(echo "$resp" | jq -r '.error // empty')" = "" ]; then
		echo "FAIL: $what did not return a JSON-RPC error: $resp"
		exit 1
	fi
	if ! echo "$resp" | grep -q "$expected"; then
		echo "FAIL: $what failed with an unexpected error: $resp"
		exit 1
	fi
}

# The pool reports any unregistered currency as `unregistered fee-currency address <addr>`.
assert_unregistered() {
	assert_error_contains "$1" "$2" 'unregistered fee-currency address'
}

# 1. A signed CIP-64 whose RLP carries twenty zero bytes as feeCurrency, built offline with
#    ACC_PRIVKEY for chain 1337, nonce 0 (spec, "worked example"). The Celo validator runs
#    before the inner nonce/balance validator, so the pool rejects it as unregistered
#    whatever the sender's nonce is, exactly as op-geth's pool does.
RAW_ZERO_FC_TX=0x7bf88282053980843b9aca00850ba43b740082520894dededededededededededededededededededede0180c094000000000000000000000000000000000000000001a00368eab33099f38298396666398920d71a5fbf7804afdeb7f099c0b1668fafb3a0564f7176f299b5a0a3a7ddea27415c923675413f89bdb916e7eca6905898c505
resp=$(rpc_body '{"jsonrpc":"2.0","id":1,"method":"eth_sendRawTransaction","params":["'$RAW_ZERO_FC_TX'"]}')
assert_unregistered "eth_sendRawTransaction with zero-address feeCurrency" "$resp"

# 2. Simulation: eth_estimateGas, eth_call and eth_createAccessList run the request through
#    the EVM, whose validation rejects the zero address with celo-revm's
#    `fee currency not registered: <addr>` (FeeCurrencyError::NotRegistered) instead of
#    pricing the request as native. The text comes from the EVM, not from a rate lookup.
resp=$(rpc_body '{"jsonrpc":"2.0","id":1,"method":"eth_estimateGas","params":[{"from":"'$ACC_ADDR'","to":"'$DEAD'","value":"0x1","feeCurrency":"'$ZERO_ADDRESS'"}]}')
assert_error_contains "eth_estimateGas with zero-address feeCurrency" "$resp" 'fee currency not registered'

resp=$(rpc_body '{"jsonrpc":"2.0","id":1,"method":"eth_call","params":[{"from":"'$ACC_ADDR'","to":"'$DEAD'","feeCurrency":"'$ZERO_ADDRESS'"},"latest"]}')
assert_error_contains "eth_call with zero-address feeCurrency" "$resp" 'fee currency not registered'

resp=$(rpc_body '{"jsonrpc":"2.0","id":1,"method":"eth_createAccessList","params":[{"from":"'$ACC_ADDR'","to":"'$DEAD'","value":"0x1","feeCurrency":"'$ZERO_ADDRESS'"},"latest"]}')
assert_error_contains "eth_createAccessList with zero-address feeCurrency" "$resp" 'fee currency not registered'

# 3. eth_gasPrice takes a bare address parameter and looks it up in the FeeCurrencyDirectory,
#    so the zero address fails with the directory's revert like any other unregistered
#    currency.
resp=$(rpc_body '{"jsonrpc":"2.0","id":1,"method":"eth_gasPrice","params":["'$ZERO_ADDRESS'"]}')
if [ "$(echo "$resp" | jq -r '.error // empty')" = "" ]; then
	echo "FAIL: eth_gasPrice with the zero address was answered: $resp"
	exit 1
fi

# 4. Control: a registered currency still prices, so the failures above are about the zero
#    address, not about CIP-64 support.
resp=$(rpc_body '{"jsonrpc":"2.0","id":1,"method":"eth_gasPrice","params":["'$FEE_CURRENCY'"]}')
if [ "$(echo "$resp" | jq -r '.result // empty')" = "" ]; then
	echo "FAIL: eth_gasPrice with a registered currency failed: $resp"
	exit 1
fi

echo "PASS: zero-address feeCurrency is rejected everywhere"
