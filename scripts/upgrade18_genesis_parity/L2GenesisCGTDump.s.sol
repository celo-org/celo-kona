// SPDX-License-Identifier: MIT
pragma solidity 0.8.15;

import { L2Genesis } from "scripts/L2Genesis.s.sol";
import { Fork } from "scripts/libraries/Config.sol";

/// @notice Runs the pinned `L2Genesis` with `useCustomGasToken = true` and dumps the
///         resulting alloc state as JSON, so celo-kona's Upgrade 18 activation artifact
///         (`crates/alloy-celo-evm/res/predeploys.json`) can be compared byte-for-byte
///         against a fresh CGT v2 genesis.
///
///         This file is copied by `run.sh` into `packages/contracts-bedrock/scripts/` of a
///         celo-org/optimism checkout pinned to the artifact's `build.gitCommit` — it is not
///         compiled inside celo-kona.
///
///         Values that land in compared accounts arrive via `CGT_*` environment variables so
///         `compare.py` resolves the artifact's `param:` placeholders (and the sequencer fee
///         vault's live-state slots) against the exact same inputs. Every other field only
///         shapes accounts outside the six compared predeploys and is an arbitrary non-zero
///         placeholder.
contract L2GenesisCGTDump is L2Genesis {
    function dump() external {
        run(
            Input({
                l1ChainID: 1,
                l2ChainID: 1337,
                l1CrossDomainMessengerProxy: payable(address(0x9901)),
                l1StandardBridgeProxy: payable(address(0x9902)),
                l1ERC721BridgeProxy: payable(address(0x9903)),
                celoGasBridgeL1Proxy: payable(vm.envAddress("CGT_CELO_GAS_BRIDGE_L1")),
                celoTokenL1: vm.envAddress("CGT_CELO_TOKEN_L1"),
                opChainProxyAdminOwner: address(0x9904),
                sequencerFeeVaultRecipient: vm.envAddress("CGT_SEQ_VAULT_RECIPIENT"),
                sequencerFeeVaultMinimumWithdrawalAmount: vm.envUint("CGT_SEQ_VAULT_MIN_WITHDRAWAL"),
                sequencerFeeVaultWithdrawalNetwork: vm.envUint("CGT_SEQ_VAULT_NETWORK"),
                baseFeeVaultRecipient: address(0x9906),
                baseFeeVaultMinimumWithdrawalAmount: 10 ether,
                baseFeeVaultWithdrawalNetwork: 1, // L2: L1 reverts under CGT
                l1FeeVaultRecipient: address(0x9907),
                l1FeeVaultMinimumWithdrawalAmount: 10 ether,
                l1FeeVaultWithdrawalNetwork: 1,
                operatorFeeVaultRecipient: address(0x9908),
                operatorFeeVaultMinimumWithdrawalAmount: 10 ether,
                operatorFeeVaultWithdrawalNetwork: 1,
                governanceTokenOwner: address(0x9909),
                fork: uint256(Fork.JOVIAN),
                deployCrossL2Inbox: false,
                enableGovernance: false,
                fundDevAccounts: false,
                useRevenueShare: false,
                chainFeesRecipient: address(0x990A),
                l1FeesDepositor: address(0x990B),
                useCustomGasToken: true,
                gasPayingTokenName: "Celo",
                gasPayingTokenSymbol: "CELO",
                nativeAssetLiquidityAmount: vm.envUint("CGT_NATIVE_ASSET_LIQUIDITY_AMOUNT"),
                liquidityControllerOwner: vm.envAddress("CGT_LIQUIDITY_CONTROLLER_OWNER")
            })
        );
        vm.dumpState(vm.envString("CGT_STATE_DUMP_PATH"));
    }
}
