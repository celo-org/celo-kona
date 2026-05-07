# CLAUDE.md

## Project Overview

This repo contains two things built on a shared Celo EVM:

1. **celo-reth** — a full Celo L2 execution node, extending [op-reth](https://github.com/ethereum-optimism/optimism/tree/develop/rust/op-reth) (the OP Stack reth node) with Celo-specific pool, payload, and RPC logic.
2. **kona crates** — Celo extensions to [Kona](https://github.com/ethereum-optimism/optimism/tree/develop/rust/kona) (the OP Stack ZK/fault proof client), used by Celo's [op-succinct fork](https://github.com/celo-org/op-succinct/) for ZK fault proofs.

The core Celo EVM additions are **Fee Abstraction** (CIP-64 tx type — pay gas in ERC20 tokens) and **Token Duality** (native CELO ↔ ERC20 via transfer precompile). These live in `celo-revm` and `alloy-celo-evm`, shared by both celo-reth and kona.

## Build & Test Commands

```bash
just build-native          # Build workspace (alias: just b)
just test                  # Run all tests with nextest (alias: just t)
just lint-native           # Format check + clippy + doc lint (alias: just l)
just fmt-native-fix        # Fix formatting with nightly (alias: just f)
just hack                  # cargo hack check --no-default-features (verifies no-std builds)
cargo test -p <crate>      # Test single crate
cargo build -p celo-reth   # Build single crate
```

Requires: nightly Rust toolchain (for `cargo +nightly fmt`), `cargo-nextest`, `cargo-hack`.

Excluded from workspace operations (`{{exclude_members}}` in the Justfile):
`crates/kona/registry` and `examples/execution-fixture`.

The workspace `default-members` are the three binaries in `bin/` — a bare
`cargo build` builds those, not the whole workspace.

## Architecture

### Crate Dependency Flow

```
bin/celo-reth (L2 execution node binary)
  └── crates/celo-reth (reth node configuration)
        ├── alloy-celo-evm (alloy-evm wrapper)
        │     └── celo-revm (revm fork with Celo handlers)
        │           └── celo-alloy/consensus (CeloTxEnvelope, TxCip64)
        └── celo-alloy/rpc-types (CeloTransactionRequest)

bin/host, bin/client (FPVM proof host + client)
  └── crates/kona/* (Celo wrappers around kona)
        └── alloy-celo-evm
              └── celo-revm

bin/execution-verifier
  └── crates/kona/executor
```

### Binaries (`bin/`)

These are the workspace's `default-members` — a bare `cargo build` builds all three.

- **`bin/host`** (`celo-host`): Wraps `kona-host`. Runs the preimage oracle server and drives `celo-client`.
- **`bin/client`** (`celo-client`): FPVM client program that executes the Celo state transition. Compiles no-std for the FPVM target.
- **`bin/execution-verifier`**: Replays a range of L2 blocks fetched from an RPC endpoint through `celo-executor` and asserts the result matches. Thin wrapper around the executor crate, used to verify celo-kona execution parity against a reference node.

### `celo-reth` (the L2 execution node)

`celo-reth` is a full Celo L2 execution node — sequencer in dev mode, full
node in production. It is the largest and most-touched crate in the repo and
is built as the [`celo-reth`] binary at `crates/celo-reth/src/bin/celo_reth.rs`.
The crate is structured as a node-builder customization layered on top of
`reth-optimism-node`: each Celo-specific divergence from OP Stack lives in
its own module, and unchanged behaviour is delegated back to op-reth.

Library/binary split: the binary requires `std`, but the library half (the
primitives, receipt, and signed-tx types) is `#![cfg_attr(not(feature = "std"), no_std)]`
so kona crates can depend on the same primitive types.

Modules under `crates/celo-reth/src/`:

- **`lib.rs`** — `CeloEvmConfig` (analogous to `OpEvmConfig`). Wires
  `CeloEvmFactory`, `CeloRethReceiptBuilder`, and `Cip64Storage` together,
  and implements both `ConfigureEvm` (for sequencer block building) and
  `ConfigureEngineEvm` (for engine API payload execution / derivation).
  Also defines `celo_next_block_base_fee`, which applies the 25 Gwei base
  fee floor pre-Jovian and falls through to the chain spec's
  `min_base_fee` extra-data field post-Jovian.

- **`node.rs`** — Node-builder component wiring:
  - `CeloNode` (`NodeTypes` impl) and `CeloEngineTypes` (mirrors `OpEngineTypes`
    but with `Block = CeloBlock`).
  - `CeloPoolBuilder`: registers CIP-64 (tx type `0x7b`) and wraps the
    validator with `CeloExchangeRateApplier` (defined in `pool.rs`).
  - `CeloExecutorBuilder`: builds the `CeloEvmConfig`.
  - `CeloConsensus` / `CeloConsensusBuilder`: header validation that uses
    `celo_next_block_base_fee` so the 25 Gwei floor is enforced when
    checking parent → child base fee transitions.
  - A custom dev-mode payload attributes builder that zeroes the OP-mainnet
    deposit tx's L1 fee scalars and base fees, so dev-mode receipts report
    `l1Fee = 0` (matching op-geth's Celo fork).

- **`pool.rs`** — Fee-currency-aware tx pool:
  - `CeloPoolTx` wraps `OpPooledTransaction` and overrides
    `max_fee_per_gas` / `max_priority_fee_per_gas` to return native-equivalent
    values for CIP-64 transactions, so the pool's base-fee check
    (`ENOUGH_FEE_CAP_BLOCK`) and replacement check (`is_underpriced`) work
    across currencies.
  - `CeloExchangeRateApplier`: validator that looks up the exchange rate
    from `FeeCurrencyDirectory`, checks the ERC20 balance, validates base
    fee floor and min tip, and caches the native-equivalent fees on
    `CeloPoolTx`.
  - Pool ordering uses `CoinbaseTipOrdering` over native-equivalent fees
    (deliberate simplification vs op-geth's `CompareWithRates`).

- **`payload.rs`** — Block-building filtering:
  - `FeeCurrencyLimits`: per-fee-currency fraction-of-block-gas limits
    (default 50%, configurable per currency, native CELO is unlimited).
  - `CeloPayloadTransactions` / `CeloFeeCurrencyFilter`: wraps the pool's
    best-transactions iterator and skips txs whose fee currency has used
    its allotment.
  - **Important**: these limits apply during *sequencing* only. During
    derivation, `ConfigureEngineEvm::tx_iterator_for_payload` (in `lib.rs`)
    bypasses `CeloPayloadTransactions` entirely — derived blocks execute
    every tx in the payload regardless of per-currency totals.

- **`rpc.rs`** — Celo RPC type set:
  - `CeloRpcTypes`, `CeloReceiptConverter`, `CeloEthApiBuilder` — the Celo
    equivalents of op-reth's `Optimism`, `OpReceiptConverter`, and
    `OpEthApiBuilder`.
  - Custom `eth_gasPrice` that returns the 25 Gwei base fee floor.
  - CIP-64 receipt conversion (surfaces `feeCurrency`, `baseFeeInFeeCurrency`,
    etc. via `CeloTransactionReceipt`).

- **`chainspec/`** — Celo chain spec parser. Embeds zstd-compressed
  `mainnet.json.zst` / `sepolia.json.zst` genesis blobs (using a shared
  dictionary lifted from `celo-org/superchain-registry`), plus
  `mainnet.toml` / `sepolia.toml` for post-snapshot fork schedules
  (Holocene, Isthmus, Jovian). Adds the `Gingerbread` and `Cel2` Celo
  hardforks that the upstream OP parser drops.

- **`primitives.rs`** / **`signed_tx.rs`** / **`receipt.rs`** /
  **`receipts.rs`** — Node primitives type set: `CeloPrimitives`,
  `CeloTransactionSigned`, `CeloConsensusTx`, the bloomless `CeloReceipt`
  storage type, and `CeloRethReceiptBuilder` (produces `CeloReceipt` from
  CIP-64 execution data via `Cip64Storage`).

- **`test_utils.rs`** — Test-only helpers (e.g. mock chain specs) used by
  unit tests across the crate.

### Other Key Crates

- **`celo-revm`**: Low-level EVM modifications. Custom handler that intercepts CIP-64 txs to debit/credit fee currency via ERC20 calls. Contains the `FeeCurrencyContext`, contract ABIs for `FeeCurrencyDirectory`, and the transfer precompile. Must compile for `riscv32imac-unknown-none-elf` (no-std).

- **`alloy-celo-evm`**: Wraps `celo-revm` into `alloy-evm`'s `Evm` trait (`CeloEvm`). Adds the fee currency blocklist (blocks currencies whose debit/credit calls fail during block building, with time-based eviction). Contains `CeloEvmFactory` and `Cip64Storage` for passing fee info to receipt builder.

- **`celo-alloy`**: Consensus types (`CeloTxEnvelope`, `TxCip64`, `CeloReceipt`), network types, and RPC types. Workspace subcrates under `crates/celo-alloy/`.

- **`celo-otel`**: Shared OpenTelemetry setup (tracing-subscriber + OTLP exporter, metrics, resource detection). Used by the binaries for logging/telemetry; not pulled into the no-std crates.

- **`kona`**: Celo-specific wrappers around Kona's `driver`, `executor`, `genesis`, `proof`, `protocol`, and `registry` crates for ZK proof generation. Each lives at `crates/kona/<name>/`.

### No-std Support

`celo-revm`, `alloy-celo-evm`, `celo-alloy/*`, `kona/*`, and `bin/client` must compile without `std`. Use `#[cfg(feature = "std")]` to gate std-only code.

`celo-reth` is split: the binary (`celo_reth.rs`) and the node-builder modules (`node`, `pool`, `payload`, `rpc`, `chainspec`) are std-only, but the primitive modules (`primitives`, `receipt`, `receipts`, `signed_tx`, `lib.rs`'s `CeloEvmConfig`) are no-std-compatible so kona crates can share the same types. `celo-otel` and `bin/host`, `bin/execution-verifier` are std-only.

Verify with `just hack`.

### Fee Currency Flow (CIP-64)

1. **Pool entry** (`pool.rs`): `CeloExchangeRateApplier` looks up the exchange rate from `FeeCurrencyDirectory` contract, checks ERC20 balance, validates base fee floor and min tip, then overwrites `max_fee_per_gas`/`max_priority_fee_per_gas` with native equivalents.
2. **Block building** (`payload.rs`): `CeloFeeCurrencyFilter` enforces per-currency gas limits (default 50% of block).
3. **Execution** (`celo-revm` handler): Pre-tx debit of gas cost in fee currency, post-tx credit of refund. On failure, currency is added to the blocklist.
4. **Receipt** (`alloy-celo-evm`): `Cip64Storage` passes fee currency info from execution to receipt builder.

## E2E Tests

Located in `e2e_test/`. Run with `e2e_test/run_all_tests.sh` — it builds celo-reth, starts a dev node, funds test accounts, and runs all `test_*.sh` scripts automatically. JavaScript tests use viem in `e2e_test/js-tests/`.
