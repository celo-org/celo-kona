# celo-kona

Celo's Rust execution client and fault-proof stack.

This repository extends the OP Stack Rust components, [op-reth][op-reth] and [Kona][kona], with the
two features that separate Celo from a stock OP chain:

- **[Fee Abstraction][fee-abstraction]** — pay gas in an ERC20 token, via the CIP-64 transaction
  type.
- **[Token Duality][token-duality]** — native CELO and its ERC20 representation share one balance,
  via the transfer precompile.

Both live in [`celo-revm`](./crates/celo-revm) and [`alloy-celo-evm`](./crates/alloy-celo-evm), and
both artifacts below build on them.

## What this repository ships

**[`celo-reth`](./crates/celo-reth)** is the Celo L2 execution client. It wraps op-reth's node
builder and substitutes Celo's EVM, transaction pool, payload builder, chain spec and RPC types.
Node operators run this.

**[`celo-client`](./bin/client)** and **[`celo-host`](./bin/host)** are the fault-proof programs.
`celo-client` executes the Celo state transition inside an FPVM; `celo-host` serves it preimages.
Celo's [op-succinct fork][op-succinct] uses both to produce ZK fault proofs.

## Overview

**Binaries**

- [`celo-reth`](./crates/celo-reth/src/bin/celo_reth.rs): Celo L2 execution client.
- [`celo-client`](./bin/client): Client program for executing the Celo rollup state transition.
- [`celo-host`](./bin/host): Host program that runs natively alongside the prover, serving as the
  [Preimage Oracle][g-preimage-oracle] server.
- [`execution-verifier`](./bin/execution-verifier): Replays a range of L2 blocks from an RPC
  endpoint through `celo-executor` and checks that the result matches.

**Crates**

*Node*

- [`celo-reth`](./crates/celo-reth): Celo node configuration for reth: EVM config, pool, payload
  builder, consensus, chain spec, RPC and node primitives.

*Celo EVM (shared by the node and the proof programs)*

- [`celo-revm`](./crates/celo-revm): Variant of revm with Celo handlers, the fee currency context
  and the transfer precompile.
- [`alloy-celo-evm`](./crates/alloy-celo-evm): Celo EVM behind `alloy-evm`'s `Evm` trait, plus the
  fee currency blocklist.
- [`celo-alloy`](./crates/celo-alloy): Celo consensus, network and RPC types
  ([`consensus`](./crates/celo-alloy/consensus), [`network`](./crates/celo-alloy/network),
  [`rpc-types`](./crates/celo-alloy/rpc-types),
  [`rpc-types-engine`](./crates/celo-alloy/rpc-types-engine)).

*Proof*

- [`kona/derive`](./crates/kona/derive): Derivation-pipeline extensions, including Espresso
  event-based batch authentication.
- [`kona/driver`](./crates/kona/driver): `no_std` derivation pipeline driver.
- [`kona/executor`](./crates/kona/executor): `no_std` stateless block builder.
- [`kona/genesis`](./crates/kona/genesis): Celo genesis and rollup config types.
- [`kona/proof`](./crates/kona/proof): Celo Proof SDK.
- [`kona/protocol`](./crates/kona/protocol): Celo protocol types.
- [`kona/registry`](./crates/kona/registry): Superchain config registry.

*Utilities*

- [`celo-otel`](./crates/celo-otel): OpenTelemetry setup for logging, tracing and metrics.

## Running celo-reth

See [Run a Celo node][run-node] for hardware requirements, Docker images, datadir bootstrapping and
network configuration.

`celo-reth` serves Celo Mainnet (`--chain celo`) and Celo Sepolia (`--chain celo-sepolia`); genesis
for both is embedded in the binary. To build it from source:

```bash
cargo build --release -p celo-reth --bin celo-reth
```

A bare `cargo build` builds the three fault-proof binaries instead: they are the workspace's
`default-members`.

On top of the op-reth command set, `celo-reth` adds `import-celo-state` and overrides `download`,
`snapshot-manifest` and `celo-migrate-v2`. All four exist because Celo Mainnet migrated from an L1:
blocks below the migration height are header-only placeholders, so any upstream path that rebuilds
an index from block 1 has to be taught where the real history starts. Run `celo-reth --help` for
the full set.

## Development

Requires the nightly toolchain (for `cargo +nightly fmt`), [`just`][just],
[`cargo-nextest`][nextest] and [`cargo-hack`][hack]. MSRV is 1.94.

```bash
just setup           # install the pre-commit fmt hook
just build-native    # build the workspace          (alias: just b)
just test            # run tests with nextest       (alias: just t)
just lint-native     # fmt check + clippy + rustdoc (alias: just l)
just fmt-native-fix  # format with nightly          (alias: just f)
just hack            # check the no-std builds      (alias: just h)
```

`celo-revm`, `alloy-celo-evm`, `celo-alloy/*`, `kona/*` and `bin/client` must compile without
`std`, because the client program targets the FPVM. `just hack` verifies that.

End-to-end tests live in [`e2e_test/`](./e2e_test). `e2e_test/run_all_tests.sh` builds `celo-reth`,
starts it in dev mode, funds test accounts and runs every `test_*.sh` script. It needs
[Foundry][foundry] (`cast` and `forge`) on the `PATH`.

Upstream is pinned: the `kona-*` and `reth-optimism-*` crates come from the
[Optimism monorepo][op-go-monorepo] at tag `op-reth/v2.3.1`, and the `reth-*` crates from a pinned
[reth][reth] revision.

## Credits

`celo-kona` is based on the work of several teams, namely [OP Labs][op-labs] and other
contributors' work on the [kona monorepo][kona-monorepo], [Optimism monorepo][op-go-monorepo],
[Paradigm][paradigm]'s work on [reth][reth], and [BadBoiLabs][bad-boi-labs]'s work on
[Cannon-rs][badboi-cannon-rs].

## License

Licensed under the [MIT license.](https://github.com/op-rs/kona/blob/main/LICENSE.md)

> [!NOTE]
>
> Contributions intentionally submitted for inclusion in these crates by you
> shall be licensed as above, without any additional terms or conditions.

<!-- Links -->

[op-reth]: https://github.com/ethereum-optimism/optimism/tree/develop/rust/op-reth
[kona]: https://github.com/ethereum-optimism/optimism/tree/develop/rust/kona
[kona-monorepo]: https://github.com/op-rs/kona/
[op-go-monorepo]: https://github.com/ethereum-optimism/optimism/
[badboi-cannon-rs]: https://github.com/BadBoiLabs/cannon-rs
[op-labs]: https://github.com/ethereum-optimism
[paradigm]: https://github.com/paradigmxyz
[reth]: https://github.com/paradigmxyz/reth
[bad-boi-labs]: https://github.com/BadBoiLabs
[op-succinct]: https://github.com/celo-org/op-succinct/
[run-node]: https://docs.celo.org/infra-partners/operators/run-node
[fee-abstraction]: https://specs.celo.org/fee_abstraction.html
[token-duality]: https://specs.celo.org/token_duality.html
[g-preimage-oracle]: https://specs.optimism.io/fault-proof/index.html#pre-image-oracle
[just]: https://github.com/casey/just
[nextest]: https://nexte.st
[hack]: https://github.com/taiki-e/cargo-hack
[foundry]: https://getfoundry.sh
