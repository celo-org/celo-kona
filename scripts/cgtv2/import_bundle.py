#!/usr/bin/env python3
"""Install a reviewed CGTv2 NUT bundle into the Go JSON and Rust RLP paths.

Requires Foundry's cast for Ethereum Keccak. No RPC calls or broadcasts are made.
"""
import argparse
import hashlib
import json
import re
import subprocess
from functools import lru_cache
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "crates/celo-alloy/consensus/testdata/cgt_v2"
NETWORKS = ("mainnet", "sepolia", "chaos")


def rlp(value):
    if isinstance(value, int):
        value = value.to_bytes((value.bit_length() + 7) // 8, "big")
    if isinstance(value, list):
        payload, prefix = b"".join(map(rlp, value)), 192
    else:
        if len(value) == 1 and value[0] < 128:
            return value
        payload, prefix = value, 128
    if len(payload) < 56:
        return bytes([prefix + len(payload)]) + payload
    size = len(payload).to_bytes((len(payload).bit_length() + 7) // 8, "big")
    return bytes([prefix + 55 + len(size)]) + size + payload


@lru_cache(maxsize=32)
def keccak(value):
    output = subprocess.check_output(["cast", "keccak", "0x" + value.hex()], text=True)
    return bytes.fromhex(output.strip()[2:])


def address(value):
    if not isinstance(value, str) or not re.fullmatch(r"0x[0-9a-fA-F]{40}", value):
        raise ValueError("invalid NUT address")
    return bytes.fromhex(value[2:])


def exact_keys(value, keys, context):
    if not isinstance(value, dict) or set(value) != set(keys):
        raise ValueError(f"invalid {context} keys")


def unique_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def encode_bundle(bundle):
    exact_keys(bundle, ("metadata", "transactions"), "bundle")
    exact_keys(bundle["metadata"], ("version",), "metadata")
    if bundle["metadata"]["version"] != "1.0.0":
        raise ValueError("unsupported NUT version")
    transactions = bundle["transactions"]
    if not isinstance(transactions, list):
        raise ValueError("invalid NUT transactions")
    expected = json.loads((FIXTURE / "cgtv2-fixture-nut-bundle.json").read_text())["transactions"]
    if len(transactions) != len(expected):
        raise ValueError("CGTv2 requires 15 static transactions")
    total_gas = 100_000
    decoded = []
    for i, tx in enumerate(transactions):
        exact_keys(tx, ("intent", "from", "to", "data", "gasLimit"), "transaction")
        if tx.get("intent") != expected[i]["intent"]:
            raise ValueError(f"unexpected CGTv2 intent at step {i}")
        gas = tx.get("gasLimit")
        if type(gas) is not int or gas <= 0 or gas >= 1 << 64:
            raise ValueError("invalid NUT gas limit")
        total_gas += gas
        if total_gas >= 1 << 64:
            raise ValueError("CGTv2 group gas overflow")
        data = tx.get("data")
        if not isinstance(data, str) or not re.fullmatch(r"0x(?:[0-9a-fA-F]{2})*", data):
            raise ValueError("invalid NUT calldata")
        decoded.append((address(tx.get("from")), b"" if tx.get("to") is None else address(tx["to"]), gas, bytes.fromhex(data[2:])))
    encoded = []
    for i, (sender, target, gas, data) in enumerate(decoded):
        intent = f"Cgtv2 {i}: {transactions[i]['intent']}"
        source = keccak((2).to_bytes(32, "big") + keccak(intent.encode()))
        encoded.append(b"\x7e" + rlp([source, sender, target, 0, 0, gas, 0, data]))
    return rlp(encoded)


def install(source, network, kona, optimism):
    if network not in NETWORKS:
        raise ValueError("unsupported network")
    original = source.read_bytes()
    encoded = encode_bundle(json.loads(original.decode("utf-8"), object_pairs_hook=unique_keys))
    rust_path = kona / f"crates/celo-alloy/consensus/upgrades/cgtv2/{network}.rlp"
    go_path = optimism / f"op-node/rollup/derive/upgrades/cgtv2/{network}.json"
    for path, data in [(rust_path, encoded), (go_path, original)]:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
    return rust_path, go_path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle", required=True, type=Path)
    parser.add_argument("--network", required=True, choices=NETWORKS)
    parser.add_argument("--kona", type=Path, default=ROOT)
    parser.add_argument("--optimism", required=True, type=Path)
    args = parser.parse_args()
    paths = install(args.bundle, args.network, args.kona, args.optimism)
    for path in paths:
        print(f"{path}: sha256={hashlib.sha256(path.read_bytes()).hexdigest()}")


if __name__ == "__main__":
    main()
