#!/usr/bin/env bash
# Sets SystemConfig.minBaseFee so that a reference tx (USDT transfer paid in USDT) costs
# TARGET_ATTO_USD_PER_TX. Run daily. Exits non-zero on anything that needs a human.
#
# Usage: ROLES=<roles-proxy> keeper.sh [--dry-run]
source "$(dirname "$0")/common.sh"

: "${ROLES:?set ROLES to the roles proxy address}"
dry_run=${1:-}

fail() {
	echo "keeper: $*" >&2
	exit 1
}

# Primary price: Chainlink CELO/USD on Celo Mainnet.
round=$(cast call --rpc-url "$L2_RPC_URL" "$CHAINLINK_CELO_USD" \
	'latestRoundData()(uint80,int256,uint256,uint256,uint80)')
answer=$(awk 'NR == 2 {print $1}' <<<"$round")
updated_at=$(awk 'NR == 4 {print $1}' <<<"$round")
decimals=$(cast call --rpc-url "$L2_RPC_URL" "$CHAINLINK_CELO_USD" 'decimals()(uint8)')
age=$(($(date +%s) - updated_at))
((age <= MAX_PRICE_AGE)) || fail "Chainlink price is ${age}s old"
((answer > 0)) || fail "Chainlink price $answer"
chainlink=$(bc -l <<<"scale = $decimals; $answer / 10^$decimals")

# Cross-check: Coinbase spot.
coinbase=$(curl -fsS --max-time 20 "$COINBASE_URL" | jq -er '.data.amount') || fail "Coinbase fetch failed"
deviation_bps=$(bc -l <<<"d = ($chainlink - $coinbase) / $coinbase * 10000; if (d < 0) d = -d; scale = 0; d / 1")
((deviation_bps <= MAX_PRICE_DEVIATION_BPS)) ||
	fail "prices disagree by ${deviation_bps} bps: Chainlink $chainlink, Coinbase $coinbase"

# floor [wei/gas] = target [attoUSD/tx] / gas [gas/tx] / price [USD/CELO]
floor=$(bc <<<"$TARGET_ATTO_USD_PER_TX * 10^$decimals / ($REFERENCE_GAS * $answer)")
((floor >= MIN_FLOOR && floor <= MAX_FLOOR)) || fail "floor $floor outside [$MIN_FLOOR, $MAX_FLOOR]"

current=$(cast call --rpc-url "$L1_RPC_URL" "$SYSTEM_CONFIG" 'minBaseFee()(uint64)' | awk '{print $1}')
change_bps=$(bc <<<"d = ($floor - $current) * 10000 / $current; if (d < 0) d = -d; d")
echo "CELO/USD $chainlink (Coinbase $coinbase), floor $current -> $floor wei (${change_bps} bps)"

if ((change_bps < MIN_CHANGE_BPS)); then
	echo "change below ${MIN_CHANGE_BPS} bps, skipping"
	exit 0
fi
if [[ $dry_run == --dry-run ]]; then
	echo "dry run, not sending"
	exit 0
fi

data=$(cast calldata 'setMinBaseFee(uint64)' "$floor")
cast send --rpc-url "$L1_RPC_URL" "${cast_wallet[@]}" "$ROLES" \
	'execTransactionWithRole(address,uint256,bytes,uint8,bytes32,bool)' \
	"$SYSTEM_CONFIG" 0 "$data" 0 "$ROLE_KEY" true
