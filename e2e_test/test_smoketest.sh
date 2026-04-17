#!/bin/bash
set -eo pipefail

source shared.sh
prepare_node

(cd debug-fee-currency && forge build --out $PWD/out --cache-path $PWD/cache $PWD)
export COMPILED_TEST_CONTRACT=../debug-fee-currency/out/DebugFeeCurrency.sol/DebugFeeCurrency.json
(cd js-tests && ./node_modules/mocha/bin/mocha.js test_viem_smoketest.mjs --timeout 25000 --exit)

# NOTE: The op-geth repo also runs a Go-based smoketest for unsupported
# (deprecated) Celo tx types (CeloLegacy, CIP42). That test depends on
# go-ethereum Go modules and is not included here. See
# op-geth/e2e_test/smoketest_unsupported_txs/ for reference.
