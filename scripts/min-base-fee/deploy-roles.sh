#!/usr/bin/env bash
# Deploys a Zodiac Roles v2 proxy owned by the SystemConfig owner Safe. Anyone can run this.
# Prints the proxy address.
source "$(dirname "$0")/common.sh"

init=$(cast abi-encode 'f(address,address,address)' "$SAFE" "$SAFE" "$SAFE")
setup=$(cast calldata 'setUp(bytes)' "$init")
salt=${SALT_NONCE:-$(date +%s)}

receipt=$(cast send --json --rpc-url "$L1_RPC_URL" "${cast_wallet[@]}" \
	"$MODULE_PROXY_FACTORY" 'deployModule(address,bytes,uint256)' \
	"$ROLES_MASTERCOPY" "$setup" "$salt")

# ModuleProxyCreation(address indexed proxy, address indexed masterCopy)
topic=$(cast keccak 'ModuleProxyCreation(address,address)')
proxy=$(jq -r --arg t "$topic" '.logs[] | select(.topics[0] == $t) | .topics[1]' <<<"$receipt")
cast parse-bytes32-address "$proxy"
