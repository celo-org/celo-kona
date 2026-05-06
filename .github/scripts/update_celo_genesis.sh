#!/usr/bin/env bash
# Refreshes the Celo genesis files embedded in `celo-reth` from
# https://github.com/celo-org/superchain-registry.
#
# Mirrors the upstream OP Stack `superchain-configs.tar` pattern: ship
# compressed payloads alongside the binary, decompress at parse time. We
# embed the raw `.json.zst` + the registry's shared zstd dictionary, so
# updating just means re-fetching the three blobs verbatim.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DEST="$REPO_ROOT/crates/celo-reth/res/genesis"
mkdir -p "$DEST"

gh api repos/celo-org/superchain-registry/contents/superchain/extra/dictionary \
  --jq '.content' | base64 -d > "$DEST/dictionary"

for net in mainnet sepolia; do
  gh api "repos/celo-org/superchain-registry/contents/superchain/extra/genesis/$net/celo.json.zst" \
    --jq '.content' | base64 -d > "$DEST/$net.json.zst"
  # Pull the live `[hardforks]` table — the genesis JSON is frozen at the
  # snapshot date and may lag behind hardfork schedule updates.
  gh api "repos/celo-org/superchain-registry/contents/superchain/configs/$net/celo.toml" \
    --jq '.content' | base64 -d > "$DEST/$net.toml"
done

ls -la "$DEST"
