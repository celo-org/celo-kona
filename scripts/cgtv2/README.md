# CGTv2 upgrade fixture checks

`python3 scripts/cgtv2/verify_fixture.py` checks the frozen NUT/provenance files and the executor's populated legacy prestate. Pass `--optimism /path/to/the/op-node-checkout` to compare the complete Go fixture without cloning another repository.

The regular Rust CI test job executes the NUT group through the real EVM. It checks required-step success, order/completeness, reserve funding, repeat rejection, vault configuration/balance preservation, and candidate rollback. The separate Kona derivation change adds `bin/client/tests/cgtv2_activation.rs`, which derives from an authenticated L1 receipt trie, compares the stateful executor and trie-backed prover state/receipts, and runs the actual single-chain proof entry point against the expected output root.

The old `scripts/upgrade18_genesis_parity/` and `scripts/gen_upgrade18_predeploys.py` describe the retired state-override artifact. They are retained as historical reference and are not CGTv2 upgrade-deposit validation. `predeploys.json` is not consumed by the new migration.

These checks cover the synthetic chain 1337 fixture. Before release, supply and review the production reserved bridge address, L1 addresses, owner, legacy vault and gas-token settings, fixed total supply, deployer nonce/collision checks, populated chain state and gas limits. Regenerate the bundle in the contracts repository and rerun the same checks against the final artifacts. Production devnet parity is still required once those inputs are available.

The direct-upgrade alternative contains 15 static deposits plus reserve funding. It preserves the v6 ProxyAdmin implementation and ownership and uses no Karst manager helpers. Verify pinned legacy getter settings before freezing a production bundle.

Initializer resets and StorageSetter are omitted. The controller and bridge proxy must be unused; the FeeVault OZ v5 initializer slot must be zero even though its legacy slot-zero totalProcessed is populated. Existing initializer guards reject an already-initialized target.

## Register a production bundle

Rust selects `crates/celo-alloy/consensus/upgrades/cgtv2/{mainnet,sepolia,chaos}.rlp`. Go selects `op-node/rollup/derive/upgrades/cgtv2/{mainnet,sepolia,chaos}.json`. Each network has its own compiled path. The checked-in production files are empty placeholders and return an unconfigured-bundle error; production never falls back to chain 1337's fixture.

After generating and reviewing a network NUT JSON with the contracts team's production generator, install both client artifacts from that same input:

```bash
python3 scripts/cgtv2/import_bundle.py \
  --network mainnet \
  --bundle /path/to/cgtv2-mainnet-nut-bundle.json \
  --optimism /path/to/op-node-checkout
```

Use `sepolia` or `chaos` for the other supported profiles. The command requires Foundry's `cast` for Keccak, writes the JSON bytes unchanged for Go, and encodes the 15 static deposits as an RLP list for Rust. It checks the schema, agreed intents/order, addresses, calldata and gas bounds before writing. No RPC or on-chain write occurs. Runtime Rust code decodes the compiled RLP and needs no JSON reader or new dependency.

The frozen RLP list contains the same ordinary EIP-2718 deposit bytes as the existing per-transaction fixture files. Reserve funding stays dynamic. Both clients compute the one-time gas allowance from the selected static deposits plus the 100,000 funding gas, and subtract that same allowance when recovering SystemConfig from the activation parent.

Replacing the network artifacts requires no selector or gas-constant code change while the agreed 15-step protocol/intents stay unchanged. The fixed L1 bridge address in each network parameter table still needs its final deployed address. Review the contracts generator's captured settings/snapshot and artifact provenance before importing; importing a file does not establish production readiness.

Run importer tests with `python3 -m unittest discover -s scripts/cgtv2 -p 'test_import_bundle.py' -v`. The fixture parity check also binds the compiled fixture RLP list to the individually hashed transaction files.
