## Native Celo transaction  
* Testcase: Block with a Tx type 2 (Eip-1519)  
* Network: Celo Sepolia  
* File: sepolia-native-celo-tx_block-2343822.tar.gz  
* Explorer: https://celo-sepolia.blockscout.com/block/2343822

## Token duality
- Celo Erc20 transfer
  * Testcase: Block with erc20 transfer of celo
  * Network: Celo Sepolia
  * File: sepolia-token-duality-tx_block-2346831.tar.gz  
  * Explorer: https://celo-sepolia.blockscout.com/block/2346831
- Celo Erc20 transferFrom
  * Testcase: Block with erc20 transferFrom of celo (spender moves celo on behalf of owner)
  * Network: Celo Mainnet
  * File: mainnet-token-duality-transfer-from-tx_block-43396452.tar.gz  
  * Explorer: https://celo.blockscout.com/block/43396452

## Cip64 transaction
- Erc20 transfer
  * Testcase: Block with cip64 tx sending erc20, paid in the same erc20
  * Network: Celo Sepolia
  * File: sepolia-cip64-erc20-transfer-tx_block-2346450.tar.gz
  * Explorer: https://celo-sepolia.blockscout.com/block/2346450
- Native
  * Testcase: Block with cip64 tx native celo, paid in erc20
  * Network: Celo Sepolia
  * File: sepolia-cip64-native-tx_block-2346490.tar.gz
  * Explorer: https://celo-sepolia.blockscout.com/block/2346490
- Reverted tx
  * Testcase: Block with cip64 tx that reverts
  * Network: Mainnet
  * File: mainnet-cip64-reverted-tx_block-31071493.tar.gz
  * Explorer: https://celo.blockscout.com/block/31071493
- GASPRICE opcode
  * Testcase: A CIP-64 tx that calls the GASPRICE opcode and emits an event with the result. The tx does this inside a contract constructor with the CIP-64 equivalent of `cast send --create 0x3a60005260206000a0`. The bytecode is `GASPRICE, PUSH1 0, MSTORE, PUSH1 32, PUSH1 0, LOG0`.
  * Network: Celo Sepolia
  * File: sepolia-cip64-gasprice-opcode_block-11462516.tar.gz
  * Explorer: https://celo-sepolia.blockscout.com/block/11462516

## L1 to L2 bridge transaction
- Successful deposit
  * Testcase: Block with deposit tx
  * Network: Celo Sepolia
  * File: sepolia-l1-to-l2-bridge-tx_block-1022860.tar.gz
  * Explorer: https://celo-sepolia.blockscout.com/block/1022860
- Revert deposit
  * Testcase: Block with a reverted deposit
  * Network: Celo Sepolia
  * File: sepolia-revert_deposit-tx_block-9558619.tar.gz
  * Explorer: https://celo-sepolia.blockscout.com/block/9558619

## FeeCurrencyContext maintained for the whole block
* Testcase: Block with cip64 txs paid in erc20 + rate change of that erc20 + more cip64 txs paid in the same erc20
* Network: Celo Sepolia
* File: sepolia-fee-currency-context_block-2265803.tar.gz
* Explorer: https://celo-sepolia.blockscout.com/block/2265803

## Empty block
* Testcase: Block with 1 transaction (setL1ValueIsthmus)
* Network: Celo Sepolia
* File: sepolia-empty_block-2346038.tar.gz
* Explorer: https://celo-sepolia.blockscout.com/block/2346038

## Transfer precompile not warming the "to" address
- One instance
  * Testcase: Block with 3 txs. The last tx consumes the transfer precompile, with the "to" address cold, and later that "to" address is loaded again in a sub call and treated as cold again. This is to match the exact behaviour we are running from the beginning of mainnet
  * Network: Celo Mainnet
  * File: mainnet-transfer_precompile_warm_block-31128957.tar.gz
  * Explorer: https://celo.blockscout.com/block/31128957
- Multiple instances
  * Testcase: Block with 8 txs. Ther 3rd and 4th txs consume the transfer precompile, with the "to" address cold, and later that "to" address is loaded again in a sub call and treated as cold again. This is to match the exact behaviour we are running from the beginning of mainnet
  * Network: Celo Mainnet
  * File: mainnet-transfer_precompile_warm_multi_block-31074658.tar.gz
  * Explorer: https://celo.blockscout.com/block/31074658

## Transfer precompile not warming the "from" address
* Testcase: Block with an aggregate3 tx that makes 2 transferFrom using the transfer precompile using the same "from" in both txs. The deployed Celo contract is pre-warming the "from" address
* Network: Celo Sepolia
* File: sepolia-transfer_precompile_warm_from-tx_block-6750121.tar.gz  
* Explorer: https://celo-sepolia.blockscout.com/block/6750121

## Missing rate for whitelisted currencies
* Testcase: Block without a rate from one of the whitelisted currencies (avoid failure from the block context)
* Network: Celo Mainnet
* File: mainnet-missing_rate_from_whitelisted_currency-block-47668860.tar.gz  
* Explorer: https://celo.blockscout.com/block/47668860

## Legacy EIP-2930 transaction with wrong chain ID
Transaction accepted due to a bug in op-geth's EIP-2930 sender recovery that used tx.ChainId() instead of the network's chain ID. Must be accepted during sync to avoid a hard fork. See https://github.com/celo-org/op-geth/issues/454.
* Testcase: Block containing an EIP-2930 tx with chain_id 44787 instead of correct 42220
* Network: Celo Mainnet
* File: mainnet-wrong-chain-id-eip2930_block-53619115.tar.gz
* Explorer: https://celo.blockscout.com/block/53619115

## Uncategorized Blocks that failed (scenarios to be defined)
- Failed for 1.0.0-rc4, fixed after 1.0.0-rc5
  * Testcase: -
  * Network: Celo Mainnet
  * File: mainnet-failed_uncategorized_1-block-49847887.tar.gz
  * Explorer: https://celo.blockscout.com/block/49847887
