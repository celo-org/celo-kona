<h1 align="center">
Celo Kona
</h1>

<p align="center">
  <a href="#whats-celo-kona">What's Celo Kona?</a> •
  <a href="#overview">Overview</a> •
  <a href="#msrv">MSRV</a> •
  <a href="#credits">Credits</a> •
  <a href="#license">License</a>
</p>

## What's Celo Kona?

Celo-Kona extends the [kona][kona] project with Celo specific extensions, like the `transfer`-precompile and an implementation of CIP-64.

## Overview

**Binaries**

- [`client`](./bin/client): The bare-metal program that executes the Celo state transition, to be run on a prover.
- [`host`](./bin/host): The host program that runs natively alongside the prover, serving as the [Preimage Oracle][g-preimage-oracle] server.
- [`execution-verifier`](./bin/execution-verifier): A tool for verifying execution of Celo blocks.

**Crates**

- [`celo-alloy`](./crates/celo-alloy): Celo specific `alloy` extensions.
- [`alloy-celo-evm`](./crates/alloy-celo-evm): Celo specific `alloyevm` extensions.
- [`celo-otel`](./crates/celo-otel): OpenTelemetry utilities for logging and telemetry.
- [`celo-revm`](./crates/celo-revm): Variant of revm with Celo specific extensions.
- [`kona`](./crates/kona): Celo specific wrappers around kona structures.

## MSRV

The current MSRV (minimum supported rust version) is `1.86`.

The MSRV is not increased automatically, and will be updated
only as part of a patch (pre-1.0) or minor (post-1.0) release.

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
[kona]: https://github.com/op-rs/kona
[op-alloy]: https://github.com/alloy-rs/op-alloy
[kona-monorepo]: https://github.com/op-rs/kona/tree/develop
[op-go-monorepo]: https://github.com/ethereum-optimism/optimism/tree/develop
[badboi-cannon-rs]: https://github.com/BadBoiLabs/cannon-rs
[op-labs]: https://github.com/ethereum-optimism
[bad-boi-labs]: https://github.com/BadBoiLabs
[g-preimage-oracle]: https://specs.optimism.io/fault-proof/index.html#pre-image-oracle
