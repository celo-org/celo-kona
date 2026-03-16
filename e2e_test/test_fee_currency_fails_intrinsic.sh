#!/bin/bash
#shellcheck disable=SC2086
set -eo pipefail

source shared.sh
source debug-fee-currency/lib.sh

# Test that a fee currency with highGasOnCredit (exceeds intrinsic gas limit
# for CreditFees) causes the CIP-64 tx to fail (not be mined).
(
	sleep 0.2
	fee_currency=$(deploy_fee_currency false false true)

	# trigger the first failed call — CreditFees() exceeds intrinsic gas.
	# tx should not succeed.
	cip_64_tx $fee_currency 1 true 2 | assert_cip_64_tx false

	sleep 2

	# second attempt should also fail.
	cip_64_tx $fee_currency 1 true 2 | assert_cip_64_tx false

	cleanup_fee_currency $fee_currency
)
