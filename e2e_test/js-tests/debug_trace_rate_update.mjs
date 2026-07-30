#!/usr/bin/env node
// Regression test for block-scoped tracing and call simulation on CIP-64 blocks, all
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
// 4. Transaction-level parity and Otterscan tracing. Their shared replay path
//    also split the prefix and target across EVMs, so late CIP-64 transactions
//    re-loaded a mid-block rate.
//
// 5. debug_traceCallMany. Its default All position captured from block-final
//    state instead of the target block's start, and each simulated bundle did
//    not reliably keep one block-start context across all of its calls.
//
// 6. Call overrides. State and block overrides must be applied before the
//    block context is captured. An unrelated state override must not disable
//    context pinning, while an oracle override must define the pinned rate.
//
// 7. trace_callMany. Although it intentionally creates a fresh inspector and
//    EVM for every call, every call in the sequence must use the context
//    captured before the first call mutates state.
//
// 8. eth_callMany. Each bundle is a simulated block. It must capture context
//    after that bundle's overrides, then seed the same context into every
//    fresh call EVM in the bundle.
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
import { encodeFunctionData, numberToHex, parseAbi, parseEther } from "viem";
import { publicClient, walletClient, account } from "./viem_setup.mjs";

const [feeCurrency, oracle, directory] = process.argv.slice(2);
const CIP64_TX_COUNT = 3;
const UNRELATED_ACCOUNT = "0x00000000000000000000000000000000000000d0";
// Runtime returns GASPRICE followed by NUMBER as two ABI words.
const CONTEXT_PROBE_INIT_CODE =
  "0x600d600c600039600d6000f33a6000524360205260406000f3";
const PRIORITY_FEE = 100n;
let contextProbe;

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

function word(value) {
  return numberToHex(value, { size: 32 });
}

function oracleRateOverrides(numerator, denominator) {
  return {
    [oracle]: {
      stateDiff: {
        [word(0n)]: word(parseEther(numerator)),
        [word(1n)]: word(parseEther(denominator)),
      },
    },
  };
}

function setRateCall(numerator, denominator = "1") {
  return {
    from: account.address,
    to: oracle,
    data: encodeFunctionData({
      abi: oracleAbi,
      functionName: "setExchangeRate",
      args: [
        feeCurrency,
        parseEther(numerator),
        parseEther(denominator),
      ],
    }),
    gas: numberToHex(200000n),
  };
}

function feeCurrencyProbe(baseFeePerGas) {
  return {
    from: account.address,
    to: contextProbe,
    feeCurrency,
    maxFeePerGas: numberToHex(baseFeePerGas * 200n),
    maxPriorityFeePerGas: numberToHex(PRIORITY_FEE),
    gas: numberToHex(100000n),
  };
}

function assertProbeOutput(
  name,
  output,
  expectedGasPrice,
  expectedBlockNumber,
) {
  if (!output || output.length !== 130) {
    fail(`${name}: expected two ABI words, got ${output}`);
  }
  const gasPrice = BigInt(`0x${output.slice(2, 66)}`);
  const blockNumber = BigInt(`0x${output.slice(66, 130)}`);
  if (gasPrice !== expectedGasPrice || blockNumber !== expectedBlockNumber) {
    fail(
      `${name}: expected GASPRICE=${expectedGasPrice}, NUMBER=${expectedBlockNumber}; got GASPRICE=${gasPrice}, NUMBER=${blockNumber}`,
    );
  }
}

async function deployContextProbe() {
  const hash = await walletClient.sendTransaction({
    account,
    data: CONTEXT_PROBE_INIT_CODE,
    gas: 100000n,
  });
  const receipt = await publicClient.waitForTransactionReceipt({
    hash,
    timeout: 30_000,
  });
  if (receipt.status !== "success" || !receipt.contractAddress) {
    fail("could not deploy the GASPRICE/NUMBER context probe");
  }
  return receipt.contractAddress;
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
  contextProbe = await deployContextProbe();
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

  await traceParityAndOtterscan(cipHashes.at(-1));
  await debugTraceCallManyWithOverrides(blockNumber, block.baseFeePerGas);
  await traceCallAfterMidBlockRemoval();
  await ethCallMany(blockNumber, block.baseFeePerGas);
  await traceParityCallMany(blockNumber, block.baseFeePerGas);

  console.log(JSON.stringify({ success: true, error: null }));
}

// The parity and Otterscan transaction endpoints share reth's generic
// transaction-in-block replay path. The target must run on the same EVM as its
// prefix so the late CIP-64 transaction keeps the block-start rate.
async function traceParityAndOtterscan(txHash) {
  let parity, ots;
  try {
    parity = await publicClient.request({
      method: "trace_transaction",
      params: [txHash],
    });
    ots = await publicClient.request({
      method: "ots_traceTransaction",
      params: [txHash],
    });
  } catch (e) {
    fail(
      `transaction trace endpoint failed: ${e.details ?? e.shortMessage ?? e.message}`,
    );
  }

  if (!Array.isArray(parity) || parity.length === 0) {
    fail("trace_transaction returned no traces");
  }
  const parityRoot = parity.find(
    (trace) => Array.isArray(trace.traceAddress) && trace.traceAddress.length === 0,
  );
  if (
    !parityRoot ||
    parityRoot.error ||
    parityRoot.transactionHash?.toLowerCase() !== txHash.toLowerCase() ||
    parityRoot.action?.from?.toLowerCase() !== account.address.toLowerCase()
  ) {
    fail(`trace_transaction: unexpected result ${JSON.stringify(parity)}`);
  }

  if (!Array.isArray(ots) || ots.length === 0) {
    fail("ots_traceTransaction returned no traces");
  }
  const otsRoot = ots.find((trace) => trace.depth === 0);
  if (!otsRoot || otsRoot.from?.toLowerCase() !== account.address.toLowerCase()) {
    fail(`ots_traceTransaction: unexpected result ${JSON.stringify(ots)}`);
  }
}

// The relevant oracle override defines the bundle-start rate as 1:2. The first
// call changes the database back to 100:1, but the second call must still see
// the captured 1:2 rate. The block-number override also has to be applied
// before capture so the context stamp matches the call EVM.
async function debugTraceCallManyWithOverrides(blockNumber, baseFeePerGas) {
  const overriddenNumber = blockNumber + 1000n;
  let traces;
  try {
    traces = await publicClient.request({
      method: "debug_traceCallMany",
      params: [
        [
          {
            transactions: [
              setRateCall("100"),
              feeCurrencyProbe(baseFeePerGas),
            ],
            blockOverride: { number: numberToHex(overriddenNumber) },
          },
        ],
        { blockNumber: numberToHex(blockNumber) },
        {
          tracer: "callTracer",
          stateOverrides: oracleRateOverrides("1", "2"),
        },
      ],
    });
  } catch (e) {
    fail(
      `debug_traceCallMany with overrides failed: ${e.details ?? e.shortMessage ?? e.message}`,
    );
  }

  if (!Array.isArray(traces) || traces.length !== 1 || traces[0]?.length !== 2) {
    fail(
      `debug_traceCallMany with overrides: unexpected shape ${JSON.stringify(traces)}`,
    );
  }
  for (const [txIndex, result] of traces[0].entries()) {
    if (result.error) {
      fail(
        `debug_traceCallMany with overrides: tx ${txIndex} failed: ${result.error}`,
      );
    }
  }
  assertProbeOutput(
    "debug_traceCallMany with overrides",
    traces[0][1].output,
    baseFeePerGas / 2n + PRIORITY_FEE,
    overriddenNumber,
  );
}

// trace_callMany executes calls sequentially on the target block's final
// state, whose canonical oracle rate is now 100:1. The first call changes the
// database to 1:2. The second fresh EVM must still use the sequence-start
// 100:1 context.
async function traceParityCallMany(blockNumber, baseFeePerGas) {
  let traces;
  try {
    traces = await publicClient.request({
      method: "trace_callMany",
      params: [
        [
          [setRateCall("1", "2"), ["trace"]],
          [feeCurrencyProbe(baseFeePerGas), ["trace"]],
        ],
        numberToHex(blockNumber),
      ],
    });
  } catch (e) {
    fail(
      `trace_callMany failed: ${e.details ?? e.shortMessage ?? e.message}`,
    );
  }

  if (!Array.isArray(traces) || traces.length !== 2) {
    fail(`trace_callMany: unexpected shape ${JSON.stringify(traces)}`);
  }
  for (const [txIndex, result] of traces.entries()) {
    const root = result.trace?.find(
      (trace) =>
        Array.isArray(trace.traceAddress) && trace.traceAddress.length === 0,
    );
    if (!root || root.error) {
      fail(`trace_callMany: tx ${txIndex} failed: ${JSON.stringify(result)}`);
    }
  }
  // Celo's call-request conversion currently caps the effective price at the
  // native base fee plus priority before fee-currency conversion. Rates above
  // 1 therefore return this cap, while the 1:2 override remains distinguishable.
  assertProbeOutput(
    "trace_callMany",
    traces[1].output,
    baseFeePerGas + PRIORITY_FEE,
    blockNumber,
  );
}

// The top-level override makes the first bundle start at 1:2. Bundle 1 changes
// the database to 1:4, but its probe must remain at 1:2. Bundle 2 captures the
// newly committed 1:4 context, then changes the database to 1:2; its probe must
// remain at 1:4. Both rates are below 1 so the call-request native-fee cap
// cannot make an incorrect rate look correct. The same probe verifies that
// each bundle's block override is present in every call EVM.
async function ethCallMany(blockNumber, baseFeePerGas) {
  const firstNumber = blockNumber + 2000n;
  const secondNumber = blockNumber + 2001n;
  let results;
  try {
    results = await publicClient.request({
      method: "eth_callMany",
      params: [
        [
          {
            transactions: [
              setRateCall("1", "4"),
              feeCurrencyProbe(baseFeePerGas),
            ],
            blockOverride: { number: numberToHex(firstNumber) },
          },
          {
            transactions: [
              setRateCall("1", "2"),
              feeCurrencyProbe(baseFeePerGas),
            ],
            blockOverride: { number: numberToHex(secondNumber) },
          },
        ],
        { blockNumber: numberToHex(blockNumber) },
        oracleRateOverrides("1", "2"),
      ],
    });
  } catch (e) {
    fail(
      `eth_callMany failed: ${e.details ?? e.shortMessage ?? e.message}`,
    );
  }

  if (
    !Array.isArray(results) ||
    results.length !== 2 ||
    results[0]?.length !== 2 ||
    results[1]?.length !== 2
  ) {
    fail(`eth_callMany: unexpected shape ${JSON.stringify(results)}`);
  }
  for (const [bundleIndex, bundle] of results.entries()) {
    for (const [txIndex, result] of bundle.entries()) {
      if (result.error || !result.value) {
        fail(
          `eth_callMany: bundle ${bundleIndex} tx ${txIndex} failed: ${JSON.stringify(result)}`,
        );
      }
    }
  }
  assertProbeOutput(
    "eth_callMany bundle 0",
    results[0][1].value,
    baseFeePerGas / 2n + PRIORITY_FEE,
    firstNumber,
  );
  assertProbeOutput(
    "eth_callMany bundle 1",
    results[1][1].value,
    baseFeePerGas / 4n + PRIORITY_FEE,
    secondNumber,
  );
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
  let trace, traceWithUnrelatedOverride;
  const request = {
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
  };
  const block = numberToHex(removalReceipt.blockNumber);
  const txIndex = numberToHex(followUpReceipt.transactionIndex);
  try {
    trace = await publicClient.request({
      method: "debug_traceCall",
      params: [
        request,
        block,
        { tracer: "callTracer", txIndex },
      ],
    });
  } catch (e) {
    fail(
      `debug_traceCall(txIndex) after mid-block removal failed: ${e.details ?? e.shortMessage ?? e.message}`,
    );
  }
  try {
    traceWithUnrelatedOverride = await publicClient.request({
      method: "debug_traceCall",
      params: [
        request,
        block,
        {
          tracer: "callTracer",
          txIndex,
          stateOverrides: {
            [UNRELATED_ACCOUNT]: { balance: "0x1" },
          },
        },
      ],
    });
  } catch (e) {
    fail(
      `debug_traceCall(txIndex) with unrelated override failed: ${e.details ?? e.shortMessage ?? e.message}`,
    );
  }
  if (trace.error || trace.from?.toLowerCase() !== account.address.toLowerCase()) {
    fail(`debug_traceCall(txIndex): unexpected result ${JSON.stringify(trace)}`);
  }
  if (
    traceWithUnrelatedOverride.error ||
    traceWithUnrelatedOverride.from?.toLowerCase() !==
      account.address.toLowerCase()
  ) {
    fail(
      `debug_traceCall(txIndex) with unrelated override: unexpected result ${JSON.stringify(traceWithUnrelatedOverride)}`,
    );
  }

  await traceCallManyAfterRemoval(removalReceipt, baseFeePerGas);
}

// At TransactionIndex::All, execution starts from the removal block's final
// state while the first bundle still uses that block's block-start fee context.
// Bundle 1 can therefore re-register the removed currency and execute CIP-64.
// Bundle 2 removes it again, then proves every call in that bundle keeps the
// context captured before the removal.
async function traceCallManyAfterRemoval(removalReceipt, baseFeePerGas) {
  const currencies = await publicClient.readContract({
    address: directory,
    abi: directoryAbi,
    functionName: "getCurrencies",
    blockNumber: removalReceipt.blockNumber,
  });
  const restoredIndex = BigInt(currencies.length);
  const call = {
    from: account.address,
    to: "0x00000000000000000000000000000000DeaDBeef",
    value: "0x1",
    feeCurrency,
    maxFeePerGas: numberToHex(baseFeePerGas * 200n),
    maxPriorityFeePerGas: "0x64",
    gas: numberToHex(90000n),
  };
  const setCurrencyConfig = {
    from: account.address,
    to: directory,
    data: encodeFunctionData({
      abi: directoryAbi,
      functionName: "setCurrencyConfig",
      args: [feeCurrency, oracle, 60000n],
    }),
    gas: numberToHex(200000n),
  };
  const removeCurrency = {
    from: account.address,
    to: directory,
    data: encodeFunctionData({
      abi: directoryAbi,
      functionName: "removeCurrencies",
      args: [feeCurrency, restoredIndex],
    }),
    gas: numberToHex(200000n),
  };

  const optionSets = [
    ["without overrides", { tracer: "callTracer" }],
    [
      "with unrelated override",
      {
        tracer: "callTracer",
        stateOverrides: {
          [UNRELATED_ACCOUNT]: { balance: "0x1" },
        },
      },
    ],
  ];

  for (const [variant, options] of optionSets) {
    let traces;
    try {
      traces = await publicClient.request({
        method: "debug_traceCallMany",
        params: [
          [
            { transactions: [setCurrencyConfig, call] },
            { transactions: [removeCurrency, call] },
          ],
          { blockNumber: numberToHex(removalReceipt.blockNumber) },
          options,
        ],
      });
    } catch (e) {
      fail(
        `debug_traceCallMany ${variant} after removal failed: ${e.details ?? e.shortMessage ?? e.message}`,
      );
    }

    if (
      !Array.isArray(traces) ||
      traces.length !== 2 ||
      traces[0]?.length !== 2 ||
      traces[1]?.length !== 2
    ) {
      fail(
        `debug_traceCallMany ${variant}: unexpected bundle shape ${JSON.stringify(traces)}`,
      );
    }
    for (const [bundleIndex, bundle] of traces.entries()) {
      for (const [txIndex, result] of bundle.entries()) {
        if (result.error) {
          fail(
            `debug_traceCallMany ${variant}: bundle ${bundleIndex} tx ${txIndex} failed: ${result.error}`,
          );
        }
      }
      if (bundle[1].from?.toLowerCase() !== account.address.toLowerCase()) {
        fail(
          `debug_traceCallMany ${variant}: unexpected CIP-64 sender ${bundle[1].from}`,
        );
      }
    }
  }
}

await main();
process.exit(0);
