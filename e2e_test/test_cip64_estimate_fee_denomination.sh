#!/bin/bash
#shellcheck disable=SC2086
set -eo pipefail

source shared.sh

# A CIP-64 request's maxFeePerGas and maxPriorityFeePerGas are denominated in its fee
# currency. In the dev genesis one FEE_CURRENCY2 buys about 515 CELO (rate 0.001943), so
# its base fee is a small fraction of the native one and a cap a client derives from
# eth_gasPrice(feeCurrency) sits far below the native base fee. eth_estimateGas and
# eth_call must price such a request in the fee currency instead of rejecting it against
# the native base fee, and the gas allowance must come from the fee-currency balance
# rather than the native one (celo-kona #312).

DEAD=0x00000000000000000000000000000000DeaDBeef
fc=$FEE_CURRENCY2

rpc() {
	curl -s -X POST -H 'Content-Type: application/json' --data "$1" "$ETH_RPC_URL"
}

# Pin every call to one block so the base fee cannot move under the test.
block=$(cast block-number)
block_hex=$(cast to-hex $block)
base_fee=$(cast base-fee $block)
rate=$(cast call $FEE_CURRENCY_DIRECTORY_ADDR "getExchangeRate(address)(uint256,uint256)" $fc --block $block)
num=$(echo "$rate" | sed -n 1p | awk '{print $1}')
den=$(echo "$rate" | sed -n 2p | awk '{print $1}')
base_fee_fc=$(echo "$base_fee * $num / $den" | bc)
tip_fc=$(cast to-dec "$(rpc '{"jsonrpc":"2.0","id":1,"method":"eth_maxPriorityFeePerGas","params":["'$fc'"]}' | jq -r .result)")
# The cap fill_cip64_fee_defaults would pick: 2 * base fee + tip, in the fee currency.
cap_fc=$(echo "2 * $base_fee_fc + $tip_fc" | bc)
cap_hex=$(cast to-hex $cap_fc)
tip_hex=$(cast to-hex $tip_fc)

if [ "$(echo "$cap_fc < $base_fee" | bc)" != "1" ]; then
	echo "cap $cap_fc is not below the native base fee $base_fee; the test needs a currency worth more than two CELO (rate below 0.5)"
	exit 1
fi

fee_fields='"feeCurrency":"'$fc'","maxFeePerGas":"'$cap_hex'","maxPriorityFeePerGas":"'$tip_hex'"'

assert_result() {
	local what="$1" resp="$2"
	if [ "$(echo "$resp" | jq -r '.result // empty')" = "" ]; then
		echo "FAIL: $what: $resp"
		exit 1
	fi
}

assert_error_contains() {
	local what="$1" resp="$2" expected="$3"
	if ! echo "$resp" | jq -r '.error.message // empty' | grep -q "$expected"; then
		echo "FAIL: $what did not fail with '$expected': $resp"
		exit 1
	fi
}

# intrinsic of tx: 21000, intrinsic of the fee currency: 50000; allow 2% binary-search slack.
assert_estimate_band() {
	local what="$1" gas="$2"
	if [ $gas -lt 71000 ] || [ $gas -gt 72420 ]; then
		echo "FAIL: $what estimated $gas, outside 71000-72420"
		exit 1
	fi
}

# 1. eth_estimateGas with a fee-currency cap below the native base fee.
resp=$(rpc '{"jsonrpc":"2.0","id":1,"method":"eth_estimateGas","params":[{"from":"'$ACC_ADDR'","to":"'$DEAD'","value":"0x14",'$fee_fields'},"'$block_hex'"]}')
assert_result "eth_estimateGas with fee-currency cap" "$resp"
gas=$(cast to-dec "$(echo "$resp" | jq -r .result)")
assert_estimate_band "eth_estimateGas with fee-currency cap" $gas

# Same estimate without fee fields: the cap must not change the result.
resp=$(rpc '{"jsonrpc":"2.0","id":1,"method":"eth_estimateGas","params":[{"from":"'$ACC_ADDR'","to":"'$DEAD'","value":"0x14","feeCurrency":"'$fc'"},"'$block_hex'"]}')
assert_result "eth_estimateGas without fee fields" "$resp"
if [ "$(cast to-dec "$(echo "$resp" | jq -r .result)")" != "$gas" ]; then
	echo "FAIL: estimate with fee fields ($gas) differs from without: $resp"
	exit 1
fi

# 2. eth_call with the same fee fields.
resp=$(rpc '{"jsonrpc":"2.0","id":1,"method":"eth_call","params":[{"from":"'$ACC_ADDR'","to":"'$DEAD'","value":"0x14",'$fee_fields'},"'$block_hex'"]}')
assert_result "eth_call with fee-currency cap" "$resp"

# 3. GASPRICE inside the simulation is the fee-currency price min(cap, base fee + tip).
gasprice_probe=0x000000000000000000000000000000000000cafe
resp=$(rpc '{"jsonrpc":"2.0","id":1,"method":"eth_call","params":[{"from":"'$ACC_ADDR'","to":"'$gasprice_probe'",'$fee_fields'},"'$block_hex'",{"'$gasprice_probe'":{"code":"0x3a60005260206000f3"}}]}')
assert_result "eth_call GASPRICE probe" "$resp"
gasprice=$(cast to-dec "$(echo "$resp" | jq -r .result)")
expected=$(echo "$base_fee_fc + $tip_fc" | bc)
if [ "$gasprice" != "$expected" ]; then
	echo "FAIL: GASPRICE read $gasprice, expected fee-currency price $expected (cap $cap_fc, native base fee $base_fee)"
	exit 1
fi

# 4. The allowance comes from the fee-currency balance: a sender holding only the fee
#    currency (no CELO) can estimate, one holding neither is capped at zero gas.
sender=0x00000000000000000000000000000000000000fe
balance_slot=$(cast index address $sender 0)
resp=$(rpc '{"jsonrpc":"2.0","id":1,"method":"eth_estimateGas","params":[{"from":"'$sender'","to":"'$DEAD'",'$fee_fields'},"'$block_hex'",{"'$fc'":{"stateDiff":{"'$balance_slot'":"0x00000000000000000000000000000000000000000000003635c9adc5dea00000"}}}]}')
assert_result "eth_estimateGas for a fee-currency-only sender" "$resp"
assert_estimate_band "eth_estimateGas for a fee-currency-only sender" "$(cast to-dec "$(echo "$resp" | jq -r .result)")"

resp=$(rpc '{"jsonrpc":"2.0","id":1,"method":"eth_estimateGas","params":[{"from":"'$sender'","to":"'$DEAD'",'$fee_fields'},"'$block_hex'"]}')
assert_error_contains "eth_estimateGas for a sender without the fee currency" "$resp" "exceeds allowance"

# eth_call applies the same cap: with no fee-currency balance the call gets zero gas and
# fails, as it does on op-geth. Holding CELO does not help; the fee is not paid from it.
resp=$(rpc '{"jsonrpc":"2.0","id":1,"method":"eth_call","params":[{"from":"'$ACC_ADDR'","to":"'$DEAD'",'$fee_fields'},"'$block_hex'",{"'$fc'":{"stateDiff":{"'$(cast index address $ACC_ADDR 0)'":"0x0000000000000000000000000000000000000000000000000000000000000000"}}}]}')
assert_error_contains "eth_call for a sender without the fee currency" "$resp" "intrinsic gas too low"
