#!/bin/bash
#shellcheck disable=SC2086
set -eo pipefail

source shared.sh

# Native CELO is expressed only by omitting `feeCurrency`. The zero address is looked up like
# any other fee currency and fails as unregistered, the verdict op-geth has always reached, so
# a celo-reth sequencer must never admit or build a CIP-64 tx carrying it: op-geth marks such
# a block invalid. The pool and the fee RPCs reject it here; the EVM half, which decides
# eth_call, eth_estimateGas and eth_createAccessList, follows in the stacked PR.

rpc_body() {
	curl -s -X POST -H 'Content-Type: application/json' --data "$1" "$ETH_RPC_URL"
}

# The pool reports any unregistered currency as `unregistered fee-currency address <addr>`.
assert_unregistered() {
	local what="$1" resp="$2"
	if [ "$(echo "$resp" | jq -r '.error // empty')" = "" ]; then
		echo "FAIL: $what did not return a JSON-RPC error: $resp"
		exit 1
	fi
	if ! echo "$resp" | grep -q 'unregistered fee-currency address'; then
		echo "FAIL: $what failed with an unexpected error: $resp"
		exit 1
	fi
}

# 1. A signed CIP-64 whose RLP carries twenty zero bytes as feeCurrency, built offline with
#    ACC_PRIVKEY for chain 1337, nonce 0 (spec, "worked example"). The Celo validator runs
#    before the inner nonce/balance validator, so the pool rejects it as unregistered
#    whatever the sender's nonce is, exactly as op-geth's pool does.
RAW_ZERO_FC_TX=0x7bf88282053980843b9aca00850ba43b740082520894dededededededededededededededededededede0180c094000000000000000000000000000000000000000001a00368eab33099f38298396666398920d71a5fbf7804afdeb7f099c0b1668fafb3a0564f7176f299b5a0a3a7ddea27415c923675413f89bdb916e7eca6905898c505
resp=$(rpc_body '{"jsonrpc":"2.0","id":1,"method":"eth_sendRawTransaction","params":["'$RAW_ZERO_FC_TX'"]}')
assert_unregistered "eth_sendRawTransaction with zero-address feeCurrency" "$resp"

# 2. eth_gasPrice takes a bare address parameter and looks it up in the FeeCurrencyDirectory,
#    so the zero address fails with the directory's revert like any other unregistered
#    currency.
resp=$(rpc_body '{"jsonrpc":"2.0","id":1,"method":"eth_gasPrice","params":["'$ZERO_ADDRESS'"]}')
if [ "$(echo "$resp" | jq -r '.error // empty')" = "" ]; then
	echo "FAIL: eth_gasPrice with the zero address was answered: $resp"
	exit 1
fi

# 3. Control: a registered currency still prices, so the failures above are about the zero
#    address, not about CIP-64 support.
resp=$(rpc_body '{"jsonrpc":"2.0","id":1,"method":"eth_gasPrice","params":["'$FEE_CURRENCY'"]}')
if [ "$(echo "$resp" | jq -r '.result // empty')" = "" ]; then
	echo "FAIL: eth_gasPrice with a registered currency failed: $resp"
	exit 1
fi

echo "PASS: zero-address feeCurrency is rejected by the pool and the fee RPCs"
