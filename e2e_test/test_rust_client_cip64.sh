#!/bin/bash
#
# Sends a CIP-64 tx through the Rust alloy client (celo-alloy-network) instead of a JS
# wallet: provider with the `Celo` network, recommended fillers and a local signer, with
# only `feeCurrency` + to/value set on the request. The example itself asserts the CIP-64
# receipt shape, the fee-currency baseFee, and the feeCurrency echoed by the node.
set -eo pipefail
set -x

source shared.sh

# A fresh account exercises nonce filling from zero and proves the fee was paid in the fee
# currency: its CELO only moves by VALUE.
WALLET_JSON=$(cast wallet new --json)
TEST_ACCOUNT_ADDR=$(echo "$WALLET_JSON" | jq -r '.[0].address')
TEST_ACCOUNT_PRIVKEY=$(echo "$WALLET_JSON" | jq -r '.[0].private_key')

# 1e18 fee currency for gas, 1e15 CELO for the transferred value.
cast send --private-key $ACC_PRIVKEY $FEE_CURRENCY 'transfer(address to, uint256 value) returns (bool)' $TEST_ACCOUNT_ADDR 1000000000000000000
cast send --private-key $ACC_PRIVKEY --value 1000000000000000 $TEST_ACCOUNT_ADDR

celo_balance_before=$(cast balance $TEST_ACCOUNT_ADDR)
fee_balance_before=$(cast call $FEE_CURRENCY 'balanceOf(address) returns (uint256)' $TEST_ACCOUNT_ADDR)

ACC_PRIVKEY=$TEST_ACCOUNT_PRIVKEY TO=$ACC_ADDR VALUE=1000000000000 \
	cargo run --manifest-path "$SCRIPT_DIR/../Cargo.toml" -p celo-alloy-network --example send_cip64

celo_balance_after=$(cast balance $TEST_ACCOUNT_ADDR)
fee_balance_after=$(cast call $FEE_CURRENCY 'balanceOf(address) returns (uint256)' $TEST_ACCOUNT_ADDR)

# The gas fee must have been debited from the fee-currency balance, and the native
# balance must have dropped by exactly the transferred value.
if [[ "$fee_balance_after" == "$fee_balance_before" ]]; then
	echo "ERROR: fee currency balance unchanged - gas was not paid in the fee currency"
	exit 1
fi
expected_celo=$((celo_balance_before - 1000000000000))
if [[ "$celo_balance_after" != "$expected_celo" ]]; then
	echo "ERROR: native balance changed by more than the transferred value ($celo_balance_before -> $celo_balance_after)"
	exit 1
fi
