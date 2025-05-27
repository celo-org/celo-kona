#!/usr/bin/env bash
set -eo pipefail

no_std_packages=(
  # revm
  celo-revm

  # alloy-evm
  alloy-celo-evm

  # op-alloy
  celo-alloy-consensus
  celo-alloy-rpc-types-engine
  celo-alloy-rpc-types

  # kona
  celo-executor
  celo-proof
  celo-driver
  celo-rpc
  celo-protocol
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
