# CGTv2 activation from an L1-origin signal

Status: design proposal for team review, created 2026-10-21 and updated 2026-10-23. The shared Rust execution milestone is implemented in [celo-kona #241](https://github.com/celo-org/celo-kona/pull/241) at commit `72e5bb46e4bfaa734f4a19830a7031b86657bc8b`, rebased onto main at `9253817`. L1 contract changes, authenticated signal derivation, and full node/prover integration remain pending.

## Current implementation

- Implemented: canonical version-1 upgrade deposit; compiled network parameters and checked `R = S - A`; ordered, staged migration in the shared celo-reth/Kona executor; normal setter execution and deposit receipt; rollback of failed or discarded candidates; fee-vault configuration/initializer migration with balances and `totalProcessed` preserved; removal of timestamp activation and operator-supplied migration parameters.
- Implemented: ordinary L2 user transactions are excluded from the activation block in celo-reth payload building/import/unsafe execution, Kona execution, and the shared executor used by incremental witness/debug builds. L1 deposits keep their order and the next payload job resumes ordinary pool selection.
- Only synthetic chain ID `1337` has migration parameters (`S=1000`, `A=600`, `R=400` in the tests). Real network addresses, final storage layouts/artifacts, and S remain unresolved; activation with unsupported network parameters fails explicitly.
- Pending: permanent L1 snapshot and atomic L1 bundle; authenticated feature/snapshot/bootstrap reads; op-node and Kona signal construction and singular/span batch checks; positive signal-based proof fixtures and actual node/prover devnet parity. The executor checks signal encoding and origin consistency but does not authenticate A or establish the L1 event boundary by itself.

The sections below specify the complete target protocol, including the pending integration. The implementation status above describes the Rust executor milestone; the full L1-triggered migration still requires the remaining contract, derivation, proof, and devnet work.

## Goal and scope

Activate the L2 CGTv2 migration when its L1 origin reflects the completed L1 migration. Every node and proof client must use the same inputs and apply the same transition at the same L2 block. No activation timestamp is needed in the intended design.

The existing shared Celo block executor will install the L2 bridge contracts and fund `NativeAssetLiquidity`. This work does not include automatic recovery of pending legacy withdrawals or direct refunds to users on L2.

## Activation rule

Let `H` be the exact L1 origin block hash carried by an L2 block and `parent` its L2 parent.

The first L2 block adopting the migration origin must activate CGTv2 at the ordered activation signal described below, if the parent's state has no migration completion marker. The origin's end-of-block `SystemConfig.isFeatureEnabled(CUSTOM_GAS_TOKEN)` must be true. A failed read or missing proof is an error, never an implicit `false`. Once the transition is complete, later blocks must not repeat it.

Use the v6 feature flag. The legacy `isCustomGasToken()` getter already returns true for Celo before this migration and cannot identify the migration boundary. See [SystemConfig at the examined contract revision](https://github.com/celo-org/optimism/blob/20bb4480868ce6071d9b4a27bc6b5557755f54b6/packages/contracts-bedrock/src/L1/SystemConfig.sol).

Celo's sequencer is configured to select finalized L1 origins. This controls origin selection; local wall-clock time or a node's current finalized label must not become an additional replay condition. The transition is tied to `H` and parent state.

### One-time enable invariant (decided)

On the canonical L1 history, the v6 `CUSTOM_GAS_TOKEN` feature is disabled before migration, enabled exactly once inside the atomic Tx1-through-Tx4 migration transaction, and remains enabled afterward. This is the agreed migration assumption for implementation.

Tx1 through Tx4 refer to the four L1 tasks in the [examined migration script](https://github.com/celo-org/optimism/blob/20bb4480868ce6071d9b4a27bc6b5557755f54b6/packages/contracts-bedrock/scripts/cgt/MigrateV1V2.s.sol#L260-L265): move Portal funds through `PortalMigrator`, repoint `SystemConfig.superchainConfig`, run the OPCM v6 upgrade, and enable the CGT feature. Execute all four within one outer atomic L1 transaction. The existing script returns four separate tasks; producing and validating that outer batch is pending contract work.

Scan successful authenticated origin receipts in canonical transaction/log order. The first log emitted by the configured `SystemConfig` that matches `FeatureSet(CUSTOM_GAS_TOKEN, true)` is the unique activation boundary. Insert the signal at that position in the deposit sequence. This event determines both the enabled state and the position under the one-time enable invariant. Finding it completes the CGT event search; continue processing all Portal deposit logs in the origin, including those after the boundary. Reorg/reset handling follows the canonical branch and parent state as described below.

## Shared op-node attributes construction

Both sequencing and safe derivation must construct the same activation signal for the same `(parent, H)`.

- The sequencer selects `H` and passes it to `PreparePayloadAttributes`.
- Derivation obtains the origin from batch metadata and invokes the attributes builder for that origin. Batches do not need to contain the L1-info transaction itself.
- Unsafe P2P payloads contain the full transaction list, including the L1-info deposit and the proposed activation signal. The receiving execution client can read the origin and signal from that payload. Unsafe execution remains provisional until safe derivation checks the expected attributes.

Use the shared attributes-construction path. A signal supplied only as a temporary Engine API field would be lost during P2P propagation, historical import, and replay. See [attributes construction](https://github.com/celo-org/optimism/blob/1ead7226f8314c7150004c66875740f81065eced/op-node/rollup/derive/attributes.go), [sequencer](https://github.com/celo-org/optimism/blob/1ead7226f8314c7150004c66875740f81065eced/op-node/rollup/sequencing/sequencer.go), and [unsafe payload origin decoding](https://github.com/celo-org/optimism/blob/1ead7226f8314c7150004c66875740f81065eced/op-node/rollup/derive/payload_util.go).

### Feature state and migration amount

Read inputs at `H`, not `latest`:

1. Determine whether the single CGT enable event occurs in `H`, starting from the authenticated bootstrap state below. Under the one-time enable invariant, the feature remains enabled through the end of `H`.
2. Read the new L1 bridge's permanent migrated-amount snapshot `A` and require `escrowSeeded == true`.
3. Compute the L2 reserve seed `R = S - A`. `S` is the fixed migration CELO total supply, hardcoded in shared protocol code so celo-reth and Kona use the same value. Only `A` is fetched from L1; `S` is not supplied by RPC or operator configuration. Reject `A > S`.

The L1 contract change must store the amount actually moved from the Portal once, atomically with the transfer and `seedEscrow()`. Its ordinary deposit accounting changes during later bridging and cannot serve as the snapshot. `escrowSeeded` distinguishes an uninitialized zero from a completed migration with a zero amount. The permanent snapshot field is a required contract change, not an existing field in the [examined bridge](https://github.com/celo-org/optimism/blob/20bb4480868ce6071d9b4a27bc6b5557755f54b6/packages/contracts-bedrock/src/celo/CeloGasBridgeL1.sol).

The event handler detects the first matching CGT enable log described above and records its authenticated receipt/log position. Logs from other emitters or for other features do not identify activation. The current [SystemConfig receipt parser](https://github.com/celo-org/optimism/blob/1ead7226f8314c7150004c66875740f81065eced/op-node/rollup/derive/system_config.go) handles `ConfigUpdate`, so this requires new handling.

### L1 snapshot interface and fixed proof inputs (decided)

Add `uint256 public migratedAmount` to `CeloGasBridgeL1`, exposing `migratedAmount() returns (uint256)`. In `seedEscrow(amount)`, store the actual amount transferred from the Portal in this field together with `escrowSeeded = true` and the initial escrow accounting. Preserve the existing one-time seeding guard. The actual transfer and these writes must commit in the same atomic L1 migration transaction. Later deposits and withdrawals update the ordinary escrow accounting while the permanent snapshot remains unchanged.

Use the existing `escrowSeeded() returns (bool)` getter to distinguish an uninitialized snapshot from a completed migration with an amount of zero. At activation, read both values at the exact origin hash `H`, require `escrowSeeded == true`, and verify the account/storage proofs against `H.stateRoot` before using the proven `migratedAmount` as `A`.

Fix the L1 bridge proxy address in each network's protocol/chain configuration. Op-node and Kona use the same configured address for the snapshot proof. Celo-reth uses that same network-specific bridge address when initializing `CeloGasBridgeL2.otherBridge`; it is not an additional signal payload field. Keep the `SystemConfig` proxy address fixed per network as well.

Derive the snapshot and seeded-status storage slot numbers, packed-field byte offsets, and decoding from the Solidity compiler's `storageLayout` for the final L1 contract revision. Pin those definitions alongside the contract source/artifact revision and use the same definitions in Go and Rust. Proofs refer to storage at the configured bridge proxy address. Populate the actual network addresses and final slot/offset constants from the deployment and compiler artifacts before release; these values are deployment inputs still to be supplied, while the fixed-address/fixed-layout approach is decided.

### Feature-state bootstrap and reset (decided)

On startup or derivation reset, op-node and Kona initialize feature tracking from the L2 parent block at which execution resumes:

1. Read the `CeloGasBridgeL2` account in that parent's L2 state. Keep #241's existing completion marker: nonempty code at `0x4200000000000000000000000000000000001023`. If the marker is present, perform the additional L2 flag consistency check described below once for this bootstrap/reset, then skip CGT-specific L1 flag initialization and tracking.
2. If the code marker is absent, let `B` be that parent's recorded L1 origin hash. Read the L1 CGT flag's end-of-block state at `B` and verify its account/storage proof against `B.stateRoot`. Use the authenticated origin header; never substitute the node's current `latest` or finalized head. Missing data or an invalid proof is an error, not `false`.
3. Starting from that disabled state, watch for the single matching CGT enable event as derivation advances to subsequent L1 origins. Process each origin once, in canonical transaction/log order, and retain the matching event's position for signal construction. If the next L2 block has the same origin `B`, reuse the initialized state without replaying `B`'s events.
4. If the parent's own L1 origin already has the L1 CGT feature enabled but the parent has no completion marker, reject the parent as inconsistent. Do not insert a delayed migration into a later block. This does not reject the normal activation case: a parent with an earlier, disabled origin and no marker may have a child that first adopts the enabled migration origin.
5. Keep tracked feature state tied to its L1 origin hash. On a reset or reorg, discard tracking from the old path and initialize again from the new L2 parent's origin. This tracking is a cache, not an independent source of consensus state.

For example, when resuming before migration after L2 block 1000 whose L1 origin is block 200 with hash `B`, authenticate the disabled L1 flag at `B`. When the origin advances to block 201, inspect its receipts for the single matching enable event. If block 1000 already has the code marker, perform the one-time L2 flag check and skip L1 feature bootstrap. If its origin `B` is enabled but the code marker is absent, reject the inconsistent parent.

## Signal transport and execution order (decided)

Use one canonical, protocol-generated deposit transaction as the activation signal. Its data contains `version`, `l1OriginHash`, and `migratedAmount` (`A`). Include the version field even though this migration initially has only one format. Bind the origin hash to `H`; op-node and Kona obtain `A` from the authenticated L1 bridge snapshot and the executor computes the reserve seed as hardcoded `S - A`.

The signal's deposit `mint` and `value` are both zero. It must not introduce a separate native mint or value transfer. Reject malformed signals, an origin hash inconsistent with the block's L1-info transaction, duplicate activation signals, and an activation signal after migration is already complete. Safe derivation and proving must also reject a missing or misplaced required signal by reconstructing the expected signal and its position from L1 inputs.

Op-node derives the signal from L1 inputs; celo-reth consumes it from the block without its own L1 provider. The actual contract installation and reserve funding remain an irregular state transition in the shared Celo executor, applied at the signal's transaction position.

Preserve the L1 migration boundary within the L2 deposit sequence. Tx1 through Tx4 are planned as one atomic L1 transaction, so the migration's `FeatureSet(CUSTOM_GAS_TOKEN, true)` event identifies that transaction's receipt and its position in the origin block. Merge the activation signal with Portal deposit events in canonical receipt/log order:

```text
L1-info transaction
Portal deposits before the migration boundary, executed against pre-migration state
Activation signal: install predeploys, fund the reserve, then call setCustomGasToken()
Portal deposits after the migration boundary, executed against post-migration state
Other scheduled protocol upgrade transactions in their specified order
No ordinary L2 user transactions in this block
```

For deposits in separate L1 transactions, the receipt containing the first matching CGT enable event determines which side of the boundary they belong to. If the migration transaction itself emits Portal deposit events, their position relative to that enable event determines which side they belong to. Derive these positions from the authenticated receipt/log sequence rather than trusting independent RPC index fields. Under the one-time enable invariant, this first match is the migration event and fixes the signal position.

The executor may pre-validate the signal's encoding and placement, but must apply the state transition only when transaction execution reaches the signal. Do not migrate in block pre-execution or apply migration while pre-scanning the payload. The earlier block-start proposal would execute preceding legacy deposits against upgraded code/state and is superseded by this ordered transition. The shared executor now applies migration at the signal rather than in `apply_pre_execution_changes`. Celo-reth and Kona execution use this same path; independent signal reconstruction in derivation is still pending.

Keep the L1-info transaction first and preserve the relative order of all user deposits. Do not defer `H`'s deposits to the next L2 block: that changes epoch deposit rules, and the next block may use a different origin. `noTxPool` does not suppress derived L1 deposits. The pause and CGT flag are expected to prevent post-migration CELO/ETH funding deposits, but zero-value Portal calls remain possible and must keep their correct position.

The CGTv2 activation block must not contain ordinary L2 user transactions, including CIP-64 transactions. Sequencing must set `noTxPool = true` when the derived transaction list contains the activation signal. Batch validation must reject user transactions in that block, including singular and span batches. Execution must identify the signal across the complete transaction list so this rule applies before and after it; reuse the upstream no-user-transaction execution guard, including its protocol post-exec exception. Keep all derived L1 deposits in their canonical order. The next block resumes ordinary pool selection. The Rust payload builder also forces `noTxPool` and rejects forced user transactions, even if the caller omitted the flag. Incremental builders and witness/debug execution also enforce the rule in the shared executor: reject activation after any committed user transaction, and enable the upstream guard when activation commits. Candidate metadata lives in the execution result so failed or discarded candidates do not change the block policy.

### Signal transaction fields and encoding (decided)

Use the existing `UpgradeDepositSource` helper unchanged, with the fixed intent string `Celo: CGTv2 Activation`. This is a normal type-`0x7e` deposit with upgrade source-hash domain `2`. Do not add the origin hash or event index to the helper's source-hash calculation. The origin is carried in the payload, and derivation determines the signal's position from authenticated L1 receipts. See [the existing helper](https://github.com/celo-org/optimism/blob/c9d5a6d69e1ec7efce7d453cc4cd9a424804bb9a/rust/op-alloy/crates/consensus/src/source.rs#L104-L131).

```text
sourceHash = UpgradeDepositSource("Celo: CGTv2 Activation").sourceHash()
from       = DEPOSITOR_ACCOUNT (0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001)
to         = L1Block predeploy (0x4200000000000000000000000000000000000015)
mint       = 0
value      = 0
gasLimit   = 100_000
isSystemTx = false
data       = selector("setCustomGasToken()")
             || abi.encode(uint256(1), bytes32(l1OriginHash), uint256(migratedAmount))
```

The initial payload version is `1`. Calldata is exactly 100 bytes: the four-byte setter selector followed by three 32-byte ABI words. Reject other lengths, unsupported versions, or any mismatch in the required transaction fields. Validate the amount and origin before applying migration. The existing argument-free Solidity setter does not consume the trailing protocol data; the shared executor must decode and validate it. No new reserved target address or Solidity entry point is needed. This follows the sender/target pattern of [Ecotone's protocol-generated setter call](https://github.com/celo-org/optimism/blob/c9d5a6d69e1ec7efce7d453cc4cd9a424804bb9a/op-node/rollup/derive/ecotone_upgrade_transactions.go#L115-L124).

### Setter execution, gas, and receipt (decided)

At the signal's ordered position:

1. Validate the canonical signal and require that migration has not already completed.
2. Before replacing the fee-vault implementation, capture its existing configuration as described below. Stage the predeploy installation, reserve funding, and complete fee-vault configuration/initializer migration in the shared executor. Initialize the new `L1BlockCGT` CGT flag to `false` at this intermediate stage. Make the new code and storage visible to the EVM before the setter executes, including any accounts cached by earlier transactions in the block.
3. Execute the signal through the ordinary deposit transaction path, calling the installed `L1BlockCGT.setCustomGasToken()`. This setter checks the depositor sender, requires the new flag to be false, and sets it to true.
4. Require successful execution and commit the migration changes together with the deposit's state changes and normal receipt. Validation errors, DB errors, and setter revert/halt fail the block or build attempt. Never continue with a failed activation receipt or retain a partial migration. Discarded execution candidates must not retain migration changes or a completion marker.

The staged artifact initializes the flag to `false`; the setter performs the final false-to-true transition. See [the current artifact](https://github.com/celo-org/celo-kona/blob/72e5bb46e4bfaa734f4a19830a7031b86657bc8b/crates/alloy-celo-evm/res/predeploys.json) and [the existing setter](https://github.com/celo-org/optimism/blob/db220657e2fb8ad81b4ca676193e60433a7d27ef/packages/contracts-bedrock/src/L2/L1BlockCGT.sol#L59-L70). The final state must still include an enabled flag and the completed reserve/predeploy transition.

Use the fixed gas limit `100_000`; implementation must verify it against the pinned contract artifact and supported execution rules before deployment. Keep ordinary deposit gas accounting rather than forcing zero gas usage. Calldata and setter execution contribute their normal gas usage to the transaction receipt and block accounting. Direct client-applied predeploy installation and reserve funding remain an irregular state transition outside EVM gas metering. Deposit execution does not charge the sender a gas fee.

Reuse the existing Celo deposit receipt builder and encoding, including success status, cumulative gas used, depositor nonce, and deposit receipt version under the active fork rules. Preserve the ordinary deposit sender-nonce increment. Do not synthesize a separate receipt format, force a gas-used value, or fabricate migration logs. The current setter emits no event. See [deposit execution and receipt rules](https://specs.optimism.io/protocol/deposits.html#execution).

Appending the signal after every deposit would violate the boundary when post-migration deposit calls are present. The setter call does not change the L1-ordered placement rule above.

### Fee-vault upgrade and initialization (decided)

Keep the `CeloSequencerFeeVault` upgrade at `0x4200000000000000000000000000000000000011` in the CGTv2 migration. Complete its configuration and proxy initializer state in the same atomic L2 transition as the implementation change. The staged migration now captures the old getter values and installs the new configuration and initializer state, addressing [the existing review](https://github.com/celo-org/celo-kona/pull/241#issuecomment-6036761249). The populated legacy-upgrade genesis-parity harness remains a separate integration check.

Before replacing the implementation, read `recipient()`, `minWithdrawalAmount()`, and `withdrawalNetwork()` through the existing vault implementation against the L2 state immediately before the activation signal. These are deterministic read-only calls in the shared executor, using the same state in celo-reth and Kona. Earlier deposits in the activation block may already have changed that state. Preserve getter values even when the old implementation keeps them as immutables. Failed reads or execution errors fail activation.

Migrate those existing settings into the new implementation's storage fields, including the packed recipient/network field. Complete the proxy's OpenZeppelin initializer state so that it matches a successful `initialize` call for the pinned implementation: initialized version `1` and no initialization in progress. Include these writes in the staged migration diff using the pinned contract layout and initializer storage definition. Subsequent `initialize()` calls must revert. See [the new settings and initializer](https://github.com/celo-org/optimism/blob/db220657e2fb8ad81b4ca676193e60433a7d27ef/packages/contracts-bedrock/src/L2/FeeVault.sol#L27-L37) and [the initializer body](https://github.com/celo-org/optimism/blob/db220657e2fb8ad81b4ca676193e60433a7d27ef/packages/contracts-bedrock/src/L2/FeeVault.sol#L81-L96).

Preserve the vault's native CELO balance, ERC-20 balances, and `totalProcessed` at the signal boundary. Keep the same proxy address and preserve all other unaffected account/storage state. Updating the implementation and initializing settings must not sweep balances, mint replacement funds, or reset the processed counter. Commit these changes together with the rest of activation; any failure rolls the staged transition back.

The migration artifact and parity/regression coverage must model an upgrade from the legacy vault with populated configuration, a nonzero balance, and a nonzero `totalProcessed`. Check the migrated getters and initializer state, rejection of a second initialization, preservation of balances/counters, and the resulting native/token withdrawal behavior.

## Completion, restart, and reorg behavior (decided)

Keep #241's code marker as the authority for migration completion: the `CeloGasBridgeL2` account at `0x4200000000000000000000000000000000001023` has nonempty code in the relevant L2 parent state. Preserve this check in the executor; no new completion storage field is needed.

At bootstrap/reset only, if the code marker is present, additionally read the new `L1BlockCGT` flag from the same L2 parent state and require its value to be `1`. The slot at the L1Block predeploy (`0x4200000000000000000000000000000000000015`) is `0xd2ff82c9b477ff6a09f530b1c627ffb4b0b81e2ae2ba427f824162e8dad020aa`. This is an L2 consistency check, separate from the L1 `SystemConfig` feature flag. Reject a present code marker with an unset flag. Do not use the legacy getter as a substitute before the new contracts are installed. See [the new flag's storage and getter](https://github.com/celo-org/optimism/blob/db220657e2fb8ad81b4ca676193e60433a7d27ef/packages/contracts-bedrock/src/L2/L1BlockCGT.sol#L19-L35).

Limit this additional flag check to bootstrap/reset; do not add a flag storage read to every block. The setter's successful execution is already required at activation. A present marker with an unset flag cannot be a valid committed state under the atomic activation rule; the bootstrap/reset check detects inconsistent input state or an implementation error.

Celo-reth can read the parent account metadata and flag slot from its state provider. Op-node can request account/storage proofs at the exact `parentHash` and verify them against the parent's `stateRoot`. Kona must authenticate the same reads through the parent-state trie witness. Code presence requires only the account's code hash, not its full bytecode. This adds a bounded account/slot read at initialization, with no L1 RPC request for the L2 flag check and no scan of state or history.

When the code marker is absent, use the L1 feature bootstrap/reset procedure above. Reorgs roll the marker and migration effects back with the rest of that state. Any cached completion result must stay tied to the parent branch and be reinitialized on reset or reorg. A cache is not an independent authority. After completion, normal block processing skips CGT-specific L1 flag tracking and snapshot reads; a later bootstrap/reset still performs the one-time L2 consistency check. Normal receipt processing for L1 deposits continues.

## Kona and proving

Kona must independently construct the same signal from authenticated inputs. It must not trust an amount or enabled flag supplied by a host or RPC response.

- Verify origin receipts against `H.receiptsRoot` through Kona's existing receipt-preimage path. Find the first matching CGT enable event and reconstruct the activation signal's position relative to deposit events from those authenticated receipts.
- Verify the bootstrap flag account/storage proof against the resumed L2 parent's L1 origin header `B.stateRoot`. Then authenticate subsequent origin receipts against their headers to detect the single CGT enable event. At activation, verify the permanent amount and seeded status against `H.stateRoot`. Both header hashes must be bound to the corresponding L1 origins.
- Use the network's fixed bridge and SystemConfig proxy addresses and the pinned slot/offset definitions above. Verify the snapshot and seeded-status proofs at that bridge proxy against `H.stateRoot`. Reject missing or invalid proofs.
- Pass the resulting signal to the shared executor, producing the same state root as celo-reth.

The existing [Kona L1 provider](https://github.com/celo-org/optimism/blob/c9d5a6d69e1ec7efce7d453cc4cd9a424804bb9a/rust/kona/crates/proof/proof/src/l1/chain_provider.rs) authenticates receipt data. The bridge-state proof path is additional work. Op-node can use exact-hash verified storage reads; see [its L1 client](https://github.com/celo-org/optimism/blob/1ead7226f8314c7150004c66875740f81065eced/op-service/sources/eth_client.go).

## Operational pause and unpause (decided)

Keep the L1 `SystemConfig` pause active across the atomic Tx1-through-Tx4 L1 migration and L2 activation. An operator manually calls `unpause()` after all of the following checks pass:

1. An independent verifier has safely derived the activation block `C` from L1 inputs. Confirm that `C` is the same block hash on that verifier's canonical safe chain, with its safe head at or beyond `C`.
2. At `C`, verify the installed bridge contracts and their configuration against the fixed network configuration, including the L1/L2 bridge counterpart addresses.
3. At `C`, verify that the new L2 `L1BlockCGT` flag is enabled.
4. Verify that activation applied reserve funding `R = S - A`, using the authenticated L1 snapshot at origin `H` and the fixed total supply `S`. Account for any pre-existing liquidity and other legitimate state changes at the signal/block boundary when checking the result. Tie the inspection to the activation transition and block hash; later reserve movements do not change the expected activation funding.

Record the activation block hash, verifier safe-chain confirmation, and state-check results in the migration runbook before manually lifting the pause. These are operational release conditions. Client and prover activation remains determined by the canonical origin, signal position, and L2 state described above.

## Remaining deployment inputs and implementation checks

1. Finalize the updated L1 contract/artifact revision containing `migratedAmount()`. Supply the actual per-network bridge and SystemConfig proxy addresses and compiler-derived slot/offset constants. The fixed-address/fixed-layout approach is decided above.
2. Supply the exact hardcoded migration total-supply constant `S`, in native base units, for the supported networks. Its source in shared protocol code is decided above.
3. Record each network's legacy fee-vault configuration in upgrade fixtures and verify its preservation and initialized post-state.
4. Verify the fixed `100_000` signal gas limit, trailing-calldata compatibility with the pinned setter, and the required node/prover parity and regression cases below.

The protocol and operational decisions discussed above are recorded. These remaining items provide concrete release values and implementation evidence.

## Delivery and acceptance checks

Keep #241 as the L2 migration implementation; it is rebased onto main at `9253817` and has removed timestamp authority from its chain/proof configuration. Implement the flag-based op-node changes in a new Optimism PR replacing the timestamp direction in [#471](https://github.com/celo-org/optimism/pull/471), then add matching Kona derivation/proof integration. The archived timestamp smoke scripts and replay fixtures are historical material and negative configuration coverage, not evidence of the new signal-based migration working end to end.

Required coverage:

- **Authenticated activation inputs:** pre-enable origin, first enabled origin, repeated origin, first-matching-enable detection with unrelated emitter/feature logs, bootstrap after enable, invalid snapshot/proof, missing seeded status, and duplicate/missing/misplaced signals. Check that both snapshot and seeded-status proofs use the same origin state root, their slot decoders match the pinned getters, unseeded zero is rejected, and ordinary bridging leaves the snapshot unchanged.
- **Signal and atomic execution:** exact transaction fields and 100-byte encoding, trailing-calldata compatibility with the pinned setter, fixed gas limit, ordinary deposit nonce/receipt accounting, and rollback on setter or DB failure. Only the setter enables the new flag; the final installed state must match the intended artifact state.
- **Ordered deposits:** a preceding legacy deposit with a contract call that depends on pre-migration behavior, and a subsequent deposit call that depends on the installed bridge. Preserve their relative order and apply migration exactly at the signal.
- **Fee vault:** migrate existing settings and complete initializer state; reject repeated initialization; preserve native/token balances and `totalProcessed`; verify native/token withdrawals using the preserved settings. Include populated legacy-upgrade genesis parity.
- **Bootstrap/reset/reorg:** completed marker with flag true, present marker with flag false, enabled parent origin without a marker, and normal child activation from an earlier parent origin. Additional L2 flag checks run only at bootstrap/reset, reorgs invalidate cached completion, and CGT-specific L1 reads stop after completion.
- **Deposits-only activation block:** reject ordinary native and CIP-64 transactions before or after the signal, including forced payload transactions and the raw witness/debug route. Failed or discarded candidates must not change the block policy. Cover corresponding singular/span batch rejection and a following block that resumes user transactions.
- **Node/prover agreement:** sequencing, unsafe import, safe derivation, and stateless proof replay must agree on activation block, signal position, receipts, state root, and output root using real signal-based fixtures and devnet execution.
