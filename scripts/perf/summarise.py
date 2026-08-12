#!/usr/bin/env python3
"""Summarise an engine-replay timings.jsonl: latency percentiles, node counters, phase split.

The driver writes one JSON object per replayed block. This turns a run into the three things worth
looking at, and nothing else:

  1. Latency percentiles per phase, plus the per-block sequence when asked. The sequence matters
     more than it sounds: a clustered tail and a periodic tail have completely different causes, and
     the aggregate cannot tell them apart. `--sequence` prints it.
  2. Node-side counters averaged per build, from `--metrics-url` deltas. Trie seeks per build live
     here. Anything whose mean is zero is omitted.
  3. Histogram `_sum`/`_count` pairs converted to a mean per build, which is how the payload phase
     split (execution / state root / finalization) is recovered. Rolling `quantile=` series are
     never usable this way and the driver already drops them.

Usage:
    scripts/perf/summarise.py timings.jsonl [--sequence] [--grep trie]
    scripts/perf/summarise.py a.jsonl b.jsonl --label stock --label big     # side by side
"""

import argparse
import json
import sys
from pathlib import Path

PHASES = ["fcu_build_us", "get_payload_us", "build_us", "new_payload_us", "fcu_advance_us"]


def percentile(values: list[float], q: float) -> float:
    """Nearest-rank percentile, matching the driver's own `percentile()` in replay.rs."""
    if not values:
        return 0.0
    ordered = sorted(values)
    idx = min(len(ordered) - 1, max(0, round(len(ordered) * q)))
    return ordered[idx]


def load(path: Path) -> list[dict]:
    rows = []
    for line in path.read_text().splitlines():
        line = line.strip()
        if line:
            rows.append(json.loads(line))
    return rows


def phase_table(rows: list[dict]) -> None:
    print(f"  {'phase':<16} {'p50':>9} {'mean':>9} {'p90':>9} {'p99':>9} {'max':>10} {'tail':>7}")
    for phase in PHASES:
        vals = [float(r[phase]) for r in rows if phase in r]
        if not vals:
            continue
        p50 = percentile(vals, 0.50)
        mean = sum(vals) / len(vals)
        ratio = (percentile(vals, 0.99) / p50) if p50 else 0.0
        print(f"  {phase:<16} {p50:>9.0f} {mean:>9.0f} {percentile(vals, 0.90):>9.0f} "
              f"{percentile(vals, 0.99):>9.0f} {max(vals):>10.0f} {ratio:>6.1f}x")

    gas = [r.get("gas_used", 0) for r in rows]
    txs = [r.get("txs_included", 0) for r in rows]
    matched = sum(1 for r in rows if r.get("hash_match"))
    print(f"  blocks {len(rows)}, hashes reproduced {matched}/{len(rows)}, "
          f"mean {sum(txs) / len(rows):.1f} txs and {sum(gas) / len(rows):,.0f} gas per block")


def sequence(rows: list[dict], threshold: float) -> None:
    print(f"  {'blk':>5} {'fcu_build':>10} {'getPayload':>11} {'newPayload':>11} {'advance':>9}"
          f" {'txs':>4}")
    for r in rows:
        mark = "  <<<" if float(r.get("fcu_build_us", 0)) > threshold else ""
        print(f"  {r['block']:>5} {r.get('fcu_build_us', 0):>10.0f} "
              f"{r.get('get_payload_us', 0):>11.0f} {r.get('new_payload_us', 0):>11.0f} "
              f"{r.get('fcu_advance_us', 0):>9.0f} {r.get('txs_included', 0):>4}{mark}")
    spikes = [r["block"] for r in rows if float(r.get("fcu_build_us", 0)) > threshold]
    print(f"  spikes > {threshold / 1000:.0f} ms at blocks: {spikes or 'none'}")


def warn_trie_rebuild(rows: list[dict]) -> None:
    """Flag a block whose build rebuilt the whole intermediate trie rather than updating it.

    `stage unwind` drops AccountsTrie and StoragesTrie wholesale — they are derived state — while
    leaving HashedAccounts intact. The first state root computed after an unwind therefore rebuilds
    every intermediate node from the hashed state, at a cost proportional to the whole state rather
    than to the block. On the 1M-account dev chain that is 1.13 s against a 370 us steady state, and
    on a real snapshot it would be minutes. It reads as a cold-cache effect and it is not one.

    Detected by leaves-added being orders of magnitude above the run's median, which needs no
    knowledge of the state size.
    """
    key = 'reth_trie_leaves_added_sum{type="state"}'
    leaves = [((r.get("metrics") or {}).get(key, 0.0), r["block"]) for r in rows]
    values = [v for v, _ in leaves if v > 0]
    if len(values) < 3:
        return
    median = sorted(values)[len(values) // 2]
    suspects = [(v, b) for v, b in leaves if median > 0 and v > 100 * median]
    if not suspects:
        return
    print("  WARNING: full trie rebuild detected, these blocks are not measurements:")
    for value, block in suspects:
        print(f"    block {block}: {value:,.0f} state leaves added "
              f"(run median {median:,.0f}) — discard it")
    print("    Cause: `stage unwind` clears AccountsTrie/StoragesTrie; the first build after an")
    print("    unwind reconstructs them from HashedAccounts. Warm up, or drop the first block.")


def counters(rows: list[dict], pattern: str | None) -> None:
    """Mean per build of every counter that moved, plus histogram means from _sum/_count."""
    totals: dict[str, float] = {}
    builds = 0
    for r in rows:
        sample = r.get("metrics")
        if sample is None:
            continue
        builds += 1
        for key, value in sample.items():
            totals[key] = totals.get(key, 0.0) + float(value)
    if not builds:
        print("  (no metrics recorded; run with --metrics-url)")
        return

    # Pair up histogram sums with their counts so a mean duration per *observation* is available
    # alongside the mean per build.
    sums = {k[:-4]: v for k, v in totals.items() if k.endswith("_sum")}
    counts = {k[:-6]: v for k, v in totals.items() if k.endswith("_count")}
    paired = sorted(set(sums) & set(counts))

    plain = {k: v for k, v in totals.items()
             if not k.endswith(("_sum", "_count")) and v != 0.0}

    def keep(name: str) -> bool:
        return pattern is None or pattern in name

    print(f"  counters, mean per build over {builds} builds:")
    for key in sorted(plain):
        if keep(key):
            print(f"    {plain[key] / builds:>12.2f}  {key}")

    if paired:
        print(f"\n  histograms, mean per build over {builds} builds "
              f"(seconds shown as us where applicable):")
        for base in paired:
            if not keep(base):
                continue
            per_build = sums[base] / builds
            n = counts[base]
            unit = "us" if "seconds" in base else ""
            scale = 1e6 if "seconds" in base else 1.0
            print(f"    {per_build * scale:>12.1f} {unit:<3} {base}   "
                  f"(n={n / builds:.2f}/build)")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("timings", type=Path, nargs="+")
    parser.add_argument("--label", action="append", default=[],
                        help="name for each input, in order")
    parser.add_argument("--sequence", action="store_true",
                        help="print the per-block sequence; a clustered tail and a periodic tail "
                             "have different causes and only the sequence distinguishes them")
    parser.add_argument("--spike-us", type=float, default=5000.0,
                        help="fcu_build_us above which a block is flagged (default 5000)")
    parser.add_argument("--grep", help="only counters whose name contains this")
    args = parser.parse_args()

    for i, path in enumerate(args.timings):
        label = args.label[i] if i < len(args.label) else path.name
        rows = load(path)
        if not rows:
            print(f"== {label}: empty", file=sys.stderr)
            continue
        print(f"== {label}  ({len(rows)} blocks, {path})")
        warn_trie_rebuild(rows)
        phase_table(rows)
        if args.sequence:
            print()
            sequence(rows, args.spike_us)
        print()
        counters(rows, args.grep)
        print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
