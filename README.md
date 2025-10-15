# Celo-Kona

This project wraps and extends [Kona](https://github.com/op-rs/kona) as well as its dependencies to support features specific to the Celo blockchain. This means creating a CeloEVM type that supports [Fee Abstraction](https://specs.celo.org/fee_abstraction.html) (via the CIP-64 tx type) and [Token Duality](https://specs.celo.org/token_duality.html) (via the transfer precompile) and making it usable in all parts of Kona.

Celo's [op-succinct fork](https://github.com/celo-org/op-succinct/) uses Celo-Kona to provide ZK fault proofs for the Celo blockchain.
