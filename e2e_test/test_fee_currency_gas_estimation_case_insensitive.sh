#!/bin/bash
#shellcheck disable=SC2086
set -eo pipefail

source shared.sh
source debug-fee-currency/lib.sh

# go-ethereum matches JSON keys to struct tags case-insensitively, so op-geth accepts
# `feecurrency` (all-lowercase, as sent by some clients such as minipay). celo-reth mirrors
# that leniency; without it, a lowercase key is silently treated as a native-fee tx and the
# gas estimate omits the fee-currency intrinsic surcharge, producing a too-low estimate that
# the sequencer later rejects with "intrinsic gas too low". This test guards that parity by
# issuing a raw eth_estimateGas with the non-canonical `feecurrency` key and asserting the
# surcharge is still applied.

fee_currency=$(deploy_fee_currency false false false 70000)

body=$(cat <<EOF
{"jsonrpc":"2.0","id":1,"method":"eth_estimateGas","params":[{"from":"$ACC_ADDR","to":"0x00000000000000000000000000000000DeaDBeef","value":"0x14","feecurrency":"$fee_currency"}]}
EOF
)

resp=$(curl -s -X POST -H 'Content-Type: application/json' --data "$body" "$ETH_RPC_URL")
hex=$(echo "$resp" | jq -r '.result // empty')

cleanup_fee_currency $fee_currency

if [ -z "$hex" ]; then
	echo "eth_estimateGas failed: $resp"
	exit 1
fi

gas=$(cast to-dec "$hex")

# intrinsic of fee_currency: 70000
# intrinsic of tx: 21000
# total: 91000
# Allow up to 2% overhead for binary-search based estimators (e.g. reth).
if [ $gas -lt 91000 ] || [ $gas -gt 92820 ]; then
	echo "lowercase feecurrency estimate $gas outside expected surcharged band 91000-92820"
	exit 1
fi
