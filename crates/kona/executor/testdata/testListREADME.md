## Native Celo transaction  
* Testcase: Block with a Tx type 2 (Eip-1519)  
* Network: Celo Sepolia  
* File: sepolia-native-celo-tx_block-2343822.tar.gz  
* Explorer: https://celo-sepolia.blockscout.com/block/2343822

## Token duality
* Testcase: Block with erc20 transfer of celo
* Network: Celo Sepolia
* File: sepolia-token-duality-tx_block-2346831.tar.gz  
* Explorer: https://celo-sepolia.blockscout.com/block/2346831

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

## L1 to L2 bridge transaction
* Testcase: Block with deposit tx
* Network: Celo Sepolia
* File: sepolia-l1-to-l2-bridge-tx_block-1022860.tar.gz
* Explorer: https://celo-sepolia.blockscout.com/block/1022860

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
