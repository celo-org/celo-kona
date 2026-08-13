#!/usr/bin/env python3
"""Generate a transfer recipient list that isolates trie *insertion* from trie *update*.

Why this exists: celo-blockchain-planning#1453's confirmed driver is new-account insertion —
`HashedAccounts` growth goes 0.23/s to 67.3/s at the onset of the excursion, ~67 brand-new
keccak-uniform leaves per second into a 112.5 M-leaf account trie, while `HashedStorages` growth
*falls*. The proposed mechanism is that a brand-new leaf forces a cold root-to-leaf descent plus a
branch split, where an update to an account already in the trie reuses paths that are already
resident. That mechanism is inference from timing and shape; the issue's own hypothesis graveyard
lists the competing explanation ("sender scatter / first-pass amortisation") as set aside but never
independently re-derived.

The two explanations make different predictions, and one recipient list separates them:

    fresh               every recipient is an address that does not exist in the trie, so every
                        transfer INSERTS a leaf. Uniformly spread, so no path locality.
    existing-scattered  every recipient already exists in genesis, and no address repeats, so every
                        transfer UPDATES a leaf. Uniformly spread, so also no path locality.
    existing-hot        the same `--cycle` existing addresses in every block, so every transfer
                        updates a leaf AND the paths repeat block to block. Maximum locality.

`fresh` vs `existing-scattered` isolates insertion, because both scatter identically and differ only
in whether the leaf is new. `existing-scattered` vs `existing-hot` isolates locality, because both
update and differ only in whether the paths repeat. If insertion is the driver, `fresh` is dear and
both `existing-*` arms are cheap. If residency is the driver, `fresh` and `existing-scattered` cost
the same and only `existing-hot` is cheap.

Controls that hold by construction, so the comparison is of one variable:

  * Changed entries per block are identical in all three modes — one sender plus `--cycle` distinct
    recipients — because no mode repeats a recipient *within* a block. A repeat inside a block would
    silently drop the changed-account count and so change the per-entry denominator.
  * Gas per transaction is identical: a value transfer to a non-existent account costs the same
    21000 as one to an existing account (the 25000 account-creation charge applies to CREATE, not to
    value transfers).
  * The sender's own leaf is touched in every block of every mode, so it cancels.

Addresses use the same derivation as scripts/perf/make_state.py, so `existing-*` modes name accounts
that genesis actually allocated: EOA `i` is the first 20 bytes of sha3_256(b"eoa" + i). The `fresh`
domain is b"recipient", which make_state.py never emits.

Usage:
    scripts/perf/recipients.py --count 500 --mode fresh
    scripts/perf/recipients.py --count 500 --mode existing-scattered --accounts 2000000
    scripts/perf/recipients.py --count 500 --mode existing-hot --accounts 2000000 --cycle 25
"""

import argparse
import hashlib
import math
import sys

# Coprime with any account count that has no factor of 1_000_003, which makes `i * STRIDE % accounts`
# a bijection and therefore collision-free. Only distinctness needs this; the addresses are hashed,
# so index locality never becomes trie locality regardless.
STRIDE = 1_000_003

FRESH_DOMAIN = b"recipient"
EXISTING_DOMAIN = b"eoa"  # must match make_state.py's EOA domain


def address(index: int, domain: bytes) -> str:
    """Deterministic, uniformly distributed 20-byte address, as scripts/perf/make_state.py."""
    return "0x" + hashlib.sha3_256(domain + index.to_bytes(8, "big")).digest()[:20].hex()


def existing_index(i: int, accounts: int) -> int:
    """Index of a genesis-allocated EOA, distinct for every distinct `i` below `accounts`."""
    return (i * STRIDE) % accounts


def recipients(mode: str, count: int, accounts: int, cycle: int):
    if mode == "fresh":
        for i in range(count):
            yield address(i, FRESH_DOMAIN)
    elif mode == "existing-scattered":
        for i in range(count):
            yield address(existing_index(i, accounts), EXISTING_DOMAIN)
    elif mode == "existing-hot":
        # Position within the block decides the recipient, so block N and block N+1 touch exactly
        # the same `cycle` leaves. Distinct within a block, repeated across blocks.
        for i in range(count):
            yield address(existing_index(i % cycle, accounts), EXISTING_DOMAIN)
    else:
        raise ValueError(f"unknown mode {mode!r}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--count", type=int, required=True, help="how many recipients to emit")
    parser.add_argument("--mode", required=True,
                        choices=("fresh", "existing-scattered", "existing-hot"))
    parser.add_argument("--accounts", type=int, default=0,
                        help="how many EOAs make_state.py put in genesis; required unless --mode "
                             "fresh, since that is the pool the existing-* modes draw from")
    parser.add_argument("--cycle", type=int, default=25,
                        help="transactions per block: the repeat period for --mode existing-hot")
    args = parser.parse_args()

    if args.mode != "fresh":
        # Fail loudly rather than silently drawing from a pool that does not exist: an address that
        # is not in genesis turns an intended update arm into a second insertion arm, which would
        # look like a null result instead of a broken one.
        if args.accounts <= 0:
            parser.error(f"--mode {args.mode} needs --accounts (the genesis EOA count)")
        if args.count > args.accounts:
            parser.error(f"--count {args.count} exceeds the {args.accounts} EOAs in genesis")
        if math.gcd(STRIDE, args.accounts) != 1:
            parser.error(f"STRIDE {STRIDE} is not coprime with --accounts {args.accounts}, so the "
                         "index walk would repeat and blocks would hold fewer changed entries than "
                         "transactions")
        if args.mode == "existing-hot" and args.cycle > args.accounts:
            parser.error(f"--cycle {args.cycle} exceeds the {args.accounts} EOAs in genesis")

    for addr in recipients(args.mode, args.count, args.accounts, args.cycle):
        print(addr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
