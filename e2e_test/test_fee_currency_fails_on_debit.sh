#!/bin/bash
#shellcheck disable=SC2086
set -eo pipefail

source shared.sh
source debug-fee-currency/lib.sh

# Expect that the debitGasFees fails during tx submission
#
fee_currency=$(deploy_fee_currency true false false)
# The pool simulates debitGasFees at the chain's active fork, so a fee currency
# whose debitGasFees() reverts is rejected up front: eth_sendRawTransaction
# returns an error and the tx never enters the pool. (Previously the pool EVM ran
# at the BEDROCK default, where the fee currency's PUSH0 bytecode halted and the
# simulation silently no-opped, so the tx was admitted and only dropped later
# during block building.)
cip_64_tx $fee_currency 1 true 2 | assert_cip_64_tx false "debitGasFees simulation failed"

cleanup_fee_currency $fee_currency
