#!/bin/bash
#shellcheck disable=SC2086
set -eo pipefail

source shared.sh
source debug-fee-currency/lib.sh

tail -F -n0 celo-reth.log >debug-fee-currency/reth.disable_blocking.log & # start log capture
trap 'kill %%' EXIT # kill bg tail job on exit
(
	sleep 0.2
	fee_currency=$(deploy_fee_currency false false true)

	# Disable blocking for the fee currency
	disable_block_list_fee_currency $fee_currency

	# Send faulty transactions multiple times.
	# Because blocking is disabled, each attempt should hit the EVM execution
	# error (instead of being rejected early as "blocklisted").
	cip_64_tx $fee_currency 1 true 2 | assert_cip_64_tx false

	sleep 1

	cip_64_tx $fee_currency 1 true 2 | assert_cip_64_tx false

	sleep 1

	cleanup_fee_currency $fee_currency
)
sleep 0.5
# Because blocking was disabled, the execution error should appear multiple times
if [ "$(grep -Ec "fee-currency EVM execution error" debug-fee-currency/reth.disable_blocking.log)" -le 1 ]; then exit 1; fi
