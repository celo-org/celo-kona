# Celo-Kona

This project wraps and extends [Kona](https://github.com/op-rs/kona) as well as its dependencies to support features specific to the Celo blockchain. This means creating a CeloEVM type that supports [Fee Abstraction](https://specs.celo.org/fee_abstraction.html) (via the CIP-64 tx type) and [Token Duality](https://specs.celo.org/token_duality.html) (via the transfer precompile) and making it usable in all parts of Kona.

Celo's [op-succinct fork](https://github.com/celo-org/op-succinct/) uses Celo-Kona to provide ZK fault proofs for the Celo blockchain.

## Overview

**Binaries**

- [`client`](./bin/client): Client program for executing the Celo rollup state transition.
- [`host`](./bin/host): The host program that runs natively alongside the prover, serving as the [Preimage Oracle][g-preimage-oracle] server.
- [`execution-verifier`](./bin/execution-verifier): A tool for verifying execution of Celo blocks.

**Crates**

- [`celo-alloy`](./crates/celo-alloy): Celo specific `alloy` extensions.
- [`alloy-celo-evm`](./crates/alloy-celo-evm): Celo specific `alloy-evm` extensions.
- [`celo-otel`](./crates/celo-otel): OpenTelemetry utilities for logging and telemetry.
- [`celo-revm`](./crates/celo-revm): Variant of revm with Celo specific extensions.
- [`kona`](./crates/kona): Celo specific wrappers around `kona` structures.

## Credits

`celo-kona` is based on the work of several teams, namely [OP Labs][op-labs] and other
contributors' work on the [kona monorepo][kona-monorepo], [Optimism monorepo][op-go-monorepo] and
[BadBoiLabs][bad-boi-labs]'s work on [Cannon-rs][badboi-cannon-rs].

`kona` is also built on rust types in [alloy][alloy], [op-alloy][op-alloy], and [maili][maili].

## License

Licensed under the [MIT license.](https://github.com/op-rs/kona/blob/main/LICENSE.md)

> [!NOTE]
>
> Contributions intentionally submitted for inclusion in these crates by you
> shall be licensed as above, without any additional terms or conditions.

<!-- Links -->

[alloy]: https://github.com/alloy-rs/alloy
[maili]: https://github.com/op-rs/maili
[op-alloy]: https://github.com/alloy-rs/op-alloy
[kona-monorepo]: https://github.com/op-rs/kona/
[op-go-monorepo]: https://github.com/ethereum-optimism/optimism/
[badboi-cannon-rs]: https://github.com/BadBoiLabs/cannon-rs
[op-labs]: https://github.com/ethereum-optimism
[bad-boi-labs]: https://github.com/BadBoiLabs
[g-preimage-oracle]: https://specs.optimism.io/fault-proof/index.html#pre-image-oracle
