#!/usr/bin/env python3
"""Compare engine-replay arms: phase decomposition, per-entry cost, marginal cost per entry.

An *arm* is one replay of one archive under one defined condition — cache regime, prefetch setting,
recipient mode. `scripts/perf/replay_arms.sh` produces them; this reads them back. Where
`scripts/perf/summarise.py` describes a single run in detail, this compares runs to each other,
which is the only way a claim like "the walk has no irreducible cost" can be supported.

Three things about the input, all of which have caused wrong numbers here at least once:

  DELTAS      Every `metrics` field in a timings.jsonl row is already a per-block delta
              (`bin/engine-replay/src/replay.rs` calls `metrics::delta`), so a `_sum` is seconds
              spent in that phase during that block. Summing across label sets aggregates every
              build attempt in the block, which is what "what does the sequencer spend per block"
              means.
  PREFIXES    The exporter prefixes every metric with `reth_`, so match on the suffix. Matching bare
              names silently returns 0.0 for every payload phase.
  MEDIANS     Means here are tail-dominated: in one 34-block arm, 7 blocks carried the mean, and in
              6 of those the build outran the evictor's period so pages warmed early were
              invalidated before the walk reached them. The mean was 11.80 ms for a phase whose
              median was 0.61 ms. Medians are the default; `--means` prints both.

Blocks 1-3 are dropped by default: after `stage unwind to-block 0` the first builds reconstruct the
whole intermediate trie, which is proportional to state size rather than to the block.

Subcommands:
    analyse_arms.py rep      --timings F [--post F] [--label L] [--rep N]
    analyse_arms.py phases   --workdir W --arm warm-off --arm cold-off ...
    analyse_arms.py modes    --arm fresh=W1 --arm existing-scattered=W2 --regime warm --regime ...
    analyse_arms.py slopes   --arm fresh=W1 --arm existing-scattered=W2 --regime warm [--y seeks]

`--arm` takes `LABEL` (resolved against `--workdir`) or `LABEL=WORKDIR`, so arms that share a datadir
and arms that cannot share one are described the same way.
"""

import argparse
import glob
import json
import math
import os
import statistics as st
import sys

PHASES = [
    ("build", "celo_payload_build_duration_seconds"),
    ("exec", "celo_payload_transaction_execution_duration_seconds"),
    ("final", "celo_payload_finalization_duration_seconds"),
    ("hps", "celo_payload_hashed_post_state_duration_seconds"),
    ("root", "celo_payload_state_root_duration_seconds"),
    ("pf", "celo_payload_trie_prefetch_duration_seconds"),
]
SIZES = [
    ("entries", "celo_payload_hashed_post_state_size"),
    ("upd_nodes", "celo_payload_trie_updates_size"),
    ("pf_accts", "celo_payload_trie_prefetch_accounts"),
    ("pf_slots", "celo_payload_trie_prefetch_slots"),
    ("exec_calls", "celo_payload_transaction_execution_calls"),
]
ENTRIES = "celo_payload_hashed_post_state_size"
ROOT = "celo_payload_state_root_duration_seconds"
SKIP_DEFAULT = 3


# --------------------------------------------------------------------------- metric access

def total(metrics, base, suffix):
    """Sum `<prefix>base_suffix{...}` across every label set present."""
    want = base + "_" + suffix
    return sum(v for k, v in metrics.items() if k.split("{")[0].endswith(want))


def trie_seeks(metrics):
    """Trie cursor work: seeks and advances, excluding the histogram `_count` companions."""
    return sum(v for k, v in metrics.items()
               if k.startswith("reth_trie") and ("seek" in k or "advance" in k)
               and not k.endswith("_count"))


def m(row):
    return row.get("metrics") or {}


# --------------------------------------------------------------------------- loading

def load_reps(workdir, label, skip=SKIP_DEFAULT, first_block=None):
    """Per-rep row lists for `t-<label>-<rep>.jsonl` under `workdir`, newest last."""
    reps = []
    for path in sorted(glob.glob(os.path.join(workdir, f"t-{label}-*.jsonl"))):
        with open(path) as fh:
            rows = [json.loads(line) for line in fh if line.strip()]
        rows = ([r for r in rows if r["block"] >= first_block] if first_block is not None
                else rows[skip:])
        if rows:
            reps.append(rows)
    return reps


def dedupe_reps(reps, vector):
    """Drop reps whose `vector` duplicates an earlier rep's, and say so.

    Trie cursor counters are deterministic given the same blocks, so replaying one archive twice
    produces byte-identical seek and changed-entry vectors. Pooling them doubles n without adding a
    single independent observation and deflates every standard error by sqrt(2) — which is exactly
    how an earlier version of this analysis reported +-1.99 and t = 0.95 for a difference whose
    honest figures are +-2.91 and t = 0.65. Timings are not deterministic, so this only fires for
    counter-valued regressions; that is the point.
    """
    kept, seen, dropped = [], set(), 0
    for rows in reps:
        key = tuple(vector(r) for r in rows)
        if key in seen:
            dropped += 1
            continue
        seen.add(key)
        kept.append(rows)
    return kept, dropped


def flatten(reps):
    return [r for rows in reps for r in rows]


def resolve_arms(args):
    """`LABEL` or `LABEL=WORKDIR` -> [(label, workdir)], erroring rather than guessing."""
    out = []
    for spec in args.arm:
        label, sep, workdir = spec.partition("=")
        if not sep:
            if not args.workdir:
                sys.exit(f"--arm {spec} has no workdir: pass LABEL=WORKDIR or set --workdir")
            workdir = args.workdir
        if not os.path.isdir(workdir):
            sys.exit(f"--arm {spec}: no such workdir {workdir}")
        out.append((label, workdir))
    return out


# --------------------------------------------------------------------------- statistics

def ols(xs, ys):
    """Slope, intercept, R^2, standard error of the slope, n. None when it cannot be fitted."""
    n = len(xs)
    if n < 3:
        return None
    mx, my = sum(xs) / n, sum(ys) / n
    sxx = sum((x - mx) ** 2 for x in xs)
    if sxx == 0:
        return None
    slope = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / sxx
    intercept = my - slope * mx
    sst = sum((y - my) ** 2 for y in ys)
    ssr = sum((y - (intercept + slope * x)) ** 2 for x, y in zip(xs, ys))
    r2 = 1 - ssr / sst if sst else float("nan")
    se = math.sqrt(ssr / (n - 2) / sxx)
    return slope, intercept, r2, se, n


def stat(values, use_mean):
    return st.mean(values) if use_mean else st.median(values)


# --------------------------------------------------------------------------- rep

def cmd_rep(args):
    """One-line summary of a single replay — what replay_arms.sh prints after each rep."""
    with open(args.timings) as fh:
        rows = [json.loads(line) for line in fh if line.strip()]
    if not rows:
        sys.exit(f"{args.timings}: no rows")

    # Block 1 excluded: after `stage unwind` the first build reconstructs the whole intermediate
    # trie, an artefact proportional to state size rather than a measurement.
    body = sorted(r["fcu_build_us"] + r["get_payload_us"] for r in rows[1:])
    k = len(body)

    counters, phases = {}, {}
    if args.post and os.path.exists(args.post):
        with open(args.post) as fh:
            for line in fh:
                if line.startswith("#"):
                    continue
                name, _, value = line.rpartition(" ")
                try:
                    val = float(value)
                except ValueError:
                    continue
                bare = name.split("{")[0]
                for short, needle in (
                        ("bp", "reth_consensus_engine_beacon_backpressure_stall_duration_count"),
                        ("saves",
                         "reth_consensus_engine_persistence_save_blocks_duration_seconds_count")):
                    if bare == needle:
                        counters[short] = val
                # Build #1 only: the exact quantity celo-blockchain-planning#1453 reports as
                # rate(_sum[w])/rate(_count[w]). Cumulative, read once, so the run was unperturbed.
                if 'has_best_payload="false"' not in line:
                    continue
                for short, needle in (("build", "celo_payload_build_duration_seconds"),
                                      ("root", "celo_payload_state_root_duration_seconds"),
                                      ("exec",
                                       "celo_payload_transaction_execution_duration_seconds")):
                    if bare.endswith(needle + "_sum"):
                        phases.setdefault(short, {})["sum"] = val
                    elif bare.endswith(needle + "_count"):
                        phases.setdefault(short, {})["count"] = val

    def mean_ms(key):
        d = phases.get(key) or {}
        return d["sum"] / d["count"] * 1e3 if d.get("count") else float("nan")

    print("%-18s rep%-3s | blocks 2+: p50=%7d p90=%7d max=%8d us"
          "  | node build#1 mean: build=%7.1f root=%7.1f exec=%6.1f ms"
          "  | bp=%s saves=%s  %d/%d hashes" % (
              args.label, args.rep,
              body[k // 2], body[min(k - 1, round(k * 0.9))], body[-1],
              mean_ms("build"), mean_ms("root"), mean_ms("exec"),
              ("%.0f" % counters["bp"]) if "bp" in counters else "?",
              ("%.0f" % counters["saves"]) if "saves" in counters else "?",
              sum(1 for r in rows if r["hash_match"]), len(rows)))


# --------------------------------------------------------------------------- phases

def cmd_phases(args):
    """Phase-decompose builds across arms. Reproduces the warm-floor table."""
    arms, agg = resolve_arms(args), {}
    for label, workdir in arms:
        reps = load_reps(workdir, label, args.skip)
        if not reps:
            print(f"  {label}: no timings under {workdir}", file=sys.stderr)
            continue
        rows = flatten(reps)
        d = {"n": len(rows), "reps": len(reps)}
        for short, base in PHASES:
            d[short + "_vals"] = [total(m(r), base, "sum") * 1e3 for r in rows]
        # Residual per block, then the statistic — not statistic-of-statistics, which can leave the
        # decomposition failing to add up when the phases peak in different blocks.
        d["resid_vals"] = [b - e - f for b, e, f in
                           zip(d["build_vals"], d["exec_vals"], d["final_vals"])]
        d["rest_vals"] = [f - h - p - rt for f, h, p, rt in
                          zip(d["final_vals"], d["hps_vals"], d["pf_vals"], d["root_vals"])]
        for short, base in SIZES:
            d[short] = stat([total(m(r), base, "sum") for r in rows], args.means)
        d["seeks"] = stat([trie_seeks(m(r)) for r in rows], args.means)
        d["txs"] = stat([r["txs_included"] for r in rows], args.means)
        d["attempts"] = stat([total(m(r), PHASES[0][1], "count") for r in rows], args.means)
        drv = sorted(r["fcu_build_us"] + r["get_payload_us"] for r in rows)
        d["p50"] = drv[len(drv) // 2] / 1e3
        d["ok"] = sum(1 for r in rows if r["hash_match"])
        agg[label] = d

    if not agg:
        sys.exit("no arms loaded")
    unit = "means" if args.means else "medians"

    def v(d, key):
        return stat(d[key + "_vals"], args.means)

    head = "%-12s %5s %4s %8s %9s %8s %7s %8s %9s %8s %7s %7s" % (
        "arm", "blks", "reps", "drv p50", "build", "exec", "hps", "prefetch", "root", "resid",
        "txs", "ok")
    print(f"\nPer-block {unit}, ms, summed over every build attempt in the block "
          f"(blocks {args.skip + 1}+)")
    print(head + "\n" + "-" * len(head))
    for label, _ in arms:
        d = agg.get(label)
        if d:
            print("%-12s %5d %4d %8.2f %9.2f %8.2f %7.3f %8.2f %9.2f %8.2f %7.1f %4d/%d" % (
                label, d["n"], d["reps"], d["p50"], v(d, "build"), v(d, "exec"), v(d, "hps"),
                v(d, "pf"), v(d, "root"), v(d, "resid"), d["txs"], d["ok"], d["n"]))
    print("\nresid = build - exec - finalization: provider construction, "
          "apply_pre_execution_changes, payload assembly. Not instrumented.")

    print(f"\nfinalization = hashed post state + prefetch + root + rest ({unit})")
    head = "%-12s %9s %8s %9s %9s %9s" % ("arm", "final", "hps", "prefetch", "root", "rest")
    print(head + "\n" + "-" * len(head))
    for label, _ in arms:
        d = agg.get(label)
        if d:
            print("%-12s %9.2f %8.3f %9.2f %9.2f %9.2f" % (
                label, v(d, "final"), v(d, "hps"), v(d, "pf"), v(d, "root"), v(d, "rest")))

    print(f"\nwork walked per block ({unit})")
    head = "%-12s %9s %10s %10s %10s %11s" % (
        "arm", "entries", "upd_nodes", "pf_accts", "pf_slots", "trie_seeks")
    print(head + "\n" + "-" * len(head))
    for label, _ in arms:
        d = agg.get(label)
        if d:
            print("%-12s %9.1f %10.0f %10.1f %10.1f %11.1f" % (
                label, d["entries"], d["upd_nodes"], d["pf_accts"], d["pf_slots"], d["seeks"]))

    warm = agg.get(args.warm)
    cold = agg.get(args.cold)
    if warm and cold:
        penalty = v(cold, "build") - v(warm, "build")
        print(f"\n--- accounting, against the {args.warm} floor")
        print(f"floor build      {v(warm, 'build'):.2f} ms "
              f"(root {v(warm, 'root'):.2f}, exec {v(warm, 'exec'):.2f})")
        if penalty:
            d_root = v(cold, "root") - v(warm, "root")
            d_exec = v(cold, "exec") - v(warm, "exec")
            print(f"{args.cold} penalty  {penalty:+.2f} ms = root {d_root:+.2f} + "
                  f"exec {d_exec:+.2f} + other {penalty - d_root - d_exec:+.2f}")
            print(f"  {100 * d_root / penalty:.1f}% of it is the state-root walk; "
                  f"{100 * (1 - v(warm, 'build') / v(cold, 'build')):.1f}% of the cold build "
                  f"is not work the code performs")
        for label, _ in arms:
            d = agg.get(label)
            if not d or label in (args.warm, args.cold) or not v(d, "pf"):
                continue
            covered = v(d, "pf") + v(d, "root")
            print(f"{label:<12} prefetch {v(d, 'pf'):6.2f} + root {v(d, 'root'):6.2f} "
                  f"= {covered:6.2f} ms vs {args.cold} root {v(cold, 'root'):.2f} "
                  f"({v(cold, 'root') / covered:.2f}x)   seeks {d['seeks']:.0f} "
                  f"= {d['seeks'] / cold['seeks']:.2f}x the serial walk   "
                  f"build {v(d, 'build'):.2f} ms ({v(cold, 'build') / v(d, 'build'):.2f}x)   "
                  f"{v(d, 'build') - v(warm, 'build'):+.2f} ms above the floor")

    if not args.means:
        print("\nmeans, for the tail check only — a mean far above the median means blocks outran "
              "the evictor")
        head = "%-12s %9s %9s %9s   %s" % ("arm", "build", "root", "prefetch", "root mean/median")
        print(head + "\n" + "-" * len(head))
        for label, _ in arms:
            d = agg.get(label)
            if not d:
                continue
            med = st.median(d["root_vals"])
            mean = st.mean(d["root_vals"])
            print("%-12s %9.2f %9.2f %9.2f   %s" % (
                label, st.mean(d["build_vals"]), mean, st.mean(d["pf_vals"]),
                f"{mean / med:.1f}x" if med else "n/a"))


# --------------------------------------------------------------------------- modes

def cmd_modes(args):
    """Per-changed-entry state-root cost for every arm x regime, in the #1453 unit."""
    arms, agg = resolve_arms(args), {}
    for mode, workdir in arms:
        for regime in args.regime:
            label = f"{mode}-{regime}" if regime else mode
            reps = load_reps(workdir, label, args.skip)
            if not reps:
                continue
            rows = flatten(reps)
            ent = [total(m(r), ENTRIES, "sum") for r in rows]
            root = [total(m(r), ROOT, "sum") * 1e3 for r in rows]
            # Median of per-block ratios, not median(root)/median(entries): the latter mixes blocks
            # and hides any block whose entry count came out wrong.
            per_entry = [rt * 1e3 / e for rt, e in zip(root, ent) if e > 0]
            agg[(mode, regime)] = dict(
                n=len(rows), reps=len(reps), entries=st.median(ent),
                txs=st.median([r["txs_included"] for r in rows]),
                gas=st.median([r["gas_used"] for r in rows]),
                seeks=st.median([trie_seeks(m(r)) for r in rows]),
                root=st.median(root),
                us_entry=st.median(per_entry) if per_entry else float("nan"),
                ok=sum(1 for r in rows if r["hash_match"]))
    if not agg:
        sys.exit("no arms loaded")

    print("\n=== CONTROLS — these must match across modes, or the comparison has two variables")
    head = "%-34s %5s %5s %9s %11s %7s %10s %8s" % (
        "arm", "blks", "reps", "entries", "gas", "txs", "trie_seeks", "hashes")
    print(head + "\n" + "-" * len(head))
    for mode, _ in arms:
        for regime in args.regime:
            d = agg.get((mode, regime))
            if d:
                print("%-34s %5d %5d %9.1f %11.0f %7.1f %10.1f %5d/%d" % (
                    f"{mode}-{regime}", d["n"], d["reps"], d["entries"], d["gas"], d["txs"],
                    d["seeks"], d["ok"], d["n"]))

    print("\n=== STATE-ROOT COST PER CHANGED ENTRY (median us/entry) — the #1453 unit")
    head = "%-24s" % "mode" + "".join("%14s" % r for r in args.regime)
    print(head + "\n" + "-" * len(head))
    for mode, _ in arms:
        cells = []
        for regime in args.regime:
            d = agg.get((mode, regime))
            cells.append("%.1f" % d["us_entry"] if d else "-")
        print("%-24s" % mode + "".join("%14s" % c for c in cells))

    print("\n=== CONTRASTS, within each regime")
    for regime in args.regime:
        pairs = [(a, b) for a, _ in arms for b, _ in arms if a != b]
        seen = set()
        printed = False
        for a, b in pairs:
            if (b, a) in seen:
                continue
            seen.add((a, b))
            da, db = agg.get((a, regime)), agg.get((b, regime))
            if not da or not db or not db["us_entry"]:
                continue
            if not printed:
                print(f"\n  --- {regime}")
                printed = True
            print(f"    {a} / {b}: {da['us_entry']:8.1f} vs {db['us_entry']:8.1f} us/entry "
                  f"= {da['us_entry'] / db['us_entry']:5.2f}x")

    print("\n  Per-entry medians are only comparable across arms that carry a similar number of "
          "changed\n  entries — the walk has a fixed per-block cost that amortises over however "
          "many there are.\n  Check the CONTROLS table above, and use `slopes` when they differ.")


# --------------------------------------------------------------------------- slopes

def cmd_slopes(args):
    """Marginal cost of one more changed entry, per arm, with standard errors.

    This is the control that survives arms carrying different numbers of changed entries: the slope
    is the marginal cost of one more entry and the intercept absorbs the fixed per-block cost. The
    stratified medians underneath are the independent second control — a claim that only one of them
    supports is not a finding.
    """
    arms = resolve_arms(args)
    y_is_seeks = args.y == "seeks"

    def y_of(row):
        return trie_seeks(m(row)) if y_is_seeks else total(m(row), ROOT, "sum") * 1e6

    unit = "seeks/entry" if y_is_seeks else "us/entry"
    fits, strat = {}, {}
    for mode, workdir in arms:
        label = f"{mode}-{args.regime}" if args.regime else mode
        reps = load_reps(workdir, label, args.skip, args.first_block)
        if not reps:
            print(f"  {label}: no timings under {workdir}", file=sys.stderr)
            continue
        # Deterministic y values must not be pooled across reps; timings may be.
        dropped = 0
        if y_is_seeks:
            reps, dropped = dedupe_reps(reps, lambda r: (total(m(r), ENTRIES, "sum"), y_of(r)))
        rows = flatten(reps)
        ent = [total(m(r), ENTRIES, "sum") for r in rows]
        ys = [y_of(r) for r in rows]
        fits[mode] = (ols(ent, ys), len(reps), dropped)
        window = [y / e for e, y in zip(ent, ys)
                  if e and args.window[0] <= e <= args.window[1]]
        strat[mode] = (st.median(window), len(window)) if window else (float("nan"), 0)

    if not fits:
        sys.exit("no arms loaded")
    label_regime = f" [{args.regime}]" if args.regime else ""
    first = f", blocks {args.first_block}+" if args.first_block else f", blocks {args.skip + 1}+"

    print(f"\n=== CONTROL 1: marginal cost of one more changed entry{label_regime}{first}")
    head = "%-24s %5s %5s %16s %12s %8s" % ("arm", "reps", "n", unit, "fixed cost", "R2")
    print(head + "\n" + "-" * len(head))
    for mode, _ in arms:
        got = fits.get(mode)
        if not got:
            continue
        fit, nreps, dropped = got
        if not fit:
            print("%-24s %5d %5s   not fittable" % (mode, nreps, "-"))
            continue
        slope, intercept, r2, se, n = fit
        note = "" if r2 >= 0.5 else "   <-- unfittable, do not quote"
        if dropped:
            note += f"   [{dropped} duplicate rep(s) dropped]"
        print("%-24s %5d %5d %8.2f +- %-4.2f %12.2f %8.3f%s" % (
            mode, nreps, n, slope, se, intercept, r2, note))

    print(f"\n=== CONTROL 2: median {unit} among blocks carrying "
          f"{args.window[0]}-{args.window[1]} changed entries")
    for mode, _ in arms:
        if mode in strat:
            value, n = strat[mode]
            print("  %-24s %8.1f   (n=%d)" % (mode, value, n) if n
                  else "  %-24s no blocks in window" % mode)

    print("\n=== CONTRASTS — a claim counts only if both controls agree")
    seen = set()
    for a, _ in arms:
        for b, _ in arms:
            if a == b or (b, a) in seen:
                continue
            seen.add((a, b))
            parts = []
            fa, fb = fits.get(a), fits.get(b)
            if fa and fb and fa[0] and fb[0]:
                sa, sb = fa[0], fb[0]
                diff = sa[0] - sb[0]
                se = math.sqrt(sa[3] ** 2 + sb[3] ** 2)
                t = abs(diff) / se if se else float("nan")
                verdict = "not distinguishable" if t < 2 else "distinguishable"
                parts.append(f"slope diff {diff:+.2f} +- {se:.2f}  t={t:.2f}  {verdict}")
                if se and sb[0]:
                    # Two standard errors, i.e. the gap this arm pair could have called
                    # significant. Quoting ONE standard error as the floor overstates the power by
                    # 2x: at 1 SE a true gap of that size yields t = 1, which this test does not
                    # detect. 80% power would be 2.8 SE, so treat even this as generous.
                    floor = 100 * 2 * se / abs(sb[0])
                    parts.append(f"detects a gap wider than ~{floor:.0f}% (2 SE), not a smaller one")
            if a in strat and b in strat and strat[b][0]:
                parts.append(f"stratified {strat[a][0] / strat[b][0]:.2f}x")
            if parts:
                print(f"  {a} / {b}:")
                for p in parts:
                    print(f"      {p}")


# --------------------------------------------------------------------------- cli

def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)

    def add_arms(p):
        p.add_argument("--arm", action="append", required=True, metavar="LABEL[=WORKDIR]",
                       help="arm label, optionally with its own workdir; repeatable")
        p.add_argument("--workdir", help="default workdir for --arm values that omit one")
        p.add_argument("--skip", type=int, default=SKIP_DEFAULT,
                       help="leading blocks to drop (default 3: the post-unwind trie rebuild)")

    p = sub.add_parser("rep", help="one-line summary of a single replay")
    p.add_argument("--timings", required=True)
    p.add_argument("--post", help="one-shot metrics scrape taken after the replay")
    p.add_argument("--label", default="run")
    p.add_argument("--rep", default="1")
    p.set_defaults(func=cmd_rep)

    p = sub.add_parser("phases", help="phase-decompose builds across arms")
    add_arms(p)
    p.add_argument("--means", action="store_true", help="report means instead of medians")
    p.add_argument("--warm", default="warm-off", help="arm to treat as the resident floor")
    p.add_argument("--cold", default="cold-off", help="arm to treat as the unmitigated problem")
    p.set_defaults(func=cmd_phases)

    p = sub.add_parser("modes", help="per-changed-entry cost across arms and cache regimes")
    add_arms(p)
    p.add_argument("--regime", action="append", help="regime suffix, e.g. warm; repeatable")
    p.set_defaults(func=cmd_modes)

    p = sub.add_parser("slopes", help="marginal cost per changed entry, with standard errors")
    add_arms(p)
    p.add_argument("--regime", default="", help="regime suffix appended to each arm label")
    p.add_argument("--y", choices=("seeks", "root"), default="seeks",
                   help="seeks = logical trie work (deterministic); root = microseconds")
    p.add_argument("--first-block", type=int,
                   help="start at this block instead of skipping --skip (use when the arm evicts "
                        "partway in, so the pre-eviction blocks are a different regime)")
    p.add_argument("--window", type=int, nargs=2, default=(8, 18), metavar=("LO", "HI"),
                   help="changed-entry window for the stratified control (default 8 18)")
    p.set_defaults(func=cmd_slopes)

    args = parser.parse_args()
    if getattr(args, "regime", None) == [] or getattr(args, "regime", None) is None:
        if args.cmd == "modes":
            args.regime = ["warm", "coldstart", "coldloop"]
    args.func(args)
    return 0


if __name__ == "__main__":
    sys.exit(main())
