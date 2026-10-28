#!/usr/bin/env python3
"""Check frozen NUT provenance and copies. Runtime parity is covered by Rust CI.

Optional --optimism compares the same fixture consumed by op-node without cloning.
The fixture uses chain 1337 only and is not a production bundle.
"""
import argparse
import hashlib
import json
from pathlib import Path

from import_bundle import rlp

ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "crates/celo-alloy/consensus/testdata/cgt_v2"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--optimism", type=Path)
    args = parser.parse_args()
    manifest = json.loads((FIXTURE / "cgtv2-provenance.json").read_text())
    assert manifest["fixtureOnly"] and manifest["chainId"] == 1337
    assert manifest["staticGasLimit"] + manifest["fundingGasLimit"] == 40_600_000
    for name, digest in manifest["sha256"].items():
        assert Path(name).name == name, name
        assert hashlib.sha256((FIXTURE / name).read_bytes()).hexdigest() == digest, name
    bundle = json.loads((FIXTURE / "cgtv2-fixture-nut-bundle.json").read_text())
    assert len(bundle["transactions"]) == 15
    static = [(FIXTURE / f"cgtv2-{i:02}.rlp").read_bytes() for i in range(15)]
    assert (ROOT / "crates/celo-alloy/consensus/upgrades/cgtv2/fixture.rlp").read_bytes() == rlp(static)
    assert sum(tx["gasLimit"] for tx in bundle["transactions"]) == manifest["staticGasLimit"]
    assert (ROOT / "crates/alloy-celo-evm/src/block/cgt_v2_prestate.json").read_bytes() == (FIXTURE / "cgtv2-fixture-prestate.json").read_bytes()
    if args.optimism:
        target = args.optimism / "op-node/rollup/derive/testdata/cgtv2"
        for name in [*manifest["sha256"], "cgtv2-provenance.json"]:
            assert (target / name).read_bytes() == (FIXTURE / name).read_bytes(), name
    print("CGTv2 fixture digests, gas and prestate copies match" + ("; op-node vectors match" if args.optimism else ""))


if __name__ == "__main__":
    main()
