#!/bin/bash
set -eo pipefail

source shared.sh
source debug-fee-currency/lib.sh

# The proofs-history sidecar (--proofs-history) installs RPC overrides that serve
# debug_executePayload, debug_executionWitness, and debug_proofsSyncStatus from the
# bounded-history store. debug_proofsSyncStatus exists ONLY in that override, so a
# well-formed response proves DebugApiExt is wired in (celo-kona#188).

# debug_proofsSyncStatus reports the available proof window.
sync_status=$(cast rpc debug_proofsSyncStatus)
earliest=$(echo "$sync_status" | jq -r '.earliest')
latest=$(echo "$sync_status" | jq -r '.latest')
if [ "$earliest" = "null" ] || [ "$latest" = "null" ]; then
    echo "FAIL: debug_proofsSyncStatus returned an empty window: $sync_status"
    exit 1
fi
echo "proof window: earliest=$earliest latest=$latest"

# Mine a block containing a CIP-64 (fee-currency) transaction, then prove the sidecar
# serves a witness for that block. CIP-64 has no op-alloy OpPooledTransaction
# representation, so this confirms proofs work for CIP-64 blocks regardless: the
# witness path re-executes the block via the Celo executor, not the (Noop) pool
# iterator the conversion bound applies to.
fee_currency=$(deploy_fee_currency false false false)
before=$(cast block-number)
cip_64_tx "$fee_currency" 10 | assert_cip_64_tx true
after=$(cast block-number)

# Locate the block the CIP-64 tx (type 0x7b) landed in.
cip64_block=
cip64_num=
for ((n = before + 1; n <= after; n++)); do
    hexn=$(cast to-hex "$n")
    if cast rpc eth_getBlockByNumber "$hexn" true | jq -e '.transactions[] | select(.type == "0x7b")' >/dev/null; then
        cip64_block=$hexn
        cip64_num=$n
        break
    fi
done
if [ -z "$cip64_block" ]; then
    echo "FAIL: no CIP-64 tx found in blocks $((before + 1))..$after"
    exit 1
fi
echo "CIP-64 tx mined in block $cip64_block"

# The sidecar state provider errors for out-of-window blocks instead of falling back
# to live state, and executionWitness needs the block's parent state. The ExEx can
# trail the chain head, so wait until the proof window covers the CIP-64 block.
window_latest=
for _ in $(seq 1 60); do
    window_latest=$(cast rpc debug_proofsSyncStatus | jq -r '.latest')
    if [ "$window_latest" != "null" ] && [ "$window_latest" -ge "$cip64_num" ]; then
        break
    fi
    sleep 0.5
done
if [ "$window_latest" = "null" ] || [ "$window_latest" -lt "$cip64_num" ]; then
    echo "FAIL: proof window latest=$window_latest never reached CIP-64 block $cip64_num"
    exit 1
fi

# debug_executionWitness re-executes that block using sidecar-served state.
witness=$(cast rpc debug_executionWitness "$cip64_block")
state_len=$(echo "$witness" | jq -r '.state | length')
if [ "$state_len" = "null" ] || [ "$state_len" -lt 1 ]; then
    echo "FAIL: debug_executionWitness returned no state for CIP-64 block $cip64_block: $witness"
    exit 1
fi
echo "execution witness for CIP-64 block: $state_len state entries"

cleanup_fee_currency "$fee_currency"

echo "PASS: proofs-history sidecar serves a witness for a CIP-64 block"
