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
# currency: its CELO only moves by VALUE. The key comes from openssl because the JSON shape
# of `cast wallet new` differs between foundry versions.
TEST_ACCOUNT_PRIVKEY=0x$(openssl rand -hex 32)
TEST_ACCOUNT_ADDR=$(cast wallet address --private-key $TEST_ACCOUNT_PRIVKEY)

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

# A sender who holds CELO but none of the fee currency must fail at the gas estimate. The
# client sends the estimate with the fees the tx will carry, so the node caps the gas by the
# fee-currency balance; an estimate without fees would pass and leave the rejection to the
# pool. No unit test can see this: the mocked transport never looks at request params.
UNFUNDED_PRIVKEY=0x$(openssl rand -hex 32)
UNFUNDED_ADDR=$(cast wallet address --private-key $UNFUNDED_PRIVKEY)
cast send --private-key $ACC_PRIVKEY --value 1000000000000000 $UNFUNDED_ADDR

if unfunded_output=$(ACC_PRIVKEY=$UNFUNDED_PRIVKEY TO=$ACC_ADDR VALUE=1 \
	cargo run --manifest-path "$SCRIPT_DIR/../Cargo.toml" -p celo-alloy-network --example send_cip64 2>&1); then
	echo "ERROR: a sender without the fee currency sent a CIP-64 tx"
	exit 1
fi
# `(0)` is the gas the node allows: the fee-currency balance, zero here, over the cap.
if [[ "$unfunded_output" != *"gas required exceeds allowance (0)"* ]]; then
	echo "ERROR: expected the gas estimate's allowance error, got: $unfunded_output"
	exit 1
fi
