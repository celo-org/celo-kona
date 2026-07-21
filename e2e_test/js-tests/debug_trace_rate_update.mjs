#!/usr/bin/env node
// Regression test for three debug_trace* replay bugs on CIP-64 blocks, all
// violations of one invariant: replay and simulation must use the fee-currency
// context (exchange rates, directory membership) from block-start state, like
// transactions actually included in the block do.
//
// 1. Fresh-EVM-per-transaction replay. reth's debug API used to replay blocks
//    with a fresh EVM per transaction, so a rate update landing earlier in the
//    block made the replay of a later CIP-64 transaction re-load the new rate
//    mid-block and fail with "max fee per gas less than block base fee". Fixed
//    by replaying blocks on a single EVM (celo-org/reth branch
//    karlb/debug-trace-single-evm).
//
// 2. Cip64Storage double store. The prefix replay of debug_traceTransaction is
//    a non-inspecting EVM that never builds receipts, but it used to store
//    CIP-64 receipt data anyway — panicking on the second CIP-64 transaction it
//    replayed. Fixed by the cip64_store_enabled flag (celo-kona
//    karlb/cip64-replay-double-store).
//
// 3. Mid-block call simulations. debug_traceCall with a txIndex simulates a
//    call at a mid-block position; the call EVM used to load its fee-currency
//    context from the mid-block state instead of the block-start context a
//    transaction at that position would see. Fixed by the
//    capture/seed_block_replay_ctx hook (same reth branch + celo-kona impl).
//
// The script lands, in the SAME block, an oracle rate update (2:1 -> 100:1)
// followed by three CIP-64 transactions with limited fee headroom (valid at
// 2:1, invalid at 100:1), then asserts that debug_traceBlockByNumber and
// debug_traceBlockByHash trace every transaction and that
// debug_traceTransaction succeeds on the last CIP-64 transaction — whose
// prefix replay covers the rate update (bug 1) and two earlier CIP-64
// transactions (bug 2). It then covers bug 3 by removing the fee currency from
// the directory mid-block and tracing a CIP-64 call positioned after the
// removal: only with block-start context is the currency still registered.
//
// args: feeCurrency oracle directory
import { numberToHex, parseAbi, parseEther } from "viem";
import { publicClient, walletClient, account } from "./viem_setup.mjs";

const [feeCurrency, oracle, directory] = process.argv.slice(2);
const CIP64_TX_COUNT = 3;

const oracleAbi = parseAbi([
  "function setExchangeRate(address token, uint256 numerator, uint256 denominator)",
]);
const directoryAbi = parseAbi([
  "function getCurrencies() view returns (address[])",
  "function removeCurrencies(address currency, uint256 index)",
  "function setCurrencyConfig(address token, address oracle, uint256 intrinsicGas)",
]);

function fail(message) {
  console.log(JSON.stringify({ success: false, error: message }));
  process.exit(1);
}

async function setRate(numerator, nonce) {
  return walletClient.writeContract({
    address: oracle,
    abi: oracleAbi,
    functionName: "setExchangeRate",
    args: [feeCurrency, parseEther(numerator), parseEther("1")],
    gas: 200000n,
    nonce,
  });
}

// If part of the batch mined without the rest (e.g. the rate update alone, at
// which point the CIP-64 txs are unminable at the 100:1 rate), replace the
// first unmined nonce with a fee-bumped rate reset, resubmit the remaining
// CIP-64 txs in case the pool dropped them, and wait for the account nonce to
// pass the batch so the next attempt starts clean.
async function unstickBatch(nonce, maxFeePerGas, serializedCipTxs) {
  const firstUnmined = await publicClient.getTransactionCount({
    address: account.address,
  });
  const resetHash = await walletClient.writeContract({
    address: oracle,
    abi: oracleAbi,
    functionName: "setExchangeRate",
    args: [feeCurrency, parseEther("2"), parseEther("1")],
    gas: 200000n,
    nonce: firstUnmined,
    maxFeePerGas: maxFeePerGas * 2n,
    maxPriorityFeePerGas: 10n ** 9n,
  });
  await publicClient.waitForTransactionReceipt({
    hash: resetHash,
    timeout: 30_000,
  });
  // Only resubmit once the reset is mined: pool validation checks CIP-64 fee
  // caps against the canonical rate, so before the reset these would be
  // rejected as under-priced at 100:1.
  for (const serializedTransaction of serializedCipTxs) {
    // Already-known / replaced / mined txs are all fine here.
    await walletClient.sendRawTransaction({ serializedTransaction }).catch(() => {});
  }
  const target = nonce + 1 + CIP64_TX_COUNT;
  for (let i = 0; i < 30; i++) {
    const mined = await publicClient.getTransactionCount({
      address: account.address,
    });
    if (mined >= target) {
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, 1000));
  }
  fail("could not unstick the account after a partially mined batch");
}

// Send the rate update (nonce n) and the CIP-64 txs (nonces n+1..) as one
// batch, submitting the CIP-64 txs FIRST: the nonce gap keeps them queued
// until the rate update reaches the pool, so no block can mine the rate
// update without them — a block sealing mid-batch just pushes the whole
// batch into the next block. Returns receipts once all are mined, or null
// (after restoring the 2:1 rate) if the batch had to be unstuck.
async function sendBatch() {
  const nonce = await publicClient.getTransactionCount({
    address: account.address,
  });
  // The dev chain's base fee drifts, so derive the CIP-64 fee cap from the
  // current base fee: 10x covers the 2:1 rate (2x) with room for base-fee
  // movement, but is far below the converted base fee at the 100:1 rate.
  const { baseFeePerGas } = await publicClient.getBlock();
  const maxFeePerGas = baseFeePerGas * 10n;
  const serializedCipTxs = await Promise.all(
    Array.from({ length: CIP64_TX_COUNT }, async (_, i) => {
      const cipRequest = await walletClient.prepareTransactionRequest({
        account,
        to: "0x00000000000000000000000000000000DeaDBeef",
        value: 2n,
        gas: 90000,
        feeCurrency,
        maxFeePerGas,
        maxPriorityFeePerGas: 100n,
        nonce: nonce + 1 + i,
      });
      return walletClient.signTransaction(cipRequest);
    }),
  );
  const cipHashes = [];
  for (const serializedTransaction of serializedCipTxs) {
    cipHashes.push(
      await walletClient.sendRawTransaction({ serializedTransaction }),
    );
  }
  const rateHash = await setRate("100", nonce);
  let rateReceipt, cipReceipts;
  try {
    [rateReceipt, ...cipReceipts] = await Promise.all(
      [rateHash, ...cipHashes].map((hash) =>
        publicClient.waitForTransactionReceipt({ hash, timeout: 30_000 }),
      ),
    );
  } catch {
    await unstickBatch(nonce, maxFeePerGas, serializedCipTxs);
    return null;
  }
  return { rateReceipt, cipReceipts, cipHashes, maxFeePerGas };
}

async function sendBatchInOneBlock() {
  for (let attempt = 1; attempt <= 5; attempt++) {
    const res = await sendBatch();
    if (res === null) {
      // The batch was unstuck (rate already back at 2:1); just try again.
      continue;
    }
    const receipts = [res.rateReceipt, ...res.cipReceipts];
    const sameBlock = receipts.every(
      (r) => r.blockNumber === res.rateReceipt.blockNumber,
    );
    const ordered = receipts.every(
      (r, i) => i === 0 || receipts[i - 1].transactionIndex < r.transactionIndex,
    );
    if (sameBlock && ordered) {
      return res;
    }
    // The txs straddled a block boundary; reset the rate to 2:1 (so the next
    // CIP-64 txs pass pool validation again) and retry.
    const resetHash = await setRate("2");
    await publicClient.waitForTransactionReceipt({
      hash: resetHash,
      timeout: 30_000,
    });
  }
  fail("could not land the rate update and CIP-64 txs in one block");
}

async function main() {
  const { rateReceipt, cipReceipts, cipHashes, maxFeePerGas } =
    await sendBatchInOneBlock();
  if ([rateReceipt, ...cipReceipts].some((r) => r.status !== "success")) {
    fail("rate update or CIP-64 tx reverted");
  }

  const blockNumber = rateReceipt.blockNumber;
  const block = await publicClient.getBlock({ blockNumber });
  // Guard against the scenario becoming vacuous: the CIP-64 fee cap must be
  // insufficient at the post-update 100:1 rate, otherwise a mid-block rate
  // reload would go unnoticed.
  if (maxFeePerGas >= block.baseFeePerGas * 100n) {
    fail("CIP-64 fee cap not below the converted base fee at the new rate");
  }

  const traceOpts = { tracer: "callTracer" };
  let byNumber, byHash, single;
  try {
    byNumber = await publicClient.request({
      method: "debug_traceBlockByNumber",
      params: [numberToHex(blockNumber), traceOpts],
    });
    byHash = await publicClient.request({
      method: "debug_traceBlockByHash",
      params: [block.hash, traceOpts],
    });
    // The last CIP-64 tx: its prefix replay covers the rate update and two
    // CIP-64 transactions.
    single = await publicClient.request({
      method: "debug_traceTransaction",
      params: [cipHashes.at(-1), traceOpts],
    });
  } catch (e) {
    fail(`debug_trace* call failed: ${e.details ?? e.shortMessage ?? e.message}`);
  }

  for (const [name, traces] of [
    ["debug_traceBlockByNumber", byNumber],
    ["debug_traceBlockByHash", byHash],
  ]) {
    for (const cipHash of cipHashes) {
      const entry = traces.find(
        (t) => t.txHash?.toLowerCase() === cipHash.toLowerCase(),
      );
      if (!entry) {
        fail(`${name}: no trace entry for CIP-64 tx ${cipHash}`);
      }
      if (entry.error || entry.result?.error) {
        fail(`${name}: CIP-64 trace reports error: ${entry.error ?? entry.result.error}`);
      }
      if (entry.result?.from?.toLowerCase() !== account.address.toLowerCase()) {
        fail(`${name}: unexpected CIP-64 trace sender ${entry.result?.from}`);
      }
    }
  }
  if (single.error || single.from?.toLowerCase() !== account.address.toLowerCase()) {
    fail(`debug_traceTransaction: unexpected result ${JSON.stringify(single)}`);
  }

  await traceCallAfterMidBlockRemoval();

  console.log(JSON.stringify({ success: true, error: null }));
}

// Lands a directory removal of the fee currency and a plain transfer in one
// block, then debug_traceCall's a CIP-64 call positioned at the transfer's
// index: the prefix replay includes the removal, so only the block-start
// context (where the currency is still registered) lets the trace succeed.
async function traceCallAfterMidBlockRemoval() {
  let removalReceipt, followUpReceipt;
  for (let attempt = 1; ; attempt++) {
    // Re-resolved every attempt: a retry after a lone-mined removal re-registers
    // the currency below, which changes its directory index.
    const currencies = await publicClient.readContract({
      address: directory,
      abi: directoryAbi,
      functionName: "getCurrencies",
    });
    const index = currencies.findIndex(
      (c) => c.toLowerCase() === feeCurrency.toLowerCase(),
    );
    if (index < 0) {
      fail("fee currency not registered in the directory");
    }

    const nonce = await publicClient.getTransactionCount({
      address: account.address,
    });
    // Same pattern as sendBatch: the follow-up tx first, queued on a nonce gap
    // behind the removal so both land in one block.
    const followUpRequest = await walletClient.prepareTransactionRequest({
      account,
      to: account.address,
      value: 1n,
      gas: 21000,
      nonce: nonce + 1,
    });
    const followUpHash = await walletClient.sendRawTransaction({
      serializedTransaction: await walletClient.signTransaction(followUpRequest),
    });
    const removalHash = await walletClient.writeContract({
      address: directory,
      abi: directoryAbi,
      functionName: "removeCurrencies",
      args: [feeCurrency, BigInt(index)],
      gas: 200000n,
      nonce,
    });
    [removalReceipt, followUpReceipt] = await Promise.all(
      [removalHash, followUpHash].map((hash) =>
        publicClient.waitForTransactionReceipt({ hash, timeout: 30_000 }),
      ),
    );
    if (
      removalReceipt.blockNumber === followUpReceipt.blockNumber &&
      removalReceipt.transactionIndex < followUpReceipt.transactionIndex
    ) {
      break;
    }
    if (attempt >= 5) {
      fail("could not land the removal and follow-up tx in one block");
    }
    if (removalReceipt.status === "success") {
      // The removal landed without the follow-up; re-register the currency so
      // the next attempt's removal starts from the original directory state.
      const restoreHash = await walletClient.writeContract({
        address: directory,
        abi: directoryAbi,
        functionName: "setCurrencyConfig",
        args: [feeCurrency, oracle, 60000n],
        gas: 200000n,
      });
      await publicClient.waitForTransactionReceipt({
        hash: restoreHash,
        timeout: 30_000,
      });
    }
  }
  if (removalReceipt.status !== "success") {
    fail("directory removal reverted");
  }

  const { baseFeePerGas } = await publicClient.getBlock({
    blockNumber: removalReceipt.blockNumber,
  });
  let trace;
  try {
    trace = await publicClient.request({
      method: "debug_traceCall",
      params: [
        {
          from: account.address,
          to: "0x00000000000000000000000000000000DeaDBeef",
          value: "0x1",
          feeCurrency,
          // 200x: above the converted base fee even at the 100:1 rate still
          // active from the rate-update scenario, so the trace outcome hinges
          // only on the directory membership of the fee currency.
          maxFeePerGas: numberToHex(baseFeePerGas * 200n),
          maxPriorityFeePerGas: "0x64",
          gas: numberToHex(90000n),
        },
        numberToHex(removalReceipt.blockNumber),
        {
          tracer: "callTracer",
          txIndex: numberToHex(followUpReceipt.transactionIndex),
        },
      ],
    });
  } catch (e) {
    fail(
      `debug_traceCall(txIndex) after mid-block removal failed: ${e.details ?? e.shortMessage ?? e.message}`,
    );
  }
  if (trace.error || trace.from?.toLowerCase() !== account.address.toLowerCase()) {
    fail(`debug_traceCall(txIndex): unexpected result ${JSON.stringify(trace)}`);
  }
}

await main();
process.exit(0);
