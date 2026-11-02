# Min base fee keeper

Keeps Celo's base fee floor (`SystemConfig.minBaseFee` on L1, applied from Jovian) at a
USD target: a USDT transfer paid in USDT (115,501 gas) costs $0.002 at the floor.

The SystemConfig owner Safe enables a Zodiac Roles v2 module. Its only permission lets
one keeper call `setMinBaseFee` with a value in [25, 2000] Gwei.

The keeper runs daily. It reads CELO/USD from Chainlink on Celo Mainnet and cross-checks
it against Coinbase. It sends nothing when the two prices differ by more than 10% or the
change is below 1%.

## Setup

```bash
export NETWORK=sepolia L1_RPC_URL=<ethereum sepolia rpc> CAST_WALLET_ARGS="--account keeper"
./deploy-roles.sh                                  # prints the Roles proxy address
./safe-batch.sh <roles> <keeper-address> > batch.json
```

Import `batch.json` into the Safe Transaction Builder and execute it with the Safe.

## Run

```bash
ROLES=<roles> ./keeper.sh --dry-run
ROLES=<roles> ./keeper.sh
```

`keeper.sh` exits non-zero when a human should look: stale or disagreeing prices, a floor
outside the bounds, or a failed tx.

## Change the target or bounds

- `TARGET_ATTO_USD_PER_TX` and `REFERENCE_GAS` are keeper settings (env or `common.sh`).
- The bounds are part of the Roles permission. To change them, the Safe re-runs the
  `scopeFunction` tx from `safe-batch.sh` with new `MIN_FLOOR`/`MAX_FLOOR`.
- To revoke the keeper, the Safe calls `assignRoles(keeper, [role], [false])` or
  `disableModule`.

## Test

```bash
FORK_URL=<l1 rpc> NETWORK=mainnet ./fork-test.sh
```

Forks L1 with anvil and impersonates the Safe. It deploys the module, executes the batch
and runs the keeper. It checks that the bounds are inclusive and that out-of-bounds
values, other functions, other targets and non-members are rejected.
