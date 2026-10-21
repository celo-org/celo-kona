//! Canonical CGTv2 activation deposit shared by execution and derivation.

use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address};
use op_alloy_consensus::{TxDeposit, UpgradeDepositSource};

/// Existing OP upgrade source-hash domain, with a Celo-specific intent.
pub const CGT_V2_ACTIVATION_INTENT: &str = "Celo: CGTv2 Activation";
/// Fixed depositor authorized by L1BlockCGT.
pub const CGT_V2_DEPOSITOR: Address = address!("deaddeaddeaddeaddeaddeaddeaddeaddead0001");
/// L1Block proxy receiving the ordinary setter call.
pub const CGT_V2_L1_BLOCK: Address = address!("4200000000000000000000000000000000000015");
/// Fixed gas limit of the protocol-generated deposit.
pub const CGT_V2_ACTIVATION_GAS: u64 = 100_000;

/// Inputs authenticated by derivation and carried in the activation deposit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CgtV2Activation {
    /// Exact canonical L1 origin, also recorded by this block's L1-info deposit.
    pub l1_origin_hash: B256,
    /// Permanent amount actually moved to the new L1 bridge, in native base units.
    pub migrated_amount: U256,
}

/// A transaction using the activation source hash has noncanonical fields or data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CgtV2SignalError;

impl core::fmt::Display for CgtV2SignalError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("invalid CGTv2 activation deposit")
    }
}
impl core::error::Error for CgtV2SignalError {}

impl CgtV2Activation {
    /// Source hash calculated by the existing OP helper without additional inputs.
    pub fn source_hash() -> B256 {
        UpgradeDepositSource { intent: CGT_V2_ACTIVATION_INTENT.into() }.source_hash()
    }

    /// Encodes version 1 as a normal upgrade deposit calling the existing zero-argument setter.
    pub fn to_deposit(&self) -> TxDeposit {
        let mut input = [0; 100];
        input[..4].copy_from_slice(&alloy_primitives::keccak256(b"setCustomGasToken()")[..4]);
        input[35] = 1;
        input[36..68].copy_from_slice(self.l1_origin_hash.as_slice());
        input[68..].copy_from_slice(&self.migrated_amount.to_be_bytes::<32>());
        TxDeposit {
            source_hash: Self::source_hash(),
            from: CGT_V2_DEPOSITOR,
            to: TxKind::Call(CGT_V2_L1_BLOCK),
            mint: 0,
            value: U256::ZERO,
            gas_limit: CGT_V2_ACTIVATION_GAS,
            is_system_transaction: false,
            input: Bytes::copy_from_slice(&input),
        }
    }

    /// Returns None for ordinary deposits; a reserved source hash must match every field.
    pub fn decode_deposit(tx: &TxDeposit) -> Result<Option<Self>, CgtV2SignalError> {
        if tx.source_hash != Self::source_hash() {
            return Ok(None);
        }
        if tx.input.len() != 100 {
            return Err(CgtV2SignalError);
        }
        let activation = Self {
            l1_origin_hash: B256::from_slice(&tx.input[36..68]),
            migrated_amount: U256::from_be_slice(&tx.input[68..100]),
        };
        if *tx != activation.to_deposit() {
            return Err(CgtV2SignalError);
        }
        Ok(Some(activation))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, B256, TxKind, U256, keccak256};

    fn activation() -> CgtV2Activation {
        CgtV2Activation {
            l1_origin_hash: B256::repeat_byte(0x11),
            migrated_amount: U256::from(600),
        }
    }

    #[test]
    fn cgt_v2_signal_matches_v1_vector() {
        let vector: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/cgt_v2_signal_v1.json")).unwrap();
        let tx = activation().to_deposit();
        assert_eq!(tx.input.len(), 100);
        assert_eq!(tx.gas_limit, 100_000);
        assert_eq!(tx.mint, 0);
        assert_eq!(tx.value, U256::ZERO);
        assert!(!tx.is_system_transaction);
        assert_eq!(tx.source_hash, vector["sourceHash"].as_str().unwrap().parse::<B256>().unwrap());
        assert_eq!(alloy_primitives::hex::encode_prefixed(tx.encoded_2718()), vector["encoded"]);
        assert_eq!(
            keccak256(tx.encoded_2718()),
            vector["transactionHash"].as_str().unwrap().parse::<B256>().unwrap()
        );
        assert_eq!(CgtV2Activation::decode_deposit(&tx).unwrap(), Some(activation()));
    }

    #[test]
    fn cgt_v2_signal_rejects_noncanonical_fields() {
        for mutate in [
            |tx: &mut op_alloy_consensus::TxDeposit| tx.from = Address::ZERO,
            |tx: &mut op_alloy_consensus::TxDeposit| tx.to = TxKind::Create,
            |tx: &mut op_alloy_consensus::TxDeposit| tx.gas_limit += 1,
            |tx: &mut op_alloy_consensus::TxDeposit| tx.mint = 1,
            |tx: &mut op_alloy_consensus::TxDeposit| tx.value = U256::from(1),
            |tx: &mut op_alloy_consensus::TxDeposit| tx.is_system_transaction = true,
            |tx: &mut op_alloy_consensus::TxDeposit| tx.input = tx.input.slice(..99),
            |tx: &mut op_alloy_consensus::TxDeposit| {
                let mut data = tx.input.to_vec();
                data[35] = 2;
                tx.input = data.into();
            },
            |tx: &mut op_alloy_consensus::TxDeposit| {
                let mut data = tx.input.to_vec();
                data[0] ^= 1;
                tx.input = data.into();
            },
        ] {
            let mut tx = activation().to_deposit();
            mutate(&mut tx);
            assert!(CgtV2Activation::decode_deposit(&tx).is_err());
        }
        let mut unrelated = activation().to_deposit();
        unrelated.source_hash = B256::ZERO;
        assert_eq!(CgtV2Activation::decode_deposit(&unrelated).unwrap(), None);
    }
}
