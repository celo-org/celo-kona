#!/usr/bin/env bash
set -eo pipefail

no_std_packages=(
  celo-revm
  alloy-celo-evm
  celo-alloy-consensus
  celo-alloy-rpc-types-engine
  celo-executor
  celo-proof
  celo-driver
)

for package in "${no_std_packages[@]}"; do
  cmd="cargo +stable build -p $package --target riscv32imac-unknown-none-elf --no-default-features"
  if [ -n "$CI" ]; then
    echo "::group::$cmd"
  else
    printf "\n%s:\n  %s\n" "$package" "$cmd"
  fi

  $cmd

  if [ -n "$CI" ]; then
    echo "::endgroup::"
  fi
done
