## `celo-alloy-network`

Built on [op-alloy-network][op-alloy-network], `celo-alloy-network` is Celo blockchain RPC behavior abstraction.

It adds CIP-64 (transaction type `0x7b`), which pays gas in an ERC20 fee currency instead of
native CELO.

### Building a provider

```rust,ignore
let provider = ProviderBuilder::new_with_network::<Celo>()
    .wallet(wallet)
    .connect_http(url);
```

`ProviderBuilder::new_with_network::<Celo>()` and `ProviderBuilder::new().network::<Celo>()`
both give you the recommended Celo fillers. Plain `ProviderBuilder::new()` gives you the
Ethereum network, so a Celo request would go out without `CeloGasFiller` and priced in native
fee suggestions.

### Fee currencies

Set `feeCurrency` on the request and the fee fields are denominated in that currency, not in
native wei:

```rust,ignore
let tx = CeloTransactionRequest::default()
    .to(recipient)
    .value(amount)
    .fee_currency(cusd);
```

`CeloGasFiller` fills the gas limit and both EIP-1559 fee fields from the node's
fee-currency-parameterized `eth_gasPrice` and `eth_maxPriorityFeePerGas`. A request carrying
`gasPrice` or an EIP-7702 `authorizationList` next to a fee currency is refused before any
RPC: those fields cannot coexist with CIP-64, and a node that signs the request itself may
drop the fee currency rather than reject it, charging a fee-currency price in CELO.

See `examples/send_cip64.rs` for a complete transfer.

[op-alloy-network]: https://crates.io/crates/op-alloy-network
