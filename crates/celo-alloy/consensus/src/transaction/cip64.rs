//! CIP-64 Transaction type.

use crate::CeloTxType;
use alloy_consensus::{
    SignableTransaction, Transaction,
    transaction::{RlpEcdsaDecodableTx, RlpEcdsaEncodableTx},
};
use alloy_eips::{
    Typed2718, eip2718::IsTyped2718, eip2930::AccessList, eip7702::SignedAuthorization,
};
use alloy_primitives::{Address, B256, Bytes, ChainId, Signature, TxKind, U256};
use alloy_rlp::{BufMut, Decodable, Encodable};
use alloy_rpc_types_eth::TransactionRequest;
use core::mem;

/// A transaction with a fee currency ([CIP-64](https://github.com/celo-org/celo-proposals/blob/master/CIPs/cip-0064.md)).
///
/// This transaction type is identical to EIP-1559 but with a different type identifier (0x7b)
/// and an additional `fee_currency` field to specify the currency to pay for gas.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "reth", derive(reth_codecs::Compact))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
#[doc(alias = "Cip64Transaction", alias = "TransactionCip64", alias = "Cip64Tx")]
pub struct TxCip64 {
    /// EIP-155: Simple replay attack protection
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub chain_id: ChainId,
    /// A scalar value equal to the number of transactions sent by the sender; formally Tn.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub nonce: u64,
    /// A scalar value equal to the maximum
    /// amount of gas that should be used in executing
    /// this transaction. This is paid up-front, before any
    /// computation is done and may not be increased
    /// later; formally Tg.
    #[cfg_attr(
        feature = "serde",
        serde(with = "alloy_serde::quantity", rename = "gas", alias = "gasLimit")
    )]
    pub gas_limit: u64,
    /// A scalar value equal to the maximum
    /// amount of gas that should be used in executing
    /// this transaction. This is paid up-front, before any
    /// computation is done and may not be increased
    /// later; formally Tg.
    ///
    /// As ethereum circulation is around 120mil eth as of 2022 that is around
    /// 120000000000000000000000000 wei we are safe to use u128 as its max number is:
    /// 340282366920938463463374607431768211455
    ///
    /// This is also known as `GasFeeCap`
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub max_fee_per_gas: u128,
    /// Max Priority fee that transaction is paying
    ///
    /// As ethereum circulation is around 120mil eth as of 2022 that is around
    /// 120000000000000000000000000 wei we are safe to use u128 as its max number is:
    /// 340282366920938463463374607431768211455
    ///
    /// This is also known as `GasTipCap`
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub max_priority_fee_per_gas: u128,
    /// The 160-bit address of the message call's recipient or, for a contract creation
    /// transaction, ∅, used here to denote the only member of B0 ; formally Tt.
    #[cfg_attr(feature = "serde", serde(default))]
    pub to: TxKind,
    /// A scalar value equal to the number of Wei to
    /// be transferred to the message call's recipient or,
    /// in the case of contract creation, as an endowment
    /// to the newly created account; formally Tv.
    pub value: U256,
    /// The accessList specifies a list of addresses and storage keys;
    /// these addresses and storage keys are added into the `accessed_addresses`
    /// and `accessed_storage_keys` global sets (introduced in EIP-2929).
    /// A gas cost is charged, though at a discount relative to the cost of
    /// accessing outside the list.
    pub access_list: AccessList,
    /// The address of the whitelisted currency to be used to pay for gas.
    /// This is a Celo-specific field not present in Ethereum transactions.
    /// None means the native currency is used.
    pub fee_currency: Option<Address>,
    /// Input has two uses depending if `to` field is Create or Call.
    /// pub init: An unlimited size byte array specifying the
    /// EVM-code for the account initialisation procedure CREATE,
    /// data: An unlimited size byte array specifying the
    /// input data of the message call, formally Td.
    ///
    /// NOTE: This field MUST be the last field in the struct because
    /// `reth_codecs::Compact` requires `Bytes` fields to be last.
    pub input: Bytes,
}

impl TxCip64 {
    /// Get the transaction type
    #[doc(alias = "transaction_type")]
    pub const fn tx_type() -> CeloTxType {
        CeloTxType::Cip64
    }

    /// Calculates a heuristic for the in-memory size of the [TxCip64]
    /// transaction.
    #[inline]
    pub fn size(&self) -> usize {
        let Self {
            chain_id,
            nonce,
            gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            to,
            value,
            access_list,
            fee_currency,
            input,
        } = self;
        mem::size_of_val(chain_id)
            + mem::size_of_val(nonce)
            + mem::size_of_val(gas_limit)
            + mem::size_of_val(max_fee_per_gas)
            + mem::size_of_val(max_priority_fee_per_gas)
            + to.size()
            + mem::size_of_val(value)
            + access_list.size()
            + mem::size_of_val(fee_currency)
            + mem::size_of_val(input)
            + input.len()
    }
}

impl RlpEcdsaEncodableTx for TxCip64 {
    /// Outputs the length of the transaction's fields, without a RLP header.
    fn rlp_encoded_fields_length(&self) -> usize {
        self.chain_id.length()
            + self.nonce.length()
            + self.max_priority_fee_per_gas.length()
            + self.max_fee_per_gas.length()
            + self.gas_limit.length()
            + self.to.length()
            + self.value.length()
            + self.input.0.length()
            + self.access_list.length()
            + self.fee_currency.as_ref().map_or(1, |addr| addr.length())
    }

    /// Encodes only the transaction's fields into the desired buffer, without
    /// a RLP header.
    fn rlp_encode_fields(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.chain_id.encode(out);
        self.nonce.encode(out);
        self.max_priority_fee_per_gas.encode(out);
        self.max_fee_per_gas.encode(out);
        self.gas_limit.encode(out);
        self.to.encode(out);
        self.value.encode(out);
        self.input.0.encode(out);
        self.access_list.encode(out);
        // Always encode fee_currency, None as empty string
        match self.fee_currency {
            Some(addr) => addr.encode(out),
            None => (&[] as &[u8]).encode(out),
        }
    }
}

impl RlpEcdsaDecodableTx for TxCip64 {
    const DEFAULT_TX_TYPE: u8 = { Self::tx_type() as u8 };

    /// Decodes the inner [TxCip64] fields from RLP bytes.
    ///
    /// NOTE: This assumes a RLP header has already been decoded, and _just_
    /// decodes the following RLP fields in the following order:
    ///
    /// - `chain_id`
    /// - `nonce`
    /// - `max_priority_fee_per_gas`
    /// - `max_fee_per_gas`
    /// - `gas_limit`
    /// - `to`
    /// - `value`
    /// - `data` (`input`)
    /// - `access_list`
    /// - `fee_currency`
    fn rlp_decode_fields(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Ok(Self {
            chain_id: Decodable::decode(buf)?,
            nonce: Decodable::decode(buf)?,
            max_priority_fee_per_gas: Decodable::decode(buf)?,
            max_fee_per_gas: Decodable::decode(buf)?,
            gas_limit: Decodable::decode(buf)?,
            to: Decodable::decode(buf)?,
            value: Decodable::decode(buf)?,
            input: Decodable::decode(buf)?,
            access_list: Decodable::decode(buf)?,
            fee_currency: {
                let bytes: Bytes = Decodable::decode(buf)?;
                if bytes.is_empty() {
                    None
                } else if bytes.len() == 20 {
                    Some(Address::from_slice(&bytes))
                } else {
                    return Err(alloy_rlp::Error::Custom("Invalid fee_currency address length"));
                }
            },
        })
    }
}

impl Transaction for TxCip64 {
    #[inline]
    fn chain_id(&self) -> Option<ChainId> {
        Some(self.chain_id)
    }

    #[inline]
    fn nonce(&self) -> u64 {
        self.nonce
    }

    #[inline]
    fn gas_limit(&self) -> u64 {
        self.gas_limit
    }

    #[inline]
    fn gas_price(&self) -> Option<u128> {
        None
    }

    #[inline]
    fn max_fee_per_gas(&self) -> u128 {
        self.max_fee_per_gas
    }

    #[inline]
    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        Some(self.max_priority_fee_per_gas)
    }

    #[inline]
    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        None
    }

    #[inline]
    fn priority_fee_or_price(&self) -> u128 {
        self.max_priority_fee_per_gas
    }

    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        base_fee.map_or(self.max_fee_per_gas, |base_fee| {
            // if the tip is greater than the max priority fee per gas, set it to the max
            // priority fee per gas + base fee
            let tip = self.max_fee_per_gas.saturating_sub(base_fee as u128);
            if tip > self.max_priority_fee_per_gas {
                self.max_priority_fee_per_gas + base_fee as u128
            } else {
                // otherwise return the max fee per gas
                self.max_fee_per_gas
            }
        })
    }

    #[inline]
    fn is_dynamic_fee(&self) -> bool {
        true
    }

    #[inline]
    fn kind(&self) -> TxKind {
        self.to
    }

    #[inline]
    fn is_create(&self) -> bool {
        self.to.is_create()
    }

    #[inline]
    fn value(&self) -> U256 {
        self.value
    }

    #[inline]
    fn input(&self) -> &Bytes {
        &self.input
    }

    #[inline]
    fn access_list(&self) -> Option<&AccessList> {
        Some(&self.access_list)
    }

    #[inline]
    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        None
    }

    #[inline]
    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        None
    }
}

impl Typed2718 for TxCip64 {
    fn ty(&self) -> u8 {
        CeloTxType::Cip64 as u8
    }
}

impl IsTyped2718 for TxCip64 {
    fn is_type(type_id: u8) -> bool {
        type_id == CeloTxType::Cip64 as u8
    }
}

impl SignableTransaction<Signature> for TxCip64 {
    fn set_chain_id(&mut self, chain_id: ChainId) {
        self.chain_id = chain_id;
    }

    fn encode_for_signing(&self, out: &mut dyn alloy_rlp::BufMut) {
        out.put_u8(Self::tx_type().into());
        self.encode(out)
    }

    fn payload_len_for_signature(&self) -> usize {
        self.length() + 1
    }
}

impl Encodable for TxCip64 {
    fn encode(&self, out: &mut dyn BufMut) {
        self.rlp_encode(out);
    }

    fn length(&self) -> usize {
        self.rlp_encoded_length()
    }
}

impl Decodable for TxCip64 {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Self::rlp_decode(buf)
    }
}

impl From<TxCip64> for TransactionRequest {
    fn from(tx: TxCip64) -> Self {
        let ty = tx.ty();
        let TxCip64 {
            chain_id,
            nonce,
            gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            to,
            value,
            access_list,
            input,
            fee_currency: _,
        } = tx;
        Self {
            to: if let TxKind::Call(to) = to { Some(to.into()) } else { None },
            max_fee_per_gas: Some(max_fee_per_gas),
            max_priority_fee_per_gas: Some(max_priority_fee_per_gas),
            gas: Some(gas_limit),
            value: Some(value),
            input: input.into(),
            nonce: Some(nonce),
            chain_id: Some(chain_id),
            access_list: Some(access_list),
            transaction_type: Some(ty),
            ..Default::default()
        }
    }
}

/// Bincode-compatible [`TxCip64`] serde implementation.
#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub(crate) mod serde_bincode_compat {
    use alloy_eips::eip2930::AccessList;
    use alloy_primitives::{Address, Bytes, ChainId, TxKind, U256};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_with::{DeserializeAs, SerializeAs};
    use std::borrow::Cow;

    /// Bincode-compatible [`super::TxCip64`] serde implementation.
    ///
    /// Intended to use with the [`serde_with::serde_as`] macro in the following way:
    /// ```rust
    /// use celo_alloy_consensus::{TxCip64, serde_bincode_compat};
    /// use serde::{Deserialize, Serialize};
    /// use serde_with::serde_as;
    ///
    /// #[serde_as]
    /// #[derive(Serialize, Deserialize)]
    /// struct Data {
    ///     #[serde_as(as = "serde_bincode_compat::TxCip64")]
    ///     transaction: TxCip64,
    /// }
    /// ```
    #[derive(Debug, Serialize, Deserialize)]
    pub struct TxCip64<'a> {
        chain_id: ChainId,
        nonce: u64,
        gas_limit: u64,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        #[serde(default)]
        to: TxKind,
        value: U256,
        access_list: Cow<'a, AccessList>,
        input: Cow<'a, Bytes>,
        fee_currency: Option<Address>,
    }

    impl<'a> From<&'a super::TxCip64> for TxCip64<'a> {
        fn from(value: &'a super::TxCip64) -> Self {
            Self {
                chain_id: value.chain_id,
                nonce: value.nonce,
                gas_limit: value.gas_limit,
                max_fee_per_gas: value.max_fee_per_gas,
                max_priority_fee_per_gas: value.max_priority_fee_per_gas,
                to: value.to,
                value: value.value,
                access_list: Cow::Borrowed(&value.access_list),
                input: Cow::Borrowed(&value.input),
                fee_currency: value.fee_currency,
            }
        }
    }

    impl<'a> From<TxCip64<'a>> for super::TxCip64 {
        fn from(value: TxCip64<'a>) -> Self {
            Self {
                chain_id: value.chain_id,
                nonce: value.nonce,
                gas_limit: value.gas_limit,
                max_fee_per_gas: value.max_fee_per_gas,
                max_priority_fee_per_gas: value.max_priority_fee_per_gas,
                to: value.to,
                value: value.value,
                access_list: value.access_list.into_owned(),
                input: value.input.into_owned(),
                fee_currency: value.fee_currency,
            }
        }
    }

    impl SerializeAs<super::TxCip64> for TxCip64<'_> {
        fn serialize_as<S>(source: &super::TxCip64, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            TxCip64::from(source).serialize(serializer)
        }
    }

    impl<'de> DeserializeAs<'de, super::TxCip64> for TxCip64<'de> {
        fn deserialize_as<D>(deserializer: D) -> Result<super::TxCip64, D::Error>
        where
            D: Deserializer<'de>,
        {
            TxCip64::deserialize(deserializer).map(Into::into)
        }
    }

    #[cfg(test)]
    mod tests {
        use arbitrary::Arbitrary;
        use rand::Rng;
        use serde::{Deserialize, Serialize};
        use serde_with::serde_as;

        use super::super::{TxCip64, serde_bincode_compat};

        #[test]
        fn test_tx_cip64_bincode_roundtrip() {
            #[serde_as]
            #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
            struct Data {
                #[serde_as(as = "serde_bincode_compat::TxCip64")]
                transaction: TxCip64,
            }

            let mut bytes = [0u8; 1024];
            rand::rng().fill(bytes.as_mut_slice());
            let data = Data {
                transaction: TxCip64::arbitrary(&mut arbitrary::Unstructured::new(&bytes)).unwrap(),
            };

            let encoded = bincode::serde::encode_to_vec(&data, bincode::config::legacy()).unwrap();
            let (decoded, _) =
                bincode::serde::decode_from_slice::<Data, _>(&encoded, bincode::config::legacy())
                    .unwrap();
            assert_eq!(decoded, data);
        }
    }
}

#[cfg(all(test, feature = "k256"))]
mod tests {
    use super::{SignableTransaction, TxCip64};
    use alloy_consensus::transaction::{RlpEcdsaDecodableTx, RlpEcdsaEncodableTx};
    use alloy_eips::eip2930::AccessList;
    use alloy_primitives::{Address, B256, Signature, U256, address, b256, hex};

    /*  The following real tx is used as test data:

    > cast tx --raw 0x2499b178a0e54fb856354bb53c9cb0fe2d1f70b2e6c876f6470e7a6e5090f3ac
    0x7bf8a882a4ec8207058304d7ee85026442dbed8303644c947a1e295c4babdf229776680c93ed0f73d069abc080a4cac35c7a290decd9548b62a8d60345a988386fc84ba6bc95484008f6362f93160ef3e563c0942f25deb3848c207fc8e0c34035b3ba7fc157602b80a0aa0cfaa3df893578b3504062b862428f0e4a94046370cf2a4fd6c392c0760dd8a01337d022bbb8faed78a9707e6c38d51f575816e7f85aa540f3d37a9081c58a71

    > cast tx 0x2499b178a0e54fb856354bb53c9cb0fe2d1f70b2e6c876f6470e7a6e5090f3ac --json | jq .
    {
      "hash": "0x2499b178a0e54fb856354bb53c9cb0fe2d1f70b2e6c876f6470e7a6e5090f3ac",
      "nonce": "0x705",
      "blockHash": "0x6a0ab3a90053fb0157fade164fb34c45e805f0e236cdabc52ac6a0b7c408d32c",
      "blockNumber": "0x1ef4fb5",
      "transactionIndex": "0x14",
      "from": "0xefe945ee33ce4ab037ff4d1e1384d0efcd95f37b",
      "to": "0x7a1e295c4babdf229776680c93ed0f73d069abc0",
      "value": "0x0",
      "gasPrice": "0x1ae0426f1",
      "gas": "0x3644c",
      "maxFeePerGas": "0x26442dbed",
      "maxPriorityFeePerGas": "0x4d7ee",
      "input": "0xcac35c7a290decd9548b62a8d60345a988386fc84ba6bc95484008f6362f93160ef3e563",
      "r": "0xaa0cfaa3df893578b3504062b862428f0e4a94046370cf2a4fd6c392c0760dd8",
      "s": "0x1337d022bbb8faed78a9707e6c38d51f575816e7f85aa540f3d37a9081c58a71",
      "v": "0x0",
      "yParity": "0x0",
      "chainId": "0xa4ec",
      "accessList": [],
      "type": "0x7b",
      "feeCurrency": "0x2f25deb3848c207fc8e0c34035b3ba7fc157602b"
    }

    >  cast receipt 0x2499b178a0e54fb856354bb53c9cb0fe2d1f70b2e6c876f6470e7a6e5090f3ac --json | jq 'del(.logs)'
    {
      "status": "0x1",
      "cumulativeGasUsed": "0x13fa23",
      "logsBloom": "0x00000000000000000000100000000000000010400000000000000000000000000000000000000000000000000040000100000000000000000000000000002000000000000000000000010008000000000000000000000000000400420000000000000000020000000000000000000800000000000000000040000010000000000000010000000008000000000000000000000000000000000000000000000000000000000000000000000000400000000000000000000008000000020000000000000002000000000000028000000000000000000000000000000800000020000000000000100008008800000000400000000000000000000000000000000000",
      "type": "0x7b",
      "transactionHash": "0x2499b178a0e54fb856354bb53c9cb0fe2d1f70b2e6c876f6470e7a6e5090f3ac",
      "transactionIndex": "0x14",
      "blockHash": "0x6a0ab3a90053fb0157fade164fb34c45e805f0e236cdabc52ac6a0b7c408d32c",
      "blockNumber": "0x1ef4fb5",
      "gasUsed": "0x2b45b",
      "effectiveGasPrice": "0x1ae0426f1",
      "from": "0xefe945ee33ce4ab037ff4d1e1384d0efcd95f37b",
      "to": "0x7a1e295c4babdf229776680c93ed0f73d069abc0",
      "contractAddress": null,
      "baseFee": "0x1adff4f03",
      "l1BaseFeeScalar": "0x0",
      "l1BlobBaseFee": "0x760c6b",
      "l1BlobBaseFeeScalar": "0x0",
      "l1Fee": "0x0",
      "l1GasPrice": "0x22654238",
      "l1GasUsed": "0x697"
    }
    */

    #[test]
    fn recover_signer_cip64() {
        let signer: Address = address!("0xefe945ee33ce4ab037ff4d1e1384d0efcd95f37b");
        let hash: B256 = b256!("2499b178a0e54fb856354bb53c9cb0fe2d1f70b2e6c876f6470e7a6e5090f3ac");

        let tx = TxCip64 {
            chain_id: 0xa4ec,
            nonce: 0x705,
            gas_limit: 0x3644c,
            to: address!("0x7a1e295c4babdf229776680c93ed0f73d069abc0").into(),
            value: U256::from(0_u64),
            input: hex!(
                "0xcac35c7a290decd9548b62a8d60345a988386fc84ba6bc95484008f6362f93160ef3e563"
            )
            .into(),
            max_fee_per_gas: 0x26442dbed,
            max_priority_fee_per_gas: 0x4d7ee,
            access_list: AccessList::default(),
            fee_currency: Some(address!("0x2f25deb3848c207fc8e0c34035b3ba7fc157602b")),
        };

        let sig = Signature::from_scalars_and_parity(
            b256!("0xaa0cfaa3df893578b3504062b862428f0e4a94046370cf2a4fd6c392c0760dd8"),
            b256!("0x1337d022bbb8faed78a9707e6c38d51f575816e7f85aa540f3d37a9081c58a71"),
            false,
        );

        let signed_tx = tx.into_signed(sig);
        assert_eq!(*signed_tx.hash(), hash, "Expected same hash");
        assert_eq!(signed_tx.recover_signer().unwrap(), signer, "Recovering signer should pass.");
    }

    #[test]
    fn encode_decode_cip64() {
        let hash: B256 =
            b256!("0x2499b178a0e54fb856354bb53c9cb0fe2d1f70b2e6c876f6470e7a6e5090f3ac");

        let tx = TxCip64 {
            chain_id: 0xa4ec,
            nonce: 0x705,
            gas_limit: 0x3644c,
            to: address!("0x7a1e295c4babdf229776680c93ed0f73d069abc0").into(),
            value: U256::from(0_u64),
            input: hex!(
                "0xcac35c7a290decd9548b62a8d60345a988386fc84ba6bc95484008f6362f93160ef3e563"
            )
            .into(),
            max_fee_per_gas: 0x26442dbed,
            max_priority_fee_per_gas: 0x4d7ee,
            access_list: AccessList::default(),
            fee_currency: Some(address!("0x2f25deb3848c207fc8e0c34035b3ba7fc157602b")),
        };

        let sig = Signature::from_scalars_and_parity(
            b256!("0xaa0cfaa3df893578b3504062b862428f0e4a94046370cf2a4fd6c392c0760dd8"),
            b256!("0x1337d022bbb8faed78a9707e6c38d51f575816e7f85aa540f3d37a9081c58a71"),
            false,
        );

        let mut buf = vec![];
        tx.rlp_encode_signed(&sig, &mut buf);
        let decoded = TxCip64::rlp_decode_signed(&mut &buf[..]).unwrap();
        assert_eq!(decoded, tx.into_signed(sig));
        assert_eq!(*decoded.hash(), hash);

        // Also verify that the encoded transaction matches what we observed on the real blockchain
        let expected_encoded_tx = "0x7bf8a882a4ec8207058304d7ee85026442dbed8303644c947a1e295c4babdf229776680c93ed0f73d069abc080a4cac35c7a290decd9548b62a8d60345a988386fc84ba6bc95484008f6362f93160ef3e563c0942f25deb3848c207fc8e0c34035b3ba7fc157602b80a0aa0cfaa3df893578b3504062b862428f0e4a94046370cf2a4fd6c392c0760dd8a01337d022bbb8faed78a9707e6c38d51f575816e7f85aa540f3d37a9081c58a71";
        assert_eq!(format!("0x7b{}", hex::encode(buf)), expected_encoded_tx);
    }

    /*  The following real tx is used as test data:

    > call `debug_getRawTransaction` 0xb7ae6b434382473c8beaba396d0bb119be1738d2ec1381e58be7cf45c65312bf on Baklava
    0x7bf86e82f37002830f42408506fc23ac00830186a094fb968b52d25549ec2dd26a9f650a0a0f135a43588080c08001a073220f0dbf123a982c75f006d76c6949e75a2bf89255432e2bf2c5016c37d0f3a01c69a201f1c24233c4a0fe8539073a7b4d3b370a25b8c00d2cf8e93a3694b9b7

    > cast tx 0xb7ae6b434382473c8beaba396d0bb119be1738d2ec1381e58be7cf45c65312bf --json | jq .
    {
      "hash": "0xb7ae6b434382473c8beaba396d0bb119be1738d2ec1381e58be7cf45c65312bf",
      "type": "0x7b",
      "accessList": [],
      "chainId": "0xf370",
      "gas": "0x186a0",
      "gasPrice": "0x5d22cfc40",
      "input": "0x",
      "maxFeePerGas": "0x6fc23ac00",
      "maxPriorityFeePerGas": "0xf4240",
      "nonce": "0x2",
      "r": "0x73220f0dbf123a982c75f006d76c6949e75a2bf89255432e2bf2c5016c37d0f3",
      "s": "0x1c69a201f1c24233c4a0fe8539073a7b4d3b370a25b8c00d2cf8e93a3694b9b7",
      "to": "0xfb968b52d25549ec2dd26a9f650a0a0f135a4358",
      "v": "0x1",
      "value": "0x0",
      "yParity": "0x1",
      "blockHash": "0x51a4dc762c46b813c1454e65dee028e427a8bf33615a6ba93fb992bde989bdfd",
      "blockNumber": "0x26ec371",
      "transactionIndex": "0x1",
      "from": "0x52bcbd8bf68ee24a15adcd05951a49ae6c168a14"
    }

    >  cast receipt 0xb7ae6b434382473c8beaba396d0bb119be1738d2ec1381e58be7cf45c65312bf --json | jq 'del(.logs)'
    {
      "status": "0x1",
      "cumulativeGasUsed": "0x105e8",
      "logsBloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
      "type": "0x7b",
      "transactionHash": "0xb7ae6b434382473c8beaba396d0bb119be1738d2ec1381e58be7cf45c65312bf",
      "transactionIndex": "0x1",
      "blockHash": "0x51a4dc762c46b813c1454e65dee028e427a8bf33615a6ba93fb992bde989bdfd",
      "blockNumber": "0x26ec371",
      "gasUsed": "0x5208",
      "effectiveGasPrice": "0x5d22cfc40",
      "from": "0x52bcbd8bf68ee24a15adcd05951a49ae6c168a14",
      "to": "0xfb968b52d25549ec2dd26a9f650a0a0f135a4358",
      "contractAddress": null,
      "baseFee": "0x5d21dba00",
      "l1BaseFeeScalar": "0x0",
      "l1BlobBaseFee": "0x1",
      "l1BlobBaseFeeScalar": "0x0",
      "l1Fee": "0x0",
      "l1GasPrice": "0xa",
      "l1GasUsed": "0x640"
    }
    */

    #[test]
    fn encode_decode_cip64_with_none_fee_currency() {
        let hash: B256 =
            b256!("0xb7ae6b434382473c8beaba396d0bb119be1738d2ec1381e58be7cf45c65312bf");

        let tx = TxCip64 {
            chain_id: 0xf370,
            nonce: 0x2,
            gas_limit: 0x186a0,
            to: address!("0xfb968b52d25549ec2dd26a9f650a0a0f135a4358").into(),
            value: U256::from(0_u64),
            input: hex!("0x").into(),
            max_fee_per_gas: 0x6fc23ac00,
            max_priority_fee_per_gas: 0xf4240,
            access_list: AccessList::default(),
            fee_currency: None,
        };

        let sig = Signature::from_scalars_and_parity(
            b256!("0x73220f0dbf123a982c75f006d76c6949e75a2bf89255432e2bf2c5016c37d0f3"),
            b256!("0x1c69a201f1c24233c4a0fe8539073a7b4d3b370a25b8c00d2cf8e93a3694b9b7"),
            true,
        );

        let mut buf = vec![];
        tx.rlp_encode_signed(&sig, &mut buf);
        let decoded = TxCip64::rlp_decode_signed(&mut &buf[..]).unwrap();
        assert_eq!(decoded, tx.into_signed(sig));
        assert_eq!(*decoded.hash(), hash);

        // Also verify that the encoded transaction matches what we observed on the real blockchain
        let expected_encoded_tx = "0x7bf86e82f37002830f42408506fc23ac00830186a094fb968b52d25549ec2dd26a9f650a0a0f135a43588080c08001a073220f0dbf123a982c75f006d76c6949e75a2bf89255432e2bf2c5016c37d0f3a01c69a201f1c24233c4a0fe8539073a7b4d3b370a25b8c00d2cf8e93a3694b9b7";
        assert_eq!(format!("0x7b{}", hex::encode(buf)), expected_encoded_tx);
    }
}

/// Tests that don't need k256 (forwarding/encoding/size assertions).
#[cfg(test)]
mod forwarding_tests {
    use super::*;
    use alloy_eips::{eip2718::IsTyped2718, eip2930::AccessListItem};
    use alloy_primitives::{address, b256, hex};
    use alloy_rpc_types_eth::TransactionRequest;
    use std::{vec, vec::Vec};

    /// Builds a TxCip64 with deliberately distinct, non-default values for
    /// every accessor. Used by the trait-forwarding test below to pin each
    /// constant-replacement mutant.
    fn populated_tx() -> TxCip64 {
        TxCip64 {
            chain_id: 0xa4ec,
            nonce: 0x705,
            gas_limit: 0x3644c,
            to: TxKind::Call(address!("0x7a1e295c4babdf229776680c93ed0f73d069abc0")),
            // Non-zero, non-default value pins `value -> Default`.
            value: U256::from(0xabc_u64),
            input: Bytes::copy_from_slice(&hex!(
                "0xcac35c7a290decd9548b62a8d60345a988386fc84ba6bc95484008f6362f93160ef3e563"
            )),
            max_fee_per_gas: 0x26442dbed,
            max_priority_fee_per_gas: 0x4d7ee,
            access_list: AccessList(vec![AccessListItem {
                address: address!("0xdd00000000000000000000000000000000000004"),
                storage_keys: vec![b256!(
                    "0x4444444444444444444444444444444444444444444444444444444444444444"
                )],
            }]),
            fee_currency: Some(address!("0x2f25deb3848c207fc8e0c34035b3ba7fc157602b")),
        }
    }

    /// Pins every `Transaction` accessor against `-> Default`/`-> 0|1`/`->
    /// None`. CIP-64 has no gas_price (returns None), no blob fields
    /// (returns None), and no authorization list (returns None) — those are
    /// asserted as None and pin the `Some(...)` mutants. The
    /// `max_fee_per_blob_gas -> None` and `blob_versioned_hashes -> None`
    /// mutants are equivalent and excluded in `.cargo/mutants.toml`.
    #[test]
    fn cip64_transaction_trait_forwards_every_field() {
        let tx = populated_tx();
        assert_eq!(tx.chain_id(), Some(0xa4ec));
        assert_eq!(tx.nonce(), 0x705);
        assert_eq!(tx.gas_limit(), 0x3644c);
        assert_eq!(<TxCip64 as Transaction>::gas_price(&tx), None);
        assert_eq!(tx.max_fee_per_gas(), 0x26442dbed);
        assert_eq!(tx.max_priority_fee_per_gas(), Some(0x4d7ee));
        assert_eq!(tx.max_fee_per_blob_gas(), None);
        assert_eq!(tx.priority_fee_or_price(), 0x4d7ee);
        // base_fee=100 → tip = max_fee - base = 0x26442dbed - 100 > max_priority,
        // so effective is max_priority + base_fee.
        assert_eq!(tx.effective_gas_price(Some(100)), 0x4d7ee + 100);
        // base_fee=None → returns max_fee_per_gas directly.
        assert_eq!(tx.effective_gas_price(None), 0x26442dbed);
        assert!(tx.is_dynamic_fee());
        assert_eq!(tx.kind(), TxKind::Call(address!("0x7a1e295c4babdf229776680c93ed0f73d069abc0")),);
        assert!(!tx.is_create());
        assert_eq!(tx.value(), U256::from(0xabc_u64));
        assert_eq!(tx.input().len(), populated_tx().input.len());
        let access = tx.access_list().expect("Some access list");
        assert_eq!(access.len(), 1);
        assert_eq!(tx.blob_versioned_hashes(), None);
        assert_eq!(tx.authorization_list(), None);
    }

    /// Pins `effective_gas_price` strict-inequality branch (line 256). When
    /// `tip <= max_priority`, the function returns `max_fee_per_gas`. With
    /// `>` flipped to `<` it'd return `max_priority + base_fee`.
    #[test]
    fn cip64_effective_gas_price_when_tip_at_priority_returns_max_fee() {
        let mut tx = populated_tx();
        tx.max_fee_per_gas = 100;
        tx.max_priority_fee_per_gas = 100;
        // base_fee=0 → tip = 100 - 0 = 100, NOT > max_priority(100).
        // Falls through to else → returns max_fee_per_gas (100).
        assert_eq!(tx.effective_gas_price(Some(0)), 100);
    }

    /// Pins `is_create -> false` against a contract-creation tx
    /// (TxKind::Create) where real returns true.
    #[test]
    fn cip64_is_create_returns_true_for_creation() {
        let mut tx = populated_tx();
        tx.to = TxKind::Create;
        assert!(tx.is_create());
        assert_eq!(tx.kind(), TxKind::Create);
    }

    /// Pins `Typed2718::ty -> 0|1` and `IsTyped2718::is_type` arms.
    #[test]
    fn cip64_typed2718_returns_cip64_type_id() {
        let tx = populated_tx();
        use alloy_consensus::Typed2718;
        assert_eq!(<TxCip64 as Typed2718>::ty(&tx), 0x7b);
        assert!(<TxCip64 as IsTyped2718>::is_type(0x7b));
        assert!(!<TxCip64 as IsTyped2718>::is_type(0x7a));
        assert!(!<TxCip64 as IsTyped2718>::is_type(0x7c));
    }

    /// Pins the `size()` arithmetic against `+ -> -|*` and `-> 0|1`. The
    /// sum is dominated by the input.len() term so any flip changes the
    /// result. We assert size() equals the explicit per-field sum.
    #[test]
    fn cip64_size_equals_explicit_field_sum() {
        let tx = populated_tx();
        let expected = core::mem::size_of_val(&tx.chain_id)
            + core::mem::size_of_val(&tx.nonce)
            + core::mem::size_of_val(&tx.gas_limit)
            + core::mem::size_of_val(&tx.max_fee_per_gas)
            + core::mem::size_of_val(&tx.max_priority_fee_per_gas)
            + tx.to.size()
            + core::mem::size_of_val(&tx.value)
            + tx.access_list.size()
            + core::mem::size_of_val(&tx.fee_currency)
            + core::mem::size_of_val(&tx.input)
            + tx.input.len();
        assert_eq!(tx.size(), expected);
    }

    /// Pins `SignableTransaction::set_chain_id -> ()`.
    #[test]
    fn cip64_set_chain_id_mutates_field() {
        let mut tx = populated_tx();
        tx.set_chain_id(0xfade);
        assert_eq!(tx.chain_id, 0xfade);
    }

    /// Pins `SignableTransaction::encode_for_signing -> ()` and
    /// `payload_len_for_signature -> 0|1`. The encoded buffer's length must
    /// equal the claimed payload_len.
    #[test]
    fn cip64_signable_encoding_matches_claimed_length() {
        let tx = populated_tx();
        let mut buf = Vec::new();
        tx.encode_for_signing(&mut buf);
        assert!(buf.len() > 1);
        assert_eq!(buf.len(), tx.payload_len_for_signature());
        // First byte is the tx_type prefix (0x7b).
        assert_eq!(buf[0], 0x7b);
    }

    /// Pins each `delete field` mutation in `From<TxCip64> for
    /// TransactionRequest` (lines 364-374). Each request field must reflect
    /// the source tx field.
    #[test]
    fn cip64_into_transaction_request_populates_every_field() {
        let tx = populated_tx();
        let req: TransactionRequest = tx.clone().into();
        assert_eq!(req.chain_id, Some(tx.chain_id));
        assert_eq!(req.nonce, Some(tx.nonce));
        assert_eq!(req.gas, Some(tx.gas_limit));
        assert_eq!(req.max_fee_per_gas, Some(tx.max_fee_per_gas));
        assert_eq!(req.max_priority_fee_per_gas, Some(tx.max_priority_fee_per_gas));
        assert_eq!(
            req.to,
            Some(TxKind::Call(address!("0x7a1e295c4babdf229776680c93ed0f73d069abc0"))),
        );
        assert_eq!(req.value, Some(tx.value));
        assert_eq!(req.access_list, Some(tx.access_list));
        assert_eq!(req.transaction_type, Some(0x7b));
        assert_eq!(req.input.input(), Some(&tx.input));
    }

    /// `From<TxCip64>` for a contract-creation tx must leave `to = None`,
    /// not `Some(Default)`. Pins the `if let TxKind::Call(to) ... else None`
    /// branch against `Default::default()` substitution.
    #[test]
    fn cip64_into_transaction_request_creation_leaves_to_none() {
        let mut tx = populated_tx();
        tx.to = TxKind::Create;
        let req: TransactionRequest = tx.into();
        assert_eq!(req.to, None);
    }

    /// Pins `RlpEcdsaEncodableTx::rlp_encoded_fields_length` against
    /// `-> 0|1`. The length must equal the actual `rlp_encode_fields`
    /// output length.
    #[test]
    fn cip64_rlp_encoded_fields_length_matches_actual() {
        use alloy_consensus::transaction::RlpEcdsaEncodableTx;
        let tx = populated_tx();
        let mut buf = Vec::new();
        tx.rlp_encode_fields(&mut buf);
        assert_eq!(buf.len(), tx.rlp_encoded_fields_length());
    }

    /// Pins the `fee_currency: None -> [] as &[u8]` encoding branch in
    /// `rlp_encode_fields` (lines 160-163) and the corresponding
    /// `rlp_decode_fields` branch (line 198). Round-trip a tx with
    /// fee_currency=None and assert it decodes back to None.
    #[test]
    fn cip64_rlp_round_trip_with_none_fee_currency() {
        use alloy_consensus::transaction::{RlpEcdsaDecodableTx, RlpEcdsaEncodableTx};
        let mut tx = populated_tx();
        tx.fee_currency = None;
        let mut buf = Vec::new();
        tx.rlp_encode_fields(&mut buf);
        let mut slice = buf.as_slice();
        let decoded = TxCip64::rlp_decode_fields(&mut slice).expect("decodes");
        assert_eq!(decoded.fee_currency, None);
        assert_eq!(decoded, tx);
    }

    /// Pins `Decodable::decode -> Ok(Default)` by encoding a populated tx
    /// via the `Encodable` impl and asserting the round-trip recovers it
    /// (not Default::default()).
    #[test]
    fn cip64_decodable_round_trip() {
        use alloy_rlp::{Decodable, Encodable};
        let tx = populated_tx();
        let mut buf = Vec::new();
        tx.encode(&mut buf);
        let decoded = TxCip64::decode(&mut buf.as_slice()).expect("decode");
        assert_eq!(decoded, tx);
    }

    /// Pins the `bytes.len() != 20` error branch in `rlp_decode_fields`'s
    /// fee_currency parsing (line 200-203). Encode a tx, then re-encode the
    /// fee_currency field as 19 bytes (invalid) and assert decode errors.
    #[test]
    fn cip64_rlp_decode_rejects_wrong_fee_currency_length() {
        use alloy_consensus::transaction::RlpEcdsaDecodableTx;
        use alloy_rlp::Encodable as _;
        let tx = populated_tx();
        let mut buf = Vec::new();
        // Encode all fields up to fee_currency (the last one), then write a
        // bogus 19-byte fee_currency.
        tx.chain_id.encode(&mut buf);
        tx.nonce.encode(&mut buf);
        tx.max_priority_fee_per_gas.encode(&mut buf);
        tx.max_fee_per_gas.encode(&mut buf);
        tx.gas_limit.encode(&mut buf);
        tx.to.encode(&mut buf);
        tx.value.encode(&mut buf);
        tx.input.0.encode(&mut buf);
        tx.access_list.encode(&mut buf);
        // 19-byte address (1 byte too short) — should error.
        let bogus: &[u8] = &[0xab; 19];
        bogus.encode(&mut buf);
        let mut slice = buf.as_slice();
        let result = TxCip64::rlp_decode_fields(&mut slice);
        assert!(result.is_err(), "decode must reject 19-byte fee_currency");
    }

    /// Suppresses unused-import warnings for items only used by
    /// k256-feature-gated tests above.
    #[allow(dead_code)]
    fn _unused() -> Address {
        Address::ZERO
    }

    /// Suppresses unused-import warning for B256 (only used by k256 tests).
    #[allow(dead_code)]
    fn _unused_b256() -> B256 {
        B256::ZERO
    }
}
