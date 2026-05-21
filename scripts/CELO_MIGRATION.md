# Celo L2 Migration: celo-reth State Import

This document describes how to initialize a `celo-reth` node with the Celo L1 state dump for the Cel2 migration.

## Overview

The Celo L2 migration imports the full Celo L1 state (pre-migration) into `celo-reth` at migration block `31,056,500`. The state dump contains all existing Celo L1 accounts. The L2 allocs (OP Stack predeploys and Celo-specific contracts injected at migration time) must be merged into the dump before import.

The `celo-reth import-celo-state` subcommand handles all the chain-specific setup: it hardcodes the Celo Mainnet chain spec, the Cel2 migration header (block `31,056,500`, hash `0x7586...e5c5`), and dummy-fills blocks 1..migration_block-1 so the state-dump import lands on a valid tip.

> **Note:** `import-celo-state` is intercepted before the upstream op-reth CLI parser runs. It will not appear in the output of `celo-reth --help` — invoke it as the first argument: `celo-reth import-celo-state ...`.

## Prerequisites

- A built `celo-reth` binary (run `cargo build --release -p celo-reth` from the repo root, then use `./target/release/celo-reth`).
- `python3`, `curl`, `zstd`.
- ~70 GB of free disk space for the decompressed state dump and ~80 GB for the resulting datadir.

## Step 1: Download and Decompress the L1 State Dump

The compressed state dump (~15 GB zstd) is available at:
<https://storage.googleapis.com/cel2-rollup-files/celo/l1-final-state.json.zst>

```bash
curl -O https://storage.googleapis.com/cel2-rollup-files/celo/l1-final-state.json.zst
zstd -d l1-final-state.json.zst    # ~56 GB JSONL
```

## Step 2: Prepare the State Dump

The L1 state dump must be merged with the L2 migration allocs before import. The merge script applies the same logic as the Go migration (`applyAllocsToState` + `setupUnreleasedTreasury`):

- Fixes the zero-address dump bug (missing `address` field on the entry for `0x0`).
- Merges L2 alloc accounts with existing L1 accounts using allowlist rules.
- Sets the treasury balance to the post-migration value.
- Updates the state root in the JSONL header line to the Cel2 migration state root.

```bash
python3 scripts/append_l2_allocs.py /path/to/l1-final-state.json
```

This creates `/path/to/l1-final-state.json.with-allocs.jsonl` without modifying the original. You can also specify a custom output path:

```bash
python3 scripts/append_l2_allocs.py /path/to/l1-final-state.json /path/to/output.jsonl
```

### State dump format

The dump is JSONL with:

- Line 1: `{"root": "0x..."}` — must match the migration state root (`celo-reth` checks this before importing).
- Lines 2+: one account per line with `address`, `balance`, `nonce`, `code`, `storage` fields.

Extra fields from the L1 dump (`root`, `codeHash`) are silently ignored. Balance can be a decimal string (e.g. `"157500000000000"`) or hex (`"0x..."`). Nonce can be a JSON integer or hex string.

After importing all accounts, `celo-reth` computes the state root from the trie and verifies it matches the migration header (`0xed980641...`).

## Step 3: Initialize celo-reth

The dummy-chain setup opens many static-file segments. On macOS, raise the file-descriptor limit before running:

```bash
ulimit -n 10240
```

Then run:

```bash
celo-reth import-celo-state \
    --datadir=/path/to/datadir \
    /path/to/l1-final-state.json.with-allocs.jsonl
```

There is no `--chain` flag — this subcommand is Celo Mainnet–only. The chain spec, migration block (`31,056,500`), and migration header are hardcoded.

The command will:

1. Construct the Celo Mainnet chain spec internally and log the active hardforks + genesis hash.
2. Create a dummy chain up to block `31,056,499`.
3. Append the hardcoded Cel2 migration header at block `31,056,500`.
4. Stream all accounts from the state-dump JSONL into the database.
5. Compute the state root and verify it matches the migration header.

## Convenience wrapper

`scripts/prepare_celo_import.sh` automates all three steps:

```bash
./scripts/prepare_celo_import.sh /path/to/workdir /path/to/datadir
```

It downloads the dump (if missing), decompresses it, runs `append_l2_allocs.py`, and invokes `celo-reth import-celo-state`. Re-running with the same workdir is safe — completed steps are skipped.

## Limitations

- Only Celo Mainnet is supported. Testnet imports require a separate path (not yet implemented).
