# Min base fee keeper

Keeps Celo's base fee floor (`SystemConfig.minBaseFee` on L1, applied from Jovian) at a
USD target: a USDT transfer paid in USDT (115,501 gas) costs $0.002 at the floor.

The SystemConfig owner Safe enables a Zodiac Roles v2 module. It lets one keeper call
`setMinBaseFee` with a value in [25, 2000] Gwei, and disable the module. Nothing else.

The keeper (`keeper/`, TypeScript) runs daily as a Google Cloud Function. It reads CELO/USD
from Chainlink on Celo Mainnet and cross-checks it against Coinbase. It sends nothing when
the two prices differ by more than 10% or the change is below 1%.

## Setup

```bash
export NETWORK=sepolia L1_RPC_URL=<ethereum sepolia rpc> CAST_WALLET_ARGS="--account keeper"
./deploy-roles.sh                                  # prints the Roles proxy address
./safe-batch.sh <roles> <keeper-address> > batch.json
```

Import `batch.json` into the Safe Transaction Builder and execute it with the Safe.

## Run

The keeper reads its settings from env: `NETWORK`, `L1_RPC_URL`, `ROLES` and
`KEEPER_PRIVATE_KEY` (not needed with `DRY_RUN=true`). Optional overrides and their
defaults are in `loadConfig` in `keeper/src/keeper.ts`.

Locally:

```bash
cd keeper && npm ci && npm run build
ROLES=<roles> node dist/cli.js --dry-run
ROLES=<roles> KEEPER_PRIVATE_KEY=<key> node dist/cli.js
```

It fails (exit code 1 locally, HTTP 500 and an `ERROR` log entry as a function) when a human
should look: stale or disagreeing prices, a floor outside the bounds, or a failed tx.

As a Cloud Function, with the key and RPC URL in Secret Manager:

```bash
gcloud functions deploy min-base-fee-keeper --gen2 --runtime=nodejs22 --region=<region> \
  --source=keeper --entry-point=keeper --trigger-http --no-allow-unauthenticated --timeout=300s \
  --set-env-vars=NETWORK=mainnet,ROLES=<roles> \
  --set-secrets=L1_RPC_URL=<rpc-secret>:latest,KEEPER_PRIVATE_KEY=<key-secret>:latest
gcloud scheduler jobs create http min-base-fee-keeper --location=<region> \
  --schedule='0 12 * * *' --uri=<function-url> --http-method=POST --attempt-deadline=320s \
  --oidc-service-account-email=<invoker-service-account>
```

Alert on `ERROR` entries from the function and on days without a run.

## Change the target or bounds

- `TARGET_ATTO_USD_PER_TX` and `REFERENCE_GAS` are keeper settings (function env). Changing
  them needs no Safe tx.
- The bounds are part of the Roles permission. To change them, the Safe re-runs the
  `scopeFunction` tx from `safe-batch.sh` with new `MIN_FLOOR`/`MAX_FLOOR`. The keeper
  needs no change: it simulates each call and fails when the Roles module rejects it.
- To revoke the keeper, the Safe calls `assignRoles(keeper, [role], [false])` or
  `disableModule`.

## Stop

```bash
ROLES=<roles> ./disable.sh
```

Signed with the keeper key, it disables the Roles module on the Safe. Only the Safe can
re-enable it.

## Test

```bash
FORK_URL=<l1 rpc> NETWORK=mainnet ./fork-test.sh
```

Forks L1 with anvil and impersonates the Safe. It deploys the module, executes the batch
and runs the keeper. It checks that the bounds are inclusive and that out-of-bounds
values, other functions, other targets and non-members are rejected. Then it checks that the
keeper can disable the Roles module but no other module.
