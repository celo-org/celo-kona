#!/usr/bin/env node
// Signs a transaction fully offline and prints {"hash", "raw"}.
//
// Every gas, fee and nonce field is supplied by the caller, so the produced raw
// bytes — and therefore the transaction hash — are reproducible across runs.
// That is what lets a test commit a golden file keyed by transaction hash. With
// --fee-currency a CIP-64 (type 0x7b) transaction is produced through viem's
// Celo serializers.
//
// Nothing here contacts a node: the signer is built straight from the private
// key, so no RPC URL is needed and no network round-trip can perturb the
// signed fields.
//
// Usage:
//   sign_raw_tx.mjs --nonce 0 --to 0x... [--value 1] [--gas 100000]
//                   [--data 0x...] [--fee-currency 0x...]
//                   [--max-fee 50000000000] [--max-priority-fee 100]
//                   [--chain-id 1337]
//
// Env: ACC_PRIVKEY (the signer key).
import { keccak256 } from "viem";
import { privateKeyToAccount } from "viem/accounts";
import { serializeTransaction } from "viem/celo";

const KNOWN = new Set([
  "nonce", "to", "value", "gas", "data", "fee-currency",
  "max-fee", "max-priority-fee", "chain-id",
]);

function die(message) {
  console.error(`sign_raw_tx.mjs: ${message}`);
  process.exit(2);
}

const args = {};
for (let i = 2; i < process.argv.length; i += 2) {
  const flag = process.argv[i];
  const value = process.argv[i + 1];
  if (!flag.startsWith("--")) die(`expected a --flag, got '${flag}'`);
  const name = flag.slice(2);
  if (!KNOWN.has(name)) die(`unknown flag '${flag}'`);
  if (value === undefined || value.startsWith("--")) {
    die(`flag '${flag}' needs a value`);
  }
  args[name] = value;
}
if (args.nonce === undefined || args.to === undefined) {
  die("--nonce and --to are required");
}
if (!process.env.ACC_PRIVKEY) die("ACC_PRIVKEY is not set");

function int(name, fallback) {
  const raw = args[name] ?? fallback;
  const parsed = Number(raw);
  if (!Number.isInteger(parsed) || parsed < 0) {
    die(`--${name} must be a non-negative integer, got '${raw}'`);
  }
  return parsed;
}

function big(name, fallback) {
  try {
    return BigInt(args[name] ?? fallback);
  } catch {
    return die(`--${name} must be an integer, got '${args[name]}'`);
  }
}

const tx = {
  chainId: int("chain-id", "1337"),
  nonce: int("nonce"),
  to: args.to,
  value: big("value", "0"),
  gas: big("gas", "100000"),
  maxFeePerGas: big("max-fee", "50000000000"),
  maxPriorityFeePerGas: big("max-priority-fee", "100"),
  type: args["fee-currency"] ? "cip64" : "eip1559",
};
if (args.data) tx.data = args.data;
if (args["fee-currency"]) tx.feeCurrency = args["fee-currency"];

// The Celo serializer is passed explicitly: a local account defaults to the
// stock one, which has no encoding for the CIP-64 type.
const account = privateKeyToAccount(process.env.ACC_PRIVKEY);
const raw = await account.signTransaction(tx, { serializer: serializeTransaction });
console.log(JSON.stringify({ hash: keccak256(raw), raw }));
