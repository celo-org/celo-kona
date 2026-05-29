import { assert } from "chai";
import "mocha";
import {
	parseAbi,
} from "viem";
import fs from "fs";
import { publicClient, walletClient } from "./viem_setup.mjs"

// Load compiled contract
const testContractJSON = JSON.parse(fs.readFileSync(process.env.COMPILED_TEST_CONTRACT, 'utf8'));

// Required-field schemas for each tx type, derived from what a real client
// (viem/web3.py/ethers) treats as mandatory when parsing eth_getTransactionByHash
// / eth_getTransactionReceipt responses. Regressions of d6c83feb (duplicate
// feeCurrency) and 0d5324d7 (missing nonce/depositReceiptVersion) bypassed the
// existing assertions because viem returned undefined on the missing fields and
// nothing checked. These tables make those failures explicit.
const REQUIRED_TX_FIELDS_COMMON = [
	"hash", "nonce", "from", "gas", "input", "value",
	"r", "s", "blockHash", "blockNumber", "transactionIndex", "type",
];
const REQUIRED_TX_FIELDS_BY_TYPE = {
	legacy: ["chainId", "gasPrice"],
	eip2930: ["chainId", "gasPrice", "accessList"],
	eip1559: ["chainId", "maxFeePerGas", "maxPriorityFeePerGas", "accessList"],
	cip64: ["chainId", "maxFeePerGas", "maxPriorityFeePerGas", "accessList", "feeCurrency"],
};
const REQUIRED_RECEIPT_FIELDS_COMMON = [
	"transactionHash", "transactionIndex", "blockHash", "blockNumber",
	"from", "cumulativeGasUsed", "gasUsed", "logs", "logsBloom",
	"status", "effectiveGasPrice", "type",
];

function assertFieldsPresent(obj, fields, label) {
	for (const field of fields) {
		assert.isDefined(obj[field], `${label} missing required field '${field}'`);
		assert.isNotNull(obj[field], `${label} required field '${field}' is null`);
	}
}

// check checks that the receipt has status success and that the transaction
// type matches the expected type, since viem sometimes mangles the type when
// building txs. It also enforces that every field a real client requires for
// the given tx type is present and non-null in both the tx and receipt
// responses — see REQUIRED_TX_FIELDS_BY_TYPE for the per-type schema.
async function check(txHash, tx_checks, receipt_checks) {
	const receipt = await publicClient.waitForTransactionReceipt({ hash: txHash });
	assert.equal(receipt.status, "success", "receipt status 'failure'");
	const transaction = await publicClient.getTransaction({ hash: txHash });
	for (const [key, expected] of Object.entries(tx_checks ?? {})) {
		assert.equal(transaction[key], expected, `transaction ${key} does not match`);
	}
	for (const [key, expected] of Object.entries(receipt_checks ?? {})) {
		assert.equal(receipt[key], expected, `receipt ${key} does not match`);
	}
	const type = tx_checks?.type;
	if (type && REQUIRED_TX_FIELDS_BY_TYPE[type]) {
		assertFieldsPresent(transaction, REQUIRED_TX_FIELDS_COMMON, `tx[${type}]`);
		assertFieldsPresent(transaction, REQUIRED_TX_FIELDS_BY_TYPE[type], `tx[${type}]`);
		assertFieldsPresent(receipt, REQUIRED_RECEIPT_FIELDS_COMMON, `receipt[${type}]`);
	}
}

// sendTypedTransaction sends a transaction with the given type and an optional
// feeCurrency.
async function sendTypedTransaction(type, feeCurrency) {
	return await walletClient.sendTransaction({
		to: "0x00000000000000000000000000000000DeaDBeef",
		value: 1,
		type: type,
		feeCurrency: feeCurrency,
	});
}

// sendTypedSmartContractTransaction initiates a token transfer with the given type
// and an optional feeCurrency.
async function sendTypedSmartContractTransaction(type, feeCurrency) {
	const abi = parseAbi(['function transfer(address to, uint256 value) external returns (bool)']);
	return await walletClient.writeContract({
		abi: abi,
		address: process.env.TOKEN_ADDR,
		functionName: 'transfer',
		args: ['0x00000000000000000000000000000000DeaDBeef', 1n],
		type: type,
		feeCurrency: feeCurrency,
	});
}

// sendTypedCreateTransaction sends a create transaction with the given type
// and an optional feeCurrency.
async function sendTypedCreateTransaction(type, feeCurrency) {
	return await walletClient.deployContract({
		type: type,
		feeCurrency: feeCurrency,
		bytecode: testContractJSON.bytecode.object,
		abi: testContractJSON.abi,
		// The constructor args for the test contract at ../debug-fee-currency/DebugFeeCurrency.sol
		args: [1n, true, true, true],
	});
}

["legacy", "eip2930", "eip1559", "cip64"].forEach(function (type) {
	describe("viem smoke test, tx type " + type, () => {
		const feeCurrency = type == "cip64" ? process.env.FEE_CURRENCY.toLowerCase() : undefined;
		let l1Fee = 0n;
		it("send tx", async () => {
			const send = await sendTypedTransaction(type, feeCurrency);
			await check(send, {type, feeCurrency}, {l1Fee});
		});
		it("send create tx", async () => {
			const create = await sendTypedCreateTransaction(type, feeCurrency);
			await check(create, {type, feeCurrency}, {l1Fee});
		});
		it("send contract interaction tx", async () => {
			const contract = await sendTypedSmartContractTransaction(type, feeCurrency);
			await check(contract, {type, feeCurrency}, {l1Fee});
		});
	});
});
