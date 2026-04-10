# CLAUDE.md

## Project Overview

This repo contains two things built on a shared Celo EVM:

1. **celo-reth** — a full Celo L2 execution node, extending [op-reth](https://github.com/paradigmxyz/reth) (the OP Stack reth node) with Celo-specific pool, payload, and RPC logic.
2. **kona crates** — Celo extensions to [Kona](https://github.com/op-rs/kona) (the OP Stack ZK/fault proof client), used by Celo's [op-succinct fork](https://github.com/celo-org/op-succinct/) for ZK fault proofs.

The core Celo EVM additions are **Fee Abstraction** (CIP-64 tx type — pay gas in ERC20 tokens) and **Token Duality** (native CELO ↔ ERC20 via transfer precompile). These live in `celo-revm` and `alloy-celo-evm`, shared by both celo-reth and kona.

## Build & Test Commands

```bash
just build-native          # Build workspace (alias: just b)
just test                  # Run all tests with nextest (alias: just t)
just lint-native           # Format check + clippy + doc lint (alias: just l)
just fmt-native-fix        # Fix formatting with nightly (alias: just f)
cargo test -p <crate>      # Test single crate
cargo build -p celo-reth   # Build single crate
scripts/check_no_std.sh    # Verify no-std compatibility
```

Requires: nightly Rust toolchain (for `cargo +nightly fmt`), `cargo-nextest`.

Excluded from workspace operations: `celo-registry`, `execution-fixture`.

## Architecture

### Crate Dependency Flow

```
celo-reth (reth node)
  ├── alloy-celo-evm (alloy-evm wrapper)
  │     └── celo-revm (revm fork with Celo handlers)
  │           └── celo-alloy/consensus (CeloTxEnvelope, TxCip64)
  └── celo-alloy/rpc-types (CeloTransactionRequest)

kona (ZK proof client/host)
  └── alloy-celo-evm
        └── celo-revm
```

### Key Crates

- **`celo-revm`**: Low-level EVM modifications. Custom handler that intercepts CIP-64 txs to debit/credit fee currency via ERC20 calls. Contains the `FeeCurrencyContext`, contract ABIs for `FeeCurrencyDirectory`, and the transfer precompile. Must compile for `riscv32imac-unknown-none-elf` (no-std).

- **`alloy-celo-evm`**: Wraps `celo-revm` into `alloy-evm`'s `Evm` trait (`CeloEvm`). Adds the fee currency blocklist (blocks currencies whose debit/credit calls fail during block building, with time-based eviction). Contains `CeloEvmFactory` and `Cip64Storage` for passing fee info to receipt builder.

- **`celo-reth`**: Full reth node. Contains:
  - `pool.rs` — `CeloExchangeRateApplier`: tx pool validator that converts CIP-64 fee fields to native equivalents via on-chain exchange rate lookup
  - `payload.rs` — `CeloPayloadTransactions`: per-fee-currency block gas limits
  - `rpc.rs` — Custom `eth_gasPrice` that returns the 25 Gwei base fee floor
  - `node.rs` — Node component wiring

- **`celo-alloy`**: Consensus types (`CeloTxEnvelope`, `TxCip64`, `CeloReceipt`), network types, and RPC types. Workspace subcrates under `crates/celo-alloy/`.

- **`kona`**: Celo-specific wrappers around Kona's driver, executor, genesis, and protocol crates for ZK proof generation.

### No-std Support

`celo-revm`, `alloy-celo-evm`, `celo-alloy/*`, and `kona/*` must compile without `std`. Use `#[cfg(feature = "std")]` to gate std-only code. The `celo-reth` crate is std-only.

### Fee Currency Flow (CIP-64)

1. **Pool entry** (`pool.rs`): `CeloExchangeRateApplier` looks up the exchange rate from `FeeCurrencyDirectory` contract, checks ERC20 balance, validates base fee floor and min tip, then overwrites `max_fee_per_gas`/`max_priority_fee_per_gas` with native equivalents.
2. **Block building** (`payload.rs`): `CeloFeeCurrencyFilter` enforces per-currency gas limits (default 50% of block).
3. **Execution** (`celo-revm` handler): Pre-tx debit of gas cost in fee currency, post-tx credit of refund. On failure, currency is added to the blocklist.
4. **Receipt** (`alloy-celo-evm`): `Cip64Storage` passes fee currency info from execution to receipt builder.

## E2E Tests

Located in `e2e_test/`. Run with `e2e_test/run_all_tests.sh` — it builds celo-reth, starts a dev node, funds test accounts, and runs all `test_*.sh` scripts automatically. JavaScript tests use viem in `e2e_test/js-tests/`.
