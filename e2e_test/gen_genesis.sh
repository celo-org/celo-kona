#!/bin/bash
#
# Generates celo-dev-genesis.json with Celo-specific contracts pre-deployed.
#
# Mirrors the Go logic in op-geth/core/celo_genesis.go, using `cast` (Foundry)
# for keccak256 slot computations.
#
# Usage:
#   ./gen_genesis.sh [OP_GETH_DIR]
#
# OP_GETH_DIR defaults to ~/op-geth
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
OP_GETH="${1:-$HOME/op-geth}"
COMPILED="$OP_GETH/contracts/celo/compiled"
OUT="$SCRIPT_DIR/celo-dev-genesis.json"

if [[ ! -d "$COMPILED" ]]; then
    echo "ERROR: compiled contracts not found at $COMPILED"
    echo "Pass the op-geth root as the first argument."
    exit 1
fi

if ! command -v cast &>/dev/null; then
    echo "ERROR: cast (foundry) is required"; exit 1
fi
if ! command -v jq &>/dev/null; then
    echo "ERROR: jq is required"; exit 1
fi

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# Pad a 20-byte address to 32 bytes (left-pad with zeros), lowercase.
pad32() {
    local addr
    addr=$(echo "$1" | tr -d '0x' | tr '[:upper:]' '[:lower:]')
    printf "0x%064s" "$addr" | tr ' ' '0'
}

# keccak256(key ++ slot) — EVM mapping slot calculation.
calc_map_addr() {
    local slot="$1" key="$2"
    # Strip 0x, concatenate key||slot, hash
    local k s
    k=$(echo "$key" | sed 's/^0x//')
    s=$(echo "$slot" | sed 's/^0x//')
    cast keccak "0x${k}${s}"
}

# Increment a 32-byte hash by i (for array/struct offsets).
inc_hash() {
    local h="$1" i="$2"
    python3 -c "print('0x' + format(int('$h', 16) + $i, '064x'))"
}

# Read a compiled bytecode file (strips whitespace, ensures 0x prefix).
read_code() {
    local raw
    raw=$(tr -d '[:space:]' < "$1")
    if [[ "$raw" != 0x* ]]; then raw="0x$raw"; fi
    echo "$raw"
}

# ---------------------------------------------------------------------------
# Addresses & constants
# ---------------------------------------------------------------------------

DEV_ADDR="42cf1bbc38BaAA3c4898ce8790e21eD2738c6A4a"
DEV_ADDR2="f280E427723B0ee6a1eF614ffFBDE15DB5fED5b1"
FUNDED_ADDR="0000000000000000000000000000000000000002"
FAUCET_ADDR="fcf982bb4015852e706100b14e21f947a5bb718e"

FEE_CURRENCY="000000000000000000000000000000000000ce16"
FEE_CURRENCY2="000000000000000000000000000000000000ce17"

ORACLE1="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb0001"
ORACLE2="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb0002"
ORACLE3="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb0003"

CELO_TOKEN="471ece3750da237f93b8e339c536989b8978a438"
FEE_CURRENCY_DIR="15F344b9E6c3Cb6F0376A36A64928b13F62C6276"

# 100_000 * 1e18
DEV_BALANCE="0x00000000000000000000000000000000000000000000152d02c7e14af6800000"
# Same as hex for alloc balance field
DEV_BALANCE_ALLOC="0x152d02c7e14af6800000"
# 500_000_000 * 1e18
FAUCET_BALANCE="0x19d971e4fe8401e74000000"

# Exchange rates (as 32-byte values)
RATE_NUM="0x00000000000000000000000000000000000000000001a784379d99db42000000"   # 2e24
RATE_NUM2="0x000000000000000000000000000000000000000000006954fe21e3e8000000"   # 5e23
RATE_DENOM="0x00000000000000000000000000000000000000000000d3c21bcecceda1000000" # 1e24

INTRINSIC_GAS="0x000000000000000000000000000000000000000000000000000000000000c350" # 50000

SLOT_0=$(pad32 "0")
SLOT_1=$(pad32 "1")
SLOT_2=$(pad32 "2")

# ---------------------------------------------------------------------------
# Compute storage slots
# ---------------------------------------------------------------------------

echo "Computing storage slots..."

# Fee currency _balances mapping (slot 0)
BAL_DEV=$(calc_map_addr "$SLOT_0" "$(pad32 "$DEV_ADDR")")
BAL_DEV2=$(calc_map_addr "$SLOT_0" "$(pad32 "$DEV_ADDR2")")
BAL_FUNDED=$(calc_map_addr "$SLOT_0" "$(pad32 "$FUNDED_ADDR")")

# FeeCurrencyDirectory: currencyList array at slot 2
ARRAY_AT_2=$(cast keccak "$SLOT_2")
ARRAY_AT_2_PLUS1=$(inc_hash "$ARRAY_AT_2" 1)

# FeeCurrencyDirectory: currencyConfig mapping at slot 1
STRUCT_CE16=$(calc_map_addr "$SLOT_1" "$(pad32 "$FEE_CURRENCY")")
STRUCT_CE16_PLUS1=$(inc_hash "$STRUCT_CE16" 1)
STRUCT_CE17=$(calc_map_addr "$SLOT_1" "$(pad32 "$FEE_CURRENCY2")")
STRUCT_CE17_PLUS1=$(inc_hash "$STRUCT_CE17" 1)

# Owner field: address at offset 0 in slot 0 (right-aligned in 32 bytes)
OWNER_VALUE=$(pad32 "$DEV_ADDR")

# ---------------------------------------------------------------------------
# Read bytecodes
# ---------------------------------------------------------------------------

echo "Reading contract bytecodes..."

CODE_GOLD_TOKEN=$(read_code "$COMPILED/GoldToken.bin-runtime")
CODE_FEE_CURRENCY=$(read_code "$COMPILED/FeeCurrency.bin-runtime")
CODE_FEE_DIR=$(read_code "$COMPILED/FeeCurrencyDirectory.bin-runtime")
CODE_MOCK_ORACLE=$(read_code "$COMPILED/MockOracle.bin-runtime")

# ---------------------------------------------------------------------------
# Build genesis JSON
# ---------------------------------------------------------------------------

echo "Assembling genesis..."

# Standard prefunded accounts (Foundry/Hardhat default mnemonic)
PREFUNDED_ACCOUNTS=(
    "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
    "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
    "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"
    "0x90F79bf6EB2c4f870365E785982E1f101E93b906"
    "0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65"
    "0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"
    "0x976EA74026E726554dB657fA54763abd0C3a0aa9"
    "0x14dC79964da2C08b23698B3D3cc7Ca32193d9955"
    "0x23618e81E3f5cdF7f54C3d65f7FBc0aBf5B21E8f"
    "0xa0Ee7A142d267C1f36714E4a8F75612F20a79720"
    "0xBcd4042DE499D14e55001CcbB24a551F3b954096"
    "0x71bE63f3384f5fb98995898A86B02Fb2426c5788"
    "0xFABB0ac9d68B0B445fB7357272Ff202C5651694a"
    "0x1CBd3b2770909D4e10f157cABC84C7264073C9Ec"
    "0xdF3e18d64BC6A983f673Ab319CCaE4f1a57C7097"
    "0xcd3B766CCDd6AE721141F452C550Ca635964ce71"
    "0x2546BcD3c84621e976D8185a91A922aE77ECEc30"
    "0xbDA5747bFD65F08deb54cb465eB87D40e51B197E"
    "0xdD2FD4581271e230360230F9337D5c0430Bf44C0"
    "0x8626f6940E2eb28930eFb4CeF49B2d1F2C9C1199"
)

# Start building alloc with jq
ALLOC='{}'

# Add prefunded accounts
for addr in "${PREFUNDED_ACCOUNTS[@]}"; do
    ALLOC=$(echo "$ALLOC" | jq --arg addr "$addr" \
        '. + {($addr): {"balance": "0xD3C21BCECCEDA1000000"}}')
done

# Dev accounts
ALLOC=$(echo "$ALLOC" | jq \
    --arg dev "0x$DEV_ADDR" --arg dev2 "0x$DEV_ADDR2" \
    --arg faucet "0x$FAUCET_ADDR" --arg funded "0x$FUNDED_ADDR" \
    --arg devBal "$DEV_BALANCE_ALLOC" --arg faucetBal "$FAUCET_BALANCE" \
    '. + {
        ($dev):    {"balance": $devBal},
        ($dev2):   {"balance": $devBal},
        ($faucet): {"balance": $faucetBal},
        ($funded): {"balance": $devBal}
    }')

# Celo Token
ALLOC=$(echo "$ALLOC" | jq \
    --arg addr "0x$CELO_TOKEN" --arg code "$CODE_GOLD_TOKEN" \
    '. + {($addr): {"balance": "0x0", "code": $code}}')

# Fee Currency ce16
ALLOC=$(echo "$ALLOC" | jq \
    --arg addr "0x$FEE_CURRENCY" --arg code "$CODE_FEE_CURRENCY" \
    --arg s1 "$BAL_DEV" --arg s2 "$BAL_DEV2" --arg s3 "$BAL_FUNDED" \
    --arg bal "$DEV_BALANCE" \
    '. + {($addr): {"balance": "0x0", "code": $code, "storage": {
        ($s1): $bal, ($s2): $bal, ($s3): $bal,
        "0x0000000000000000000000000000000000000000000000000000000000000002": $bal
    }}}')

# Fee Currency ce17
ALLOC=$(echo "$ALLOC" | jq \
    --arg addr "0x$FEE_CURRENCY2" --arg code "$CODE_FEE_CURRENCY" \
    --arg s1 "$BAL_DEV" --arg s2 "$BAL_DEV2" --arg s3 "$BAL_FUNDED" \
    --arg bal "$DEV_BALANCE" \
    '. + {($addr): {"balance": "0x0", "code": $code, "storage": {
        ($s1): $bal, ($s2): $bal, ($s3): $bal,
        "0x0000000000000000000000000000000000000000000000000000000000000002": $bal
    }}}')

# Mock Oracle 1
ALLOC=$(echo "$ALLOC" | jq \
    --arg addr "0x$ORACLE1" --arg code "$CODE_MOCK_ORACLE" \
    --arg num "$RATE_NUM" --arg den "$RATE_DENOM" \
    --arg fc "$(pad32 "$FEE_CURRENCY")" \
    '. + {($addr): {"balance": "0x0", "code": $code, "storage": {
        "0x0000000000000000000000000000000000000000000000000000000000000000": $num,
        "0x0000000000000000000000000000000000000000000000000000000000000001": $den,
        "0x0000000000000000000000000000000000000000000000000000000000000003": $fc
    }}}')

# Mock Oracle 2
ALLOC=$(echo "$ALLOC" | jq \
    --arg addr "0x$ORACLE2" --arg code "$CODE_MOCK_ORACLE" \
    --arg num "$RATE_NUM2" --arg den "$RATE_DENOM" \
    --arg fc "$(pad32 "$FEE_CURRENCY2")" \
    '. + {($addr): {"balance": "0x0", "code": $code, "storage": {
        "0x0000000000000000000000000000000000000000000000000000000000000000": $num,
        "0x0000000000000000000000000000000000000000000000000000000000000001": $den,
        "0x0000000000000000000000000000000000000000000000000000000000000003": $fc
    }}}')

# Mock Oracle 3 (empty)
ALLOC=$(echo "$ALLOC" | jq \
    --arg addr "0x$ORACLE3" --arg code "$CODE_MOCK_ORACLE" \
    '. + {($addr): {"balance": "0x0", "code": $code}}')

# Fee Currency Directory
ALLOC=$(echo "$ALLOC" | jq \
    --arg addr "0x$FEE_CURRENCY_DIR" --arg code "$CODE_FEE_DIR" \
    --arg owner "$OWNER_VALUE" \
    --arg arr0 "$ARRAY_AT_2" --arg arr1 "$ARRAY_AT_2_PLUS1" \
    --arg ce16 "$(pad32 "$FEE_CURRENCY")" --arg ce17 "$(pad32 "$FEE_CURRENCY2")" \
    --arg sce16 "$STRUCT_CE16" --arg sce16i "$STRUCT_CE16_PLUS1" \
    --arg sce17 "$STRUCT_CE17" --arg sce17i "$STRUCT_CE17_PLUS1" \
    --arg o1 "$(pad32 "$ORACLE1")" --arg o2 "$(pad32 "$ORACLE2")" \
    --arg ig "$INTRINSIC_GAS" \
    '. + {($addr): {"balance": "0x0", "code": $code, "storage": {
        "0x0000000000000000000000000000000000000000000000000000000000000000": $owner,
        "0x0000000000000000000000000000000000000000000000000000000000000002": "0x0000000000000000000000000000000000000000000000000000000000000002",
        ($arr0): $ce16, ($arr1): $ce17,
        ($sce16): $o1, ($sce16i): $ig,
        ($sce17): $o2, ($sce17i): $ig
    }}}')

# Assemble full genesis
jq -n \
    --argjson alloc "$ALLOC" \
    '{
    "config": {
        "chainId": 1337,
        "homesteadBlock": 0,
        "eip150Block": 0,
        "eip155Block": 0,
        "eip158Block": 0,
        "byzantiumBlock": 0,
        "constantinopleBlock": 0,
        "petersburgBlock": 0,
        "istanbulBlock": 0,
        "muirGlacierBlock": 0,
        "berlinBlock": 0,
        "londonBlock": 0,
        "arrowGlacierBlock": 0,
        "grayGlacierBlock": 0,
        "mergeNetsplitBlock": 0,
        "shanghaiTime": 0,
        "cancunTime": 0,
        "pragueTime": 0,
        "terminalTotalDifficulty": 0,
        "terminalTotalDifficultyPassed": true,
        "bedrockBlock": 0,
        "regolithTime": 0,
        "canyonTime": 0,
        "deltaTime": 0,
        "ecotoneTime": 0,
        "fjordTime": 0,
        "graniteTime": 0,
        "holoceneTime": 0,
        "isthmusTime": 0,
        "jovianTime": 0,
        "optimism": {
            "eip1559Elasticity": 6,
            "eip1559Denominator": 50,
            "eip1559DenominatorCanyon": 250
        }
    },
    "nonce": "0x0",
    "timestamp": "0x6490fdd2",
    "extraData": "0x0100000032000000060000000000000000",
    "gasLimit": "0x1c9c380",
    "difficulty": "0x0",
    "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    "coinbase": "0x0000000000000000000000000000000000000000",
    "baseFeePerGas": "0x3b9aca00",
    "alloc": $alloc,
    "number": "0x0",
    "gasUsed": "0x0",
    "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000"
}' > "$OUT"

echo "Generated $OUT"
echo "Alloc entries: $(jq '.alloc | length' "$OUT")"
