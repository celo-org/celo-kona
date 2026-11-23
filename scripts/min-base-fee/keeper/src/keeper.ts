// Sets SystemConfig.minBaseFee so that a reference tx (USDT transfer paid in USDT) costs
// targetAttoUsdPerTx. Throws KeeperError on anything that needs a human.
import {
  type Address,
  type Chain,
  type Hex,
  createPublicClient,
  createWalletClient,
  encodeFunctionData,
  formatUnits,
  getAddress,
  http,
  parseAbi,
  parseUnits,
  stringToHex,
} from 'viem';
import { privateKeyToAccount } from 'viem/accounts';
import { celo, mainnet, sepolia } from 'viem/chains';

const NETWORKS = {
  mainnet: {
    chain: mainnet,
    systemConfig: '0x89E31965D844a309231B1f17759Ccaf1b7c09861',
  },
  sepolia: {
    chain: sepolia,
    systemConfig: '0x760a5F022C9940f4A074e0030be682F560d29818',
  },
} as const satisfies Record<string, { chain: Chain; systemConfig: Address }>;

export const ROLE_KEY = stringToHex('MIN_BASE_FEE_KEEPER', { size: 32 });
// Always Celo Mainnet, also when updating Sepolia.
const CHAINLINK_CELO_USD = '0x0568fd19986748ceff3301e55c0eb1e729e0ab7e';

const chainlinkAbi = parseAbi([
  'function latestRoundData() view returns (uint80, int256, uint256, uint256, uint80)',
  'function decimals() view returns (uint8)',
]);
const systemConfigAbi = parseAbi([
  'function minBaseFee() view returns (uint64)',
  'function setMinBaseFee(uint64)',
]);
const rolesAbi = parseAbi([
  'function execTransactionWithRole(address to, uint256 value, bytes data, uint8 operation, bytes32 roleKey, bool shouldRevert) returns (bool)',
]);

export class KeeperError extends Error {}

export interface Config {
  network: keyof typeof NETWORKS;
  l1RpcUrl: string;
  l2RpcUrl: string;
  coinbaseUrl: string;
  roles: Address;
  privateKey?: Hex;
  dryRun: boolean;
  targetAttoUsdPerTx: bigint;
  referenceGas: bigint;
  // Bounds enforced on-chain by the Roles condition (wei per gas, inclusive).
  minFloor: bigint;
  maxFloor: bigint;
  maxPriceAgeSeconds: bigint;
  maxPriceDeviationBps: bigint;
  minChangeBps: bigint;
}

export function loadConfig(env: NodeJS.ProcessEnv = process.env): Config {
  const required = (name: string): string => {
    const value = env[name];
    if (!value) throw new KeeperError(`set ${name}`);
    return value;
  };
  const int = (name: string, fallback: bigint): bigint => (env[name] ? BigInt(env[name]) : fallback);

  const network = env.NETWORK ?? 'mainnet';
  if (!(network in NETWORKS)) throw new KeeperError(`unknown NETWORK=${network}`);
  const dryRun = env.DRY_RUN === 'true';
  const privateKey = env.KEEPER_PRIVATE_KEY?.trim() as Hex | undefined;
  if (!dryRun && !privateKey) throw new KeeperError('set KEEPER_PRIVATE_KEY');

  return {
    network: network as Config['network'],
    l1RpcUrl: required('L1_RPC_URL'),
    l2RpcUrl: env.L2_RPC_URL ?? 'https://forno.celo.org',
    coinbaseUrl: env.COINBASE_URL ?? 'https://api.coinbase.com/v2/prices/CELO-USD/spot',
    roles: getAddress(required('ROLES')),
    privateKey,
    dryRun,
    targetAttoUsdPerTx: int('TARGET_ATTO_USD_PER_TX', 2_000_000_000_000_000n), // $0.002
    referenceGas: int('REFERENCE_GAS', 115_501n),
    minFloor: int('MIN_FLOOR', 25_000_000_000n), // 25 Gwei
    maxFloor: int('MAX_FLOOR', 2_000_000_000_000n), // 2000 Gwei
    maxPriceAgeSeconds: int('MAX_PRICE_AGE', 90_000n),
    maxPriceDeviationBps: int('MAX_PRICE_DEVIATION_BPS', 1000n), // 10%
    minChangeBps: int('MIN_CHANGE_BPS', 100n), // skip updates below 1%
  };
}

// floor [wei/gas] = target [attoUSD/tx] / gas [gas/tx] / price [USD/CELO], price = answer / 10^decimals.
export function computeFloor(targetAttoUsdPerTx: bigint, referenceGas: bigint, answer: bigint, decimals: number): bigint {
  return (targetAttoUsdPerTx * 10n ** BigInt(decimals)) / (referenceGas * answer);
}

// |a - b| relative to b, in basis points, rounded down.
export function diffBps(a: bigint, b: bigint): bigint {
  return ((a > b ? a - b : b - a) * 10_000n) / b;
}

export type Result = {
  status: 'skipped' | 'dry-run' | 'sent';
  chainlink: string;
  coinbase: string;
  current: string;
  floor: string;
  changeBps: string;
  txHash?: Hex;
};

export type Log = (message: string, fields?: Record<string, unknown>) => void;

export async function runKeeper(cfg: Config, log: Log, now = () => Date.now()): Promise<Result> {
  const { chain, systemConfig } = NETWORKS[cfg.network];
  const l1 = createPublicClient({ chain, transport: http(cfg.l1RpcUrl) });
  const l2 = createPublicClient({ chain: celo, transport: http(cfg.l2RpcUrl) });

  // Primary price: Chainlink CELO/USD on Celo Mainnet.
  const [[, answer, , updatedAt], decimals] = await Promise.all([
    l2.readContract({ address: CHAINLINK_CELO_USD, abi: chainlinkAbi, functionName: 'latestRoundData' }),
    l2.readContract({ address: CHAINLINK_CELO_USD, abi: chainlinkAbi, functionName: 'decimals' }),
  ]);
  const age = BigInt(Math.floor(now() / 1000)) - updatedAt;
  if (age > cfg.maxPriceAgeSeconds) throw new KeeperError(`Chainlink price is ${age}s old`);
  if (answer <= 0n) throw new KeeperError(`Chainlink price ${answer}`);
  const chainlink = formatUnits(answer, decimals);

  // Cross-check: Coinbase spot.
  const coinbase = await fetchCoinbase(cfg.coinbaseUrl);
  const deviationBps = diffBps(answer, parseUnits(coinbase, decimals));
  if (deviationBps > cfg.maxPriceDeviationBps) {
    throw new KeeperError(`prices disagree by ${deviationBps} bps: Chainlink ${chainlink}, Coinbase ${coinbase}`);
  }

  const floor = computeFloor(cfg.targetAttoUsdPerTx, cfg.referenceGas, answer, decimals);
  if (floor < cfg.minFloor || floor > cfg.maxFloor) {
    throw new KeeperError(`floor ${floor} outside [${cfg.minFloor}, ${cfg.maxFloor}]`);
  }

  const current = await l1.readContract({ address: systemConfig, abi: systemConfigAbi, functionName: 'minBaseFee' });
  const changeBps = current === 0n ? 10_000n : diffBps(floor, current);
  const prices = {
    chainlink,
    coinbase,
    current: current.toString(),
    floor: floor.toString(),
    changeBps: changeBps.toString(),
  };
  log(`CELO/USD ${chainlink} (Coinbase ${coinbase}), floor ${current} -> ${floor} wei (${changeBps} bps)`, prices);
  const result: Result = { status: 'skipped', ...prices };

  if (changeBps < cfg.minChangeBps) {
    log(`change below ${cfg.minChangeBps} bps, skipping`);
    return result;
  }
  if (cfg.dryRun) {
    log('dry run, not sending');
    return { ...result, status: 'dry-run' };
  }

  const account = privateKeyToAccount(cfg.privateKey!);
  const wallet = createWalletClient({ account, chain, transport: http(cfg.l1RpcUrl) });
  const data = encodeFunctionData({ abi: systemConfigAbi, functionName: 'setMinBaseFee', args: [floor] });
  // Simulate first so a rejected call costs no gas.
  const { request } = await l1.simulateContract({
    account,
    address: cfg.roles,
    abi: rolesAbi,
    functionName: 'execTransactionWithRole',
    args: [systemConfig, 0n, data, 0, ROLE_KEY, true],
  });
  const txHash = await wallet.writeContract(request);
  log(`sent ${txHash}`, { txHash });
  const receipt = await l1.waitForTransactionReceipt({ hash: txHash });
  if (receipt.status !== 'success') throw new KeeperError(`tx ${txHash} reverted`);
  log(`floor set to ${floor} wei in block ${receipt.blockNumber}`);
  return { ...result, status: 'sent', txHash };
}

async function fetchCoinbase(url: string): Promise<string> {
  let amount: unknown;
  try {
    const res = await fetch(url, { signal: AbortSignal.timeout(20_000) });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    amount = ((await res.json()) as { data?: { amount?: unknown } }).data?.amount;
  } catch (e) {
    throw new KeeperError(`Coinbase fetch failed: ${(e as Error).message}`);
  }
  if (typeof amount !== 'string' || !/^\d+(\.\d+)?$/.test(amount)) {
    throw new KeeperError(`Coinbase returned ${JSON.stringify(amount)}`);
  }
  return amount;
}
