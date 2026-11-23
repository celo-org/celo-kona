#!/usr/bin/env bash
# Stops the keeper by disabling the Roles module on the Safe. Signed with the keeper key.
# Re-enabling takes a Safe tx: enableModule(roles).
#
# Usage: ROLES=<roles-proxy> disable.sh
source "$(dirname "$0")/common.sh"

: "${ROLES:?set ROLES to the roles proxy address}"
roles=$(cast to-check-sum-address "$ROLES")

# Safe modules form a linked list; disableModule needs the entry before ROLES.
modules=$(cast call --rpc-url "$L1_RPC_URL" "$SAFE" \
	'getModulesPaginated(address,uint256)(address[],address)' "$SENTINEL" 100 | head -1 | tr -d '[],')
prev=$SENTINEL
found=
for module in $modules; do
	if [[ $module == "$roles" ]]; then
		found=1
		break
	fi
	prev=$module
done
[[ -n $found ]] || {
	echo "disable: $roles is not enabled on $SAFE" >&2
	exit 1
}

exec_with_role "$SAFE" "$(cast calldata 'disableModule(address,address)' "$prev" "$roles")"
