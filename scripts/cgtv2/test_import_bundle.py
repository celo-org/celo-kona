import copy
import json
import tempfile
import unittest
from pathlib import Path

from import_bundle import FIXTURE, encode_bundle, install, rlp


class ImportBundleTests(unittest.TestCase):
    def setUp(self):
        self.bundle = json.loads((FIXTURE / "cgtv2-fixture-nut-bundle.json").read_text())

    def test_matches_existing_deposit_vectors(self):
        expected = [(FIXTURE / f"cgtv2-{i:02}.rlp").read_bytes() for i in range(15)]
        self.assertEqual(encode_bundle(self.bundle), rlp(expected))
        self.assertEqual(rlp([b"a", b"b"]), b"\xc2ab")

    def test_installs_both_network_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "reviewed.json"
            source.write_text(json.dumps(self.bundle))
            for network in ("mainnet", "sepolia", "chaos"):
                rust, go = install(source, network, root / "kona", root / "optimism")
                self.assertEqual(go.read_bytes(), source.read_bytes())
                self.assertEqual(rust.read_bytes(), encode_bundle(self.bundle))
                self.assertEqual(rust.name, network + ".rlp")

    def test_rejects_ambiguous_json_before_writing(self):
        aliases = []
        for key in ("intent", "from", "to", "data", "gasLimit"):
            bundle = copy.deepcopy(self.bundle)
            bundle["transactions"][0][key[0].upper() + key[1:]] = bundle["transactions"][0][key]
            aliases.append(json.dumps(bundle))
        bundle = copy.deepcopy(self.bundle)
        bundle["Transactions"] = bundle["transactions"]
        aliases.append(json.dumps(bundle))
        bundle = copy.deepcopy(self.bundle)
        bundle["metadata"]["Version"] = "1.0.0"
        aliases.append(json.dumps(bundle))
        original = json.dumps(self.bundle)
        aliases.append(original.replace('"version": "1.0.0"', '"version": "1.0.0", "version": "1.0.0"'))
        aliases.append(original.replace('"gasLimit": 6000000', '"gasLimit": 6000000, "gasLimit": 6000111', 1))
        for original in aliases:
            with self.subTest(original=original[:120]), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                source = root / "ambiguous.json"
                source.write_text(original)
                with self.assertRaises(ValueError):
                    install(source, "mainnet", root / "kona", root / "optimism")
                self.assertFalse((root / "kona").exists())
                self.assertFalse((root / "optimism").exists())

    def test_rejects_non_utf8_and_bom_before_writing(self):
        original = json.dumps(self.bundle)
        for encoding in ("utf-16", "utf-32", "utf-8-sig"):
            with self.subTest(encoding=encoding), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                source = root / "bad-encoding.json"
                source.write_bytes(original.encode(encoding))
                with self.assertRaises(ValueError):
                    install(source, "mainnet", root / "kona", root / "optimism")
                self.assertFalse((root / "kona").exists())
                self.assertFalse((root / "optimism").exists())

    def test_rejects_invalid_input_before_writing(self):
        for kind in ("version", "count", "intent", "address", "calldata", "zero-gas", "overflow"):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as directory:
                bundle = copy.deepcopy(self.bundle)
                if kind == "version":
                    bundle["metadata"]["version"] = "0"
                elif kind == "count":
                    bundle["transactions"].pop()
                elif kind == "intent":
                    bundle["transactions"][0]["intent"] = "wrong"
                elif kind == "address":
                    bundle["transactions"][0]["from"] = "0x01"
                elif kind == "calldata":
                    bundle["transactions"][0]["data"] = "0x0"
                elif kind == "zero-gas":
                    bundle["transactions"][0]["gasLimit"] = 0
                else:
                    bundle["transactions"][0]["gasLimit"] = (1 << 64) - 1
                root = Path(directory)
                source = root / "bad.json"
                source.write_text(json.dumps(bundle))
                with self.assertRaises(ValueError):
                    install(source, "mainnet", root / "kona", root / "optimism")
                self.assertFalse((root / "kona").exists())
                self.assertFalse((root / "optimism").exists())


if __name__ == "__main__":
    unittest.main()
