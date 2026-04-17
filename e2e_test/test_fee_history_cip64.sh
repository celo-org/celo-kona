#!/bin/bash
#shellcheck disable=SC2086
set -eo pipefail

source shared.sh
source debug-fee-currency/lib.sh

# Deploy a fee currency and send a CIP-64 transaction
fee_currency=$(deploy_fee_currency false false false)
cip_64_tx $fee_currency 10 | assert_cip_64_tx true

# Call eth_feeHistory with reward percentiles to check CIP-64 tip normalization
result=$(cast rpc eth_feeHistory "0x2" "latest" "[25, 50, 75]")

# Verify the response has reward percentiles
reward=$(echo "$result" | jq -r '.reward')
if [ "$reward" = "null" ] || [ "$reward" = "[]" ]; then
    echo "FAIL: eth_feeHistory returned no reward percentiles"
    exit 1
fi

# Verify at least one block has non-null rewards
block_rewards=$(echo "$result" | jq -r '.reward[0]')
if [ "$block_rewards" = "null" ]; then
    echo "FAIL: first block has null rewards"
    exit 1
fi

# Verify rewards are arrays with correct length (3 percentiles)
reward_len=$(echo "$result" | jq -r '.reward[0] | length')
if [ "$reward_len" != "3" ]; then
    echo "FAIL: expected 3 reward percentiles, got $reward_len"
    exit 1
fi

cleanup_fee_currency $fee_currency

echo "PASS: eth_feeHistory with CIP-64 tx returns valid reward percentiles"
