#!/bin/bash
#shellcheck disable=SC2086
set -eo pipefail

source shared.sh

# A transaction request that names a field twice is ambiguous: a proxy and the node could read
# different values from one body, and for `feeCurrency` that picks which ERC20 pays for gas.
# celo-reth rejects such requests (celo-org/celo-kona#337) instead of keeping one of the values.
# This guards the RPC path end to end; the parsing rules themselves are unit-tested in
# celo-alloy-rpc-types. Parsing fails before any currency lookup, so no fee currency is deployed.

to=0x00000000000000000000000000000000DeaDBeef
other_currency=0x0000000000000000000000000000000000000001

expect_duplicate_rejected() {
	local name=$1 tx=$2
	local body resp detail
	body="{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_estimateGas\",\"params\":[$tx]}"
	resp=$(curl -s -X POST -H 'Content-Type: application/json' --data "$body" "$ETH_RPC_URL")
	# jsonrpsee answers -32602 "Invalid params" and puts the parser's message in `data`.
	detail=$(echo "$resp" | jq -r 'select(.error.code == -32602) | .error.data // empty')
	if [[ "$detail" != *duplicate* ]]; then
		echo "$name: expected an invalid-params duplicate-field error, got: $resp"
		exit 1
	fi
	echo "$name: rejected ($detail)"
}

expect_duplicate_rejected "repeated feeCurrency" \
	"{\"from\":\"$ACC_ADDR\",\"to\":\"$to\",\"feeCurrency\":\"$FEE_CURRENCY\",\"feeCurrency\":\"$other_currency\"}"
expect_duplicate_rejected "feeCurrency under two casings" \
	"{\"from\":\"$ACC_ADDR\",\"to\":\"$to\",\"feeCurrency\":\"$FEE_CURRENCY\",\"FeeCurrency\":\"$other_currency\"}"
expect_duplicate_rejected "repeated value" \
	"{\"from\":\"$ACC_ADDR\",\"to\":\"$to\",\"value\":\"0x1\",\"value\":\"0x2\"}"
