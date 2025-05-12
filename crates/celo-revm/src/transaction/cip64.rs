use crate::CeloTxType;
use alloy_consensus::transaction::{RlpEcdsaDecodableTx, RlpEcdsaEncodableTx};
use alloy_consensus::{SignableTransaction, Transaction};
use alloy_eips::{Typed2718, eip2930::AccessList, eip7702::SignedAuthorization};
use alloy_primitives::{Address, B256, Bytes, ChainId, Signature, TxKind, U256};
use alloy_rlp::{BufMut, Decodable, Encodable};
use core::mem;

/// A transaction with a fee currency ([CIP-64](https://github.com/celo-org/celo-proposals/blob/master/CIPs/cip-0064.md)).
///
/// This transaction type is identical to EIP-1559 but with a different type identifier (0x7b)
/// and an additional `fee_currency` field to specify the currency to pay for gas.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
#[doc(
    alias = "Cip64Transaction",
    alias = "TransactionCip64",
    alias = "Cip64Tx"
)]
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
    /// Input has two uses depending if `to` field is Create or Call.
    /// pub init: An unlimited size byte array specifying the
    /// EVM-code for the account initialisation procedure CREATE,
    /// data: An unlimited size byte array specifying the
    /// input data of the message call, formally Td.
    pub input: Bytes,
    /// The address of the whitelisted currency to be used to pay for gas.
    /// This is a Celo-specific field not present in Ethereum transactions.
    // pub fee_currency: Option<eddress>, // TODO: enable optional
    pub fee_currency: Address,
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
        mem::size_of::<ChainId>() + // chain_id
        mem::size_of::<u64>() + // nonce
        mem::size_of::<u64>() + // gas_limit
        mem::size_of::<u128>() + // max_fee_per_gas
        mem::size_of::<u128>() + // max_priority_fee_per_gas
        self.to.size() + // to
        mem::size_of::<U256>() + // value
        self.access_list.size() + // access_list
        self.input.len() + // input
        // self.fee_currency.as_ref().map_or(0, |_| mem::size_of::<Address>()) // fee_currency
        mem::size_of::<Address>()
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
            + self.fee_currency.length()
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
        self.fee_currency.encode(out);
    }
}

impl RlpEcdsaDecodableTx for TxCip64 {
    // TODO: Preferable get the value from `tx_type`.
    // const DEFAULT_TX_TYPE: u8 = { Self::tx_type() as u8 };
    const DEFAULT_TX_TYPE: u8 = 123;

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
            fee_currency: Decodable::decode(buf)?,
        })
    }
}

impl Transaction for TxCip64 {
    // /// Get the fee currency for this transaction
    // #[inline]
    // fn fee_currency(&self) -> Option<&Address> {
    //     self.fee_currency.as_ref()
    // }

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
        CeloTxType::Cip64.into()
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
    /// use celo_revm::{serde_bincode_compat, TxCip64};
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
        // fee_currency: Option<Address>,
        fee_currency: Address,
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
            rand::thread_rng().fill(bytes.as_mut_slice());
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
            // fee_currency: Some(address!("0x2f25deb3848c207fc8e0c34035b3ba7fc157602b")),
            fee_currency: address!("0x2f25deb3848c207fc8e0c34035b3ba7fc157602b"),
        };

        let sig = Signature::from_scalars_and_parity(
            b256!("0xaa0cfaa3df893578b3504062b862428f0e4a94046370cf2a4fd6c392c0760dd8"),
            b256!("0x1337d022bbb8faed78a9707e6c38d51f575816e7f85aa540f3d37a9081c58a71"),
            false,
        );

        let signed_tx = tx.into_signed(sig);
        assert_eq!(*signed_tx.hash(), hash, "Expected same hash");
        assert_eq!(
            signed_tx.recover_signer().unwrap(),
            signer,
            "Recovering signer should pass."
        );
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
            // fee_currency: Some(address!("0x2f25deb3848c207fc8e0c34035b3ba7fc157602b")),
            fee_currency: address!("0x2f25deb3848c207fc8e0c34035b3ba7fc157602b"),
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
}
