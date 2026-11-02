#!/usr/bin/env bash
# Prints a Safe Transaction Builder batch that enables the Roles module and lets KEEPER call
# SystemConfig.setMinBaseFee with MIN_FLOOR <= value <= MAX_FLOOR, and nothing else.
#
# Usage: safe-batch.sh <roles-proxy> <keeper>
source "$(dirname "$0")/common.sh"

roles=${1:?roles proxy address}
keeper=${2:?keeper address}

# Condition tree: calldata matches (arg0 > MIN_FLOOR-1 AND arg0 < MAX_FLOOR+1).
# Fields: (parent, paramType, operator, compValue). paramType 0=None 1=Static 5=Calldata;
# operator 1=And 5=Matches 17=GreaterThan 18=LessThan.
gt=$(cast abi-encode 'f(uint256)' "$(bc <<<"$MIN_FLOOR - 1")")
lt=$(cast abi-encode 'f(uint256)' "$(bc <<<"$MAX_FLOOR + 1")")
conditions="[(0,5,5,0x),(0,0,1,0x),(1,1,17,$gt),(1,1,18,$lt)]"

txs=(
	"$SAFE $(cast calldata 'enableModule(address)' "$roles")"
	"$roles $(cast calldata 'assignRoles(address,bytes32[],bool[])' "$keeper" "[$ROLE_KEY]" '[true]')"
	"$roles $(cast calldata 'scopeTarget(bytes32,address)' "$ROLE_KEY" "$SYSTEM_CONFIG")"
	"$roles $(cast calldata 'scopeFunction(bytes32,address,bytes4,(uint8,uint8,uint8,bytes)[],uint8)' \
		"$ROLE_KEY" "$SYSTEM_CONFIG" "$SET_MIN_BASE_FEE_SELECTOR" "$conditions" 0)"
)

for tx in "${txs[@]}"; do
	read -r to data <<<"$tx"
	jq -n --arg to "$to" --arg data "$data" '{to: $to, value: "0", data: $data}'
done | jq -s \
	--arg chainId "$L1_CHAIN_ID" \
	--arg desc "Roles module $roles: keeper $keeper may call setMinBaseFee on $SYSTEM_CONFIG within [$MIN_FLOOR, $MAX_FLOOR] wei" \
	'{version: "1.0", chainId: $chainId, meta: {name: "Min base fee keeper", description: $desc}, transactions: .}'
