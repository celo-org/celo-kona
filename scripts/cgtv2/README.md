# CGTv2 upgrade fixture checks

`python3 scripts/cgtv2/verify_fixture.py` checks the frozen NUT/provenance files and the executor's populated legacy prestate. Pass `--optimism /path/to/the/op-node-checkout` to compare the complete Go fixture without cloning another repository.

The regular Rust CI test job executes the NUT group through the real EVM. It checks required-step success, order/completeness, reserve funding, repeat rejection, vault configuration/balance preservation, and candidate rollback. The separate Kona derivation change adds `bin/client/tests/cgtv2_activation.rs`, which derives from an authenticated L1 receipt trie, compares the stateful executor and trie-backed prover state/receipts, and runs the actual single-chain proof entry point against the expected output root.

The old `scripts/upgrade18_genesis_parity/` and `scripts/gen_upgrade18_predeploys.py` describe the retired state-override artifact. They are retained as historical reference and are not CGTv2 upgrade-deposit validation. `predeploys.json` is not consumed by the new migration.

These checks cover the synthetic chain 1337 fixture. Before release, supply and review the production reserved bridge address, L1 addresses, owner, legacy vault and gas-token settings, fixed total supply, deployer nonce/collision checks, populated chain state and gas limits. Regenerate the bundle in the contracts repository and rerun the same checks against the final artifacts. Production devnet parity is still required once those inputs are available.

The direct-upgrade alternative contains 19 static deposits plus reserve funding. It preserves the v6 ProxyAdmin implementation and ownership and uses no Karst manager helpers. Verify pinned legacy getter settings before freezing a production bundle.
