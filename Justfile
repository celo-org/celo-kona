set positional-arguments
alias la := lint-all
alias l := lint-native
alias lint := lint-native
alias f := fmt-native-fix
alias b := build-native
alias h := hack

# default recipe to display help information
default:
  @just --list

# Test for the native target with all features.
test:
  cargo nextest run --workspace --all-features

# Lint the workspace for all available targets
lint-all: lint-native lint-docs

# Runs `cargo hack check` against the workspace
hack:
  cargo hack check --no-default-features --no-dev-deps

# Fixes the formatting of the workspace
fmt-native-fix:
  cargo +nightly fmt --all

# Check the formatting of the workspace
fmt-native-check:
  cargo +nightly fmt --all -- --check

# Lint the workspace
lint-native: fmt-native-check lint-docs
  cargo clippy --workspace --all-features --all-targets -- -D warnings

# Lint the Rust documentation
lint-docs:
  RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items

# Build for the native target
build-native *args='':
  cargo build --workspace $@

# Check for unused dependencies in the crate graph.
check-udeps:
  cargo +nightly udeps --workspace --all-features --all-targets