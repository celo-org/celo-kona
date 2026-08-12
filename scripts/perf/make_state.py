#!/usr/bin/env python3
"""Generate a dev genesis with a large synthetic state, to give the trie realistic depth.

Why this exists: the stock dev genesis has 31 accounts, so its state trie is ~2 levels deep and
every read is a cache hit. Measurements taken on it (trie seeks per build, state-root time,
persistence cost) are all floor values that say nothing about a real chain. This produces the same
chain config with N extra accounts, so a build does a realistic number of trie reads per changed
leaf, without needing a snapshot download.

What it gives you and what it does not:

  * Realistic *depth*. A hex MPT over N leaves is about log16(N) deep: 31 accounts is ~1.5 levels,
    100k is ~4.2, 1M is ~5, and a 10M-account chain is ~5.8. So 1M gets most of the way to
    mainnet's per-read work, and the seeks-per-changed-leaf figure becomes meaningful.
  * NOT realistic cache pressure. 1M accounts is a few hundred MB, and this host has 32 GiB, so
    everything still fits in the page cache. Exceeding RAM needs O(100M) accounts or the cache
    shaping arm (msync/mlock) from the plan. Do not read a cache-miss result off this.

Addresses are the first 20 bytes of sha3_256(index), which spreads them uniformly across the trie
the way real addresses are spread. Sequential addresses would share long prefixes and produce an
unrealistically shallow, narrow trie. These accounts are never signed for, so the hash not being
keccak256 does not matter -- only the distribution does.

Usage:
    scripts/perf/make_state.py --accounts 1000000 --out /tmp/big-genesis.json
    scripts/perf/make_state.py --accounts 200000 --contracts 500 --slots 64 --out ...

The output is written streaming, because a 1M-account alloc is ~90 MB of JSON and building it as
one dict first is a needless several-GB spike.
"""

import argparse
import hashlib
import json
import sys
from pathlib import Path

# Enough to send transactions from, small enough not to distort the supply.
DEFAULT_BALANCE = "0x21e19e0c9bab2400000"  # 10,000 ether
# Minimal non-empty runtime code, so a slotted account is a real contract with a code hash rather
# than an EOA that implausibly has storage.
STUB_CODE = "0x6001600101"


def address(index: int, domain: bytes) -> str:
    """Deterministic, uniformly distributed 20-byte address for `index` within `domain`."""
    digest = hashlib.sha3_256(domain + index.to_bytes(8, "big")).digest()
    return "0x" + digest[:20].hex()


def slot_key(index: int) -> str:
    """A 32-byte storage key, uniformly spread so storage tries get depth too."""
    return "0x" + hashlib.sha3_256(b"slot" + index.to_bytes(8, "big")).digest().hex()


def write_genesis(base: dict, out: Path, accounts: int, contracts: int, slots: int) -> None:
    """Stream `base` back out with `accounts` EOAs and `contracts` slotted contracts added."""
    original = base.get("alloc", {})
    # Emit every key except `alloc` normally, then hand-roll `alloc` so it can stream.
    head = {k: v for k, v in base.items() if k != "alloc"}

    with out.open("w") as fh:
        fh.write("{\n")
        for key, value in head.items():
            fh.write(f"  {json.dumps(key)}: {json.dumps(value)},\n")
        fh.write('  "alloc": {\n')

        first = True

        def emit(addr: str, entry: dict) -> None:
            nonlocal first
            if not first:
                fh.write(",\n")
            first = False
            fh.write(f"    {json.dumps(addr)}: {json.dumps(entry, separators=(',', ':'))}")

        # The base alloc goes first and is never overwritten: it carries the prefunded test keys
        # and the predeploys the node needs to start at all.
        taken = {a.lower() for a in original}
        for addr, entry in original.items():
            emit(addr, entry)

        added = 0
        for i in range(accounts):
            addr = address(i, b"eoa")
            if addr.lower() in taken:  # astronomically unlikely; correctness is free here
                continue
            emit(addr, {"balance": DEFAULT_BALANCE})
            added += 1
            if added % 100_000 == 0:
                print(f"  {added:,} accounts", file=sys.stderr, flush=True)

        for c in range(contracts):
            addr = address(c, b"contract")
            if addr.lower() in taken:
                continue
            storage = {slot_key(c * slots + s): "0x" + (s + 1).to_bytes(32, "big").hex()
                       for s in range(slots)}
            emit(addr, {"balance": "0x0", "code": STUB_CODE, "storage": storage})

        fh.write("\n  }\n}\n")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--base", type=Path,
                        default=Path(__file__).resolve().parents[2] / "e2e_test"
                        / "celo-dev-genesis.json",
                        help="genesis to extend (default: the dev genesis)")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--accounts", type=int, default=1_000_000,
                        help="synthetic EOAs to add (default 1000000, ~5 trie levels)")
    parser.add_argument("--contracts", type=int, default=1000,
                        help="slotted contracts to add, for storage-trie depth")
    parser.add_argument("--slots", type=int, default=32, help="storage slots per contract")
    args = parser.parse_args()

    base = json.loads(args.base.read_text())
    print(f"base: {len(base.get('alloc', {}))} alloc entries, chainId "
          f"{base.get('config', {}).get('chainId')}", file=sys.stderr)
    write_genesis(base, args.out, args.accounts, args.contracts, args.slots)

    size = args.out.stat().st_size
    total = len(base.get("alloc", {})) + args.accounts + args.contracts
    print(f"wrote {args.out} ({size / 1e6:.1f} MB), {total:,} alloc entries, "
          f"{args.contracts * args.slots:,} storage slots", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
