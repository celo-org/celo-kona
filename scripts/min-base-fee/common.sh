# Shared config for the min-base-fee keeper. Source, don't execute.
# shellcheck shell=bash disable=SC2034 # variables are used by the scripts sourcing this

set -euo pipefail

NETWORK=${NETWORK:-mainnet}

case "$NETWORK" in
mainnet)
	L1_CHAIN_ID=1
	SYSTEM_CONFIG=0x89E31965D844a309231B1f17759Ccaf1b7c09861
	SAFE=0x9Eb44Da23433b5cAA1c87e35594D15FcEb08D34d
	;;
sepolia)
	L1_CHAIN_ID=11155111
	SYSTEM_CONFIG=0x760a5F022C9940f4A074e0030be682F560d29818
	SAFE=0x769b480A8036873a2a5EB01FE39278e5Ab78Bb27
	;;
*)
	echo "unknown NETWORK=$NETWORK" >&2
	exit 1
	;;
esac

# Zodiac Roles v2.1.1 and ModuleProxyFactory (same address on mainnet and Sepolia).
ROLES_MASTERCOPY=0xf2964ce6161ce0e75964fe7927ce114cb0b283d5
MODULE_PROXY_FACTORY=0x000000000000aDdB49795b0f9bA5BC298cDda236
ROLE_KEY=$(cast format-bytes32-string MIN_BASE_FEE_KEEPER)
SET_MIN_BASE_FEE_SELECTOR=0x7616f0e8 # setMinBaseFee(uint64)
DISABLE_MODULE_SELECTOR=0xe009cfde   # disableModule(address,address)
SENTINEL=0x0000000000000000000000000000000000000001

# Bounds enforced on-chain by the Roles condition (wei per gas, inclusive).
MIN_FLOOR=${MIN_FLOOR:-25000000000}   # 25 Gwei
MAX_FLOOR=${MAX_FLOOR:-2000000000000} # 2000 Gwei

# Signer for txs, passed to `cast send`, e.g. "--account keeper" or "--ledger".
read -r -a cast_wallet <<<"${CAST_WALLET_ARGS:-}"

: "${L1_RPC_URL:?set L1_RPC_URL}"

# Sends <to> <data> from the Safe through the keeper role on the Roles proxy ROLES.
exec_with_role() {
	cast send --rpc-url "$L1_RPC_URL" "${cast_wallet[@]}" "$ROLES" \
		'execTransactionWithRole(address,uint256,bytes,uint8,bytes32,bool)' \
		"$1" 0 "$2" 0 "$ROLE_KEY" true
}
