#!/bin/bash
#shellcheck disable=SC2086
set -eo pipefail

source shared.sh
source debug-fee-currency/lib.sh

# Expect that the debitGasFees fails during tx submission
#
fee_currency=$(deploy_fee_currency true false false)
# In celo-reth the debit failure happens during EVM execution (not pool validation),
# so the tx enters the pool but is skipped during block building. The result is
# success=false with no specific error message from the RPC.
cip_64_tx $fee_currency 1 true 2 | assert_cip_64_tx false

cleanup_fee_currency $fee_currency
