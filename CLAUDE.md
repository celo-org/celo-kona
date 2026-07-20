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

Full Celo L2 execution node, built as a node-builder customization on top of `reth-optimism-node` — Celo divergences live in their own modules, everything else delegates to op-reth. The binary is at `src/bin/celo_reth.rs`; the library's primitive modules are no-std so kona can share the same types.

Modules under `crates/celo-reth/src/`:

- **`lib.rs`** — `CeloEvmConfig` (≈ `OpEvmConfig`): wires `CeloEvmFactory` + `CeloRethReceiptBuilder` + `Cip64Storage`, and implements `ConfigureEvm` (sequencing) and `ConfigureEngineEvm` (derivation). Also `celo_next_block_base_fee`: 25 Gwei floor pre-Jovian, chain-spec `min_base_fee` post-Jovian.
- **`node.rs`** — Component wiring: `CeloNode`, `CeloEngineTypes`, `CeloPoolBuilder` (registers CIP-64 type `0x7b`, wraps validator with `CeloExchangeRateApplier`), `CeloExecutorBuilder`, `CeloConsensus` (header validation enforces the 25 Gwei floor). Also a dev-mode payload attributes builder that zeroes the OP-mainnet deposit tx's L1 fee scalars so dev receipts report `l1Fee = 0`.
- **`pool.rs`** — `CeloPoolTx` wraps `OpPooledTransaction` and overrides `max_fee_per_gas`/`max_priority_fee_per_gas` with native-equivalent values so base-fee and replacement checks work across currencies. `CeloExchangeRateApplier` is the validator (rate lookup, balance check, fee conversion). Ordering uses `CoinbaseTipOrdering` over native equivalents — simpler than op-geth's `CompareWithRates`.
- **`payload.rs`** — `FeeCurrencyLimits` + `CeloPayloadTransactions`: per-currency fraction-of-block-gas limits (default 50%, native CELO unlimited). **Sequencing only** — derivation uses `tx_iterator_for_payload` in `lib.rs` and bypasses these limits entirely.
- **`rpc.rs`** — `CeloRpcTypes`, `CeloReceiptConverter`, `CeloEthApiBuilder` (Celo equivalents of op-reth's `Optimism`/`OpReceiptConverter`/`OpEthApiBuilder`). Custom `eth_gasPrice` returns the 25 Gwei floor; CIP-64 receipts surface `feeCurrency`, `baseFeeInFeeCurrency`, etc.
- **`chainspec/`** — Celo chain spec parser. Embeds zstd-compressed `mainnet.json.zst`/`sepolia.json.zst` (shared dictionary from `celo-org/superchain-registry`) plus `mainnet.toml`/`sepolia.toml` for post-snapshot forks (Holocene, Isthmus, Jovian). Adds `Gingerbread` and `Cel2` hardforks the upstream OP parser drops.
- **`primitives.rs`** / **`signed_tx.rs`** / **`receipt.rs`** / **`receipts.rs`** — Node primitives: `CeloPrimitives`, `CeloTransactionSigned`, `CeloConsensusTx`, bloomless `CeloReceipt`, `CeloRethReceiptBuilder` (consumes `Cip64Storage`).

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

## Dependency Bumps and Upstream Ports

A dependency bump is not complete when the manifests compile. Celo owns wrappers, adapters, and modified copies of upstream code, so every bump must include a semantic audit of the full upstream range.

1. Resolve the exact old and new refs from `Cargo.toml`, `Cargo.lock`, tags, and git revisions. Never audit a dependency's current `main` or `HEAD` in place of the pinned versions.
2. Scan every relevant commit in every source repository whose packages moved. This includes the Optimism monorepo's Kona, op-reth, op-alloy, op-revm, and alloy-op-evm crates, plus alloy-rs/core, alloy-rs/alloy, alloy-rs/eips, bluealloy/revm, and paradigmxyz/reth when their resolved versions or revisions change. Include transitive family changes revealed by the lockfile.
3. Inventory Celo-owned counterparts before classifying changes. At minimum inspect `bin/client`, `bin/host`, `crates/kona/*`, `crates/celo-reth`, `crates/alloy-celo-evm`, `crates/celo-alloy/*`, and `crates/celo-revm`, including files that copy, wrap, adapt, or reimplement upstream behavior.
4. Classify every relevant upstream commit as `PORT`, `INHERIT`, or `N/A`, with a reason. `INHERIT` means the bumped dependency executes the fixed code directly. `PORT` means Celo owns a separate implementation or adapter that must receive the same semantic change. A compile fix alone is not evidence that the audit is complete.
5. Make each semantic port a separate, reviewable commit. Preserve the upstream implementation's structure, naming, control flow, and tests as closely as possible. Change only what Celo-specific types, APIs, hardfork rules, or behavior require, and do not mix opportunistic refactors into the port.
6. Name the exact upstream source in the port commit message. Use the full `owner/repository@SHA`, state the upstream purpose, explain why the dependency bump does not inherit it, and describe the minimal Celo adaptation. For example:

   ```text
   Upstream: paradigmxyz/reth@<full-sha> (<upstream subject>)

   celo-kona owns a separate <copy/adapter>, so the dependency bump does not
   apply this change there. Keep the upstream implementation intact except for
   <specific Celo adaptation>.
   ```

   Put durable Celo invariants or intentional semantic differences in code comments. The commit message is still required for provenance; do not rely on a code comment alone.
7. Keep an audit ledger in the working notes or PR description so reviewers can see the complete scanned range and the `PORT`/`INHERIT`/`N/A` decision for each relevant change. Do not add otherwise-unused dependencies merely to mirror an upstream manifest.
8. After changing a git tag or revision, clean the affected workspace packages before trusting incremental results, then run the verification depth required by this project, including no-std or dependency checks when the changed surfaces require them.
