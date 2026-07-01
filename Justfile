set positional-arguments
alias t := test
alias la := lint-all
alias l := lint-native
alias lint := lint-native
alias f := fmt-native-fix
alias b := build-native
alias h := hack

exclude_members := "--exclude celo-registry --exclude execution-fixture"

# Toolchain for `check-udeps`. CI pins this to a known-good nightly via the
# CELO_UDEPS_TOOLCHAIN env var; locally it defaults to the `nightly` channel.
udeps_toolchain := env_var_or_default("CELO_UDEPS_TOOLCHAIN", "nightly")

# default recipe to display help information
default:
  @just --list

# Install git hooks (nightly fmt check on pre-commit)
setup:
  git config core.hooksPath .githooks

# Test for the native target with all features. Excludes the slow trybuild
# compile-fail guard (see `test-trybuild`).
test:
  cargo nextest run --workspace --all-features {{exclude_members}} -E 'not binary(units_compile_fail)'

# Slow trybuild compile-fail guard for `celo_revm::units` mixed-denomination
# defenses. Each UI fixture triggers a fresh from-scratch compile in a
# trybuild-managed sub-target, so this is split off from `just test` and only
# runs in CI post-merge to main.
test-trybuild:
  cargo nextest run -p celo-reth --test units_compile_fail

# Runs benchmarks
benches:
  cargo bench --no-run --workspace --features test-utils {{exclude_members}}

# Lint the workspace for all available targets
lint-all: lint-native lint-docs

# Runs `cargo hack check` against the workspace
hack:
  cargo hack check --no-default-features --no-dev-deps {{exclude_members}}

# Fixes the formatting of the workspace
fmt-native-fix:
  cargo +nightly fmt --all

# Check the formatting of the workspace
fmt-native-check:
  cargo +nightly fmt --all -- --check

# Lint the workspace
lint-native: fmt-native-check lint-docs
  cargo clippy --workspace --all-features --all-targets {{exclude_members}} -- -D warnings

# Lint the Rust documentation
lint-docs:
  RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items {{exclude_members}}

# Build for the native target
build-native *args='':
  cargo build --workspace {{exclude_members}} $@

# Check for unused dependencies in the crate graph.
check-udeps:
  cargo +{{udeps_toolchain}} udeps --workspace --all-features --all-targets

# Release a new version (dry-run by default)
# Requires: cargo install cargo-release@0.25.20 --locked
release level='patch' *args='':
  cargo release {{level}} {{args}}

# Release a new version (execute)
release-execute level='patch':
  cargo release {{level}} --execute

# Reproduce the `FPP e2e` CI proof job locally (.github/workflows/proof.yaml):
# extracts the checked-in witness and replays Celo Sepolia (Isthmus) block
# #12177762 through the host + client offline. Run inside `nix develop` (needs
# the Rust toolchain); the first run builds `celo-host --features eigenda`.
# The fixture values mirror the "Set run environment" step in proof.yaml.
reproduce-proof-ci verbosity='':
  #!/usr/bin/env bash
  set -o errexit -o nounset -o pipefail

  WITNESS=bin/client/testdata/isthmus-celo-sepolia-12177762-witness.tar.zst
  echo "Extracting witness data to ./data ..."
  # --exclude '._*' skips the macOS AppleDouble metadata files the tar carries
  # (otherwise a stray ./._data lands, untracked, at the repo root).
  tar --zstd --exclude '._*' -xf "$WITNESS" -C .

  just -f bin/client/justfile run-client-native-offline \
    12177762 \
    0x5783aab680c0aabfae6d633a465d02da922247c8e8adf8c9a18f6caa9da3b40a \
    0xf23a74d78b68d441fde3c2be42df34844bdd13f7c47db912946b2ba120f2cd79 \
    0x594061d8101caba769b5047325840b806aaa1ab26c655a3ba0c3c878a09318b4 \
    0xf56e3e0b057a7b841b06598b37b2bc99fd119224fa4328913a085d4c49efe344 \
    11142220 \
    "{{verbosity}}"

# Download resources/g1.point if it doesn't exist.
download-srs:
  #!/usr/bin/env bash
  if [ ! -f "resources/g1.point" ]; then
      echo "Downloading SRS G1 points to resources/g1.point ..."
      mkdir -p resources
      curl -o resources/g1.point -L https://github.com/Layr-Labs/eigenda-proxy/raw/refs/heads/main/resources/g1.point || { echo "Error: Failed to download SRS G1 points."; exit 1; }
  else
      echo "SRS file resources/g1.point already exists, skipping download"
  fi
