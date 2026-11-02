#!/usr/bin/env bash
# Runs the full flow on an anvil fork of L1 with the Safe impersonated:
# deploy Roles, execute the Safe batch, run the keeper, and check the permission is tight.
#
# Usage: FORK_URL=<l1 rpc> NETWORK=mainnet|sepolia fork-test.sh
set -euo pipefail
dir=$(dirname "$0")
: "${FORK_URL:?set FORK_URL}"
port=${PORT:-8545}

anvil --silent --port "$port" --fork-url "$FORK_URL" &
anvil_pid=$!
trap 'kill $anvil_pid' EXIT
export L1_RPC_URL=http://127.0.0.1:$port
for _ in $(seq 50); do cast chain-id --rpc-url "$L1_RPC_URL" >/dev/null 2>&1 && break; sleep 0.2; done

source "$dir/common.sh"

# anvil default accounts 0 and 1.
mnemonic="test test test test test test test test test test test junk"
keeper_key=$(cast wallet private-key --mnemonic "$mnemonic" --mnemonic-index 0)
outsider_key=$(cast wallet private-key --mnemonic "$mnemonic" --mnemonic-index 1)
keeper=$(cast wallet address "$keeper_key")
export CAST_WALLET_ARGS="--private-key $keeper_key"

pass() { echo "PASS: $*"; }
fail() {
	echo "FAIL: $*" >&2
	exit 1
}

roles=$(bash "$dir/deploy-roles.sh")
pass "deployed Roles proxy $roles"

cast rpc --rpc-url "$L1_RPC_URL" anvil_impersonateAccount "$SAFE" >/dev/null
cast rpc --rpc-url "$L1_RPC_URL" anvil_setBalance "$SAFE" 0xde0b6b3a7640000 >/dev/null
batch=$(bash "$dir/safe-batch.sh" "$roles" "$keeper")
while read -r to data; do
	cast send --rpc-url "$L1_RPC_URL" --unlocked --from "$SAFE" "$to" "$data" >/dev/null
done < <(jq -r '.transactions[] | "\(.to) \(.data)"' <<<"$batch")
[[ $(cast call --rpc-url "$L1_RPC_URL" "$SAFE" 'isModuleEnabled(address)(bool)' "$roles") == true ]] ||
	fail "module not enabled"
pass "executed Safe batch ($(jq '.transactions | length' <<<"$batch") txs)"

ROLES=$roles bash "$dir/keeper.sh"
expected=$(ROLES=$roles bash "$dir/keeper.sh" --dry-run | sed -E 's/.* -> ([0-9]+) wei.*/\1/' | head -1)
actual=$(cast call --rpc-url "$L1_RPC_URL" "$SYSTEM_CONFIG" 'minBaseFee()(uint64)' | awk '{print $1}')
[[ $actual == "$expected" ]] || fail "minBaseFee $actual, expected $expected"
pass "keeper set minBaseFee to $actual"

# Calls that must revert.
exec_role() { # <key> <to> <data>
	cast send --rpc-url "$L1_RPC_URL" --private-key "$1" "$roles" \
		'execTransactionWithRole(address,uint256,bytes,uint8,bytes32,bool)' \
		"$2" 0 "$3" 0 "$ROLE_KEY" true >/dev/null 2>&1
}
set_fee() { cast calldata 'setMinBaseFee(uint64)' "$1"; }
exec_role "$keeper_key" "$SYSTEM_CONFIG" "$(set_fee "$MIN_FLOOR")" || fail "MIN_FLOOR rejected"
exec_role "$keeper_key" "$SYSTEM_CONFIG" "$(set_fee "$MAX_FLOOR")" || fail "MAX_FLOOR rejected"
pass "bounds are inclusive"
exec_role "$keeper_key" "$SYSTEM_CONFIG" "$(set_fee $((MIN_FLOOR - 1)))" && fail "below MIN_FLOOR accepted"
exec_role "$keeper_key" "$SYSTEM_CONFIG" "$(set_fee $((MAX_FLOOR + 1)))" && fail "above MAX_FLOOR accepted"
pass "out-of-bounds values rejected"
exec_role "$keeper_key" "$SYSTEM_CONFIG" "$(cast calldata 'setGasLimit(uint64)' 60000000)" &&
	fail "other SystemConfig function accepted"
exec_role "$keeper_key" "$SAFE" "$(cast calldata 'enableModule(address)' "$keeper")" && fail "other target accepted"
pass "other functions and targets rejected"
exec_role "$outsider_key" "$SYSTEM_CONFIG" "$(set_fee "$MIN_FLOOR")" && fail "non-member accepted"
pass "non-member rejected"
echo "all checks passed"
