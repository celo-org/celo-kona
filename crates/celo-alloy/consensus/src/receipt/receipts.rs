//! Transaction receipt types for Celo.

use alloy_consensus::{
    Eip658Value, Receipt, ReceiptWithBloom, RlpDecodableReceipt, RlpEncodableReceipt, TxReceipt,
};
use alloy_primitives::{Bloom, Log};
use alloy_rlp::{Buf, BufMut, Decodable, Encodable, Header};

/// Receipt containing result of transaction execution.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct CeloCip64Receipt<T = Log> {
    /// The inner receipt type.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub inner: Receipt<T>,
    /// Base fee for Celo cip64 transactions
    #[cfg_attr(
        feature = "serde",
        serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "alloy_serde::quantity::opt"
        )
    )]
    pub base_fee: Option<u128>,
}

impl CeloCip64Receipt {
    /// Calculates [`Log`]'s bloom filter. this is slow operation and [CeloCip64ReceiptWithBloom]
    /// can be used to cache this value.
    pub fn bloom_slow(&self) -> Bloom {
        self.inner.logs.iter().collect()
    }

    /// Calculates the bloom filter for the receipt and returns the [CeloCip64ReceiptWithBloom]
    /// container type.
    pub fn with_bloom(self) -> CeloCip64ReceiptWithBloom {
        self.into()
    }
}

impl<T: Encodable> CeloCip64Receipt<T> {
    /// Returns length of RLP-encoded receipt fields with the given [`Bloom`] without an RLP header.
    pub fn rlp_encoded_fields_length_with_bloom(&self, bloom: &Bloom) -> usize {
        self.inner.rlp_encoded_fields_length_with_bloom(bloom)
            + self.base_fee.map_or(0, |base_fee| base_fee.length())
    }

    /// RLP-encodes receipt fields with the given [`Bloom`] without an RLP header.
    pub fn rlp_encode_fields_with_bloom(&self, bloom: &Bloom, out: &mut dyn BufMut) {
        self.inner.rlp_encode_fields_with_bloom(bloom, out);

        if let Some(base_fee) = self.base_fee {
            base_fee.encode(out);
        }
    }

    /// Returns RLP header for this receipt encoding with the given [`Bloom`].
    pub fn rlp_header_with_bloom(&self, bloom: &Bloom) -> Header {
        Header { list: true, payload_length: self.rlp_encoded_fields_length_with_bloom(bloom) }
    }
}

impl<T: Decodable> CeloCip64Receipt<T> {
    /// RLP-decodes receipt's field with a [`Bloom`].
    ///
    /// Does not expect an RLP header.
    pub fn rlp_decode_fields_with_bloom(
        buf: &mut &[u8],
    ) -> alloy_rlp::Result<ReceiptWithBloom<Self>> {
        let ReceiptWithBloom { receipt: inner, logs_bloom } =
            Receipt::rlp_decode_fields_with_bloom(buf)?;

        let base_fee = (!buf.is_empty()).then(|| Decodable::decode(buf)).transpose()?;

        Ok(ReceiptWithBloom { logs_bloom, receipt: Self { inner, base_fee } })
    }
}

impl<T> AsRef<Receipt<T>> for CeloCip64Receipt<T> {
    fn as_ref(&self) -> &Receipt<T> {
        &self.inner
    }
}

impl<T> TxReceipt for CeloCip64Receipt<T>
where
    T: AsRef<Log> + Clone + core::fmt::Debug + PartialEq + Eq + Send + Sync,
{
    type Log = T;

    fn status_or_post_state(&self) -> Eip658Value {
        self.inner.status_or_post_state()
    }

    fn status(&self) -> bool {
        self.inner.status()
    }

    fn bloom(&self) -> Bloom {
        self.inner.bloom_slow()
    }

    fn cumulative_gas_used(&self) -> u64 {
        self.inner.cumulative_gas_used()
    }

    fn logs(&self) -> &[Self::Log] {
        self.inner.logs()
    }
}

impl<T: Encodable> RlpEncodableReceipt for CeloCip64Receipt<T> {
    fn rlp_encoded_length_with_bloom(&self, bloom: &Bloom) -> usize {
        self.rlp_header_with_bloom(bloom).length_with_payload()
    }

    fn rlp_encode_with_bloom(&self, bloom: &Bloom, out: &mut dyn BufMut) {
        self.rlp_header_with_bloom(bloom).encode(out);
        self.rlp_encode_fields_with_bloom(bloom, out);
    }
}

impl<T: Decodable> RlpDecodableReceipt for CeloCip64Receipt<T> {
    fn rlp_decode_with_bloom(buf: &mut &[u8]) -> alloy_rlp::Result<ReceiptWithBloom<Self>> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }

        if buf.len() < header.payload_length {
            return Err(alloy_rlp::Error::InputTooShort);
        }

        // Note: we pass a separate buffer to `rlp_decode_fields_with_bloom` to allow it decode
        // optional fields based on the remaining length.
        let mut fields_buf = &buf[..header.payload_length];
        let this = Self::rlp_decode_fields_with_bloom(&mut fields_buf)?;

        if !fields_buf.is_empty() {
            return Err(alloy_rlp::Error::UnexpectedLength);
        }

        buf.advance(header.payload_length);

        Ok(this)
    }
}

/// [`CeloCip64Receipt`] with calculated bloom filter, modified for the Celo Stack.
///
/// This convenience type allows us to lazily calculate the bloom filter for a
/// receipt, similar to [`Sealed`].
///
/// [`Sealed`]: alloy_consensus::Sealed
pub type CeloCip64ReceiptWithBloom<T = Log> = ReceiptWithBloom<CeloCip64Receipt<T>>;

#[cfg(feature = "arbitrary")]
impl<'a, T> arbitrary::Arbitrary<'a> for CeloCip64Receipt<T>
where
    T: arbitrary::Arbitrary<'a>,
{
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        use std::vec::Vec;
        let base_fee = Option::<u128>::arbitrary(u)?;
        Ok(Self {
            inner: Receipt {
                status: Eip658Value::arbitrary(u)?,
                cumulative_gas_used: u64::arbitrary(u)?,
                logs: Vec::<T>::arbitrary(u)?,
            },
            base_fee,
        })
    }
}

/// Bincode-compatible [`CeloCip64Receipt`] serde implementation.
#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub(crate) mod serde_bincode_compat {
    use alloy_consensus::Receipt;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_with::{DeserializeAs, SerializeAs};
    use std::borrow::Cow;

    /// Bincode-compatible [`super::CeloCip64Receipt`] serde implementation.
    ///
    /// Intended to use with the [`serde_with::serde_as`] macro in the following way:
    /// ```rust
    /// use celo_alloy_consensus::{CeloCip64Receipt, serde_bincode_compat};
    /// use serde::{Deserialize, Serialize, de::DeserializeOwned};
    /// use serde_with::serde_as;
    ///
    /// #[serde_as]
    /// #[derive(Serialize, Deserialize)]
    /// struct Data<T: Serialize + DeserializeOwned + Clone + 'static> {
    ///     #[serde_as(as = "serde_bincode_compat::CeloCip64Receipt<'_, T>")]
    ///     receipt: CeloCip64Receipt<T>,
    /// }
    /// ```
    #[derive(Debug, Serialize, Deserialize)]
    pub struct CeloCip64Receipt<'a, T: Clone> {
        logs: Cow<'a, [T]>,
        status: bool,
        cumulative_gas_used: u64,
        base_fee: Option<u128>,
    }

    impl<'a, T: Clone> From<&'a super::CeloCip64Receipt<T>> for CeloCip64Receipt<'a, T> {
        fn from(value: &'a super::CeloCip64Receipt<T>) -> Self {
            Self {
                logs: Cow::Borrowed(&value.inner.logs),
                // Celo has no post state root variant
                status: value.inner.status.coerce_status(),
                cumulative_gas_used: value.inner.cumulative_gas_used,
                base_fee: value.base_fee,
            }
        }
    }

    impl<'a, T: Clone> From<CeloCip64Receipt<'a, T>> for super::CeloCip64Receipt<T> {
        fn from(value: CeloCip64Receipt<'a, T>) -> Self {
            Self {
                inner: Receipt {
                    status: value.status.into(),
                    cumulative_gas_used: value.cumulative_gas_used,
                    logs: value.logs.into_owned(),
                },
                base_fee: value.base_fee,
            }
        }
    }

    impl<T: Serialize + Clone> SerializeAs<super::CeloCip64Receipt<T>> for CeloCip64Receipt<'_, T> {
        fn serialize_as<S>(
            source: &super::CeloCip64Receipt<T>,
            serializer: S,
        ) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            CeloCip64Receipt::<'_, T>::from(source).serialize(serializer)
        }
    }

    impl<'de, T: Deserialize<'de> + Clone> DeserializeAs<'de, super::CeloCip64Receipt<T>>
        for CeloCip64Receipt<'de, T>
    {
        fn deserialize_as<D>(deserializer: D) -> Result<super::CeloCip64Receipt<T>, D::Error>
        where
            D: Deserializer<'de>,
        {
            CeloCip64Receipt::<'_, T>::deserialize(deserializer).map(Into::into)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::super::{CeloCip64Receipt, serde_bincode_compat};
        use alloy_primitives::Log;
        use arbitrary::Arbitrary;
        use rand::Rng;
        use serde::{Deserialize, Serialize, de::DeserializeOwned};
        use serde_with::serde_as;

        #[test]
        fn test_tx_cip64_bincode_roundtrip() {
            #[serde_as]
            #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
            struct Data<T: Serialize + DeserializeOwned + Clone + 'static> {
                #[serde_as(as = "serde_bincode_compat::CeloCip64Receipt<'_,T>")]
                transaction: CeloCip64Receipt<T>,
            }

            let mut bytes = [0u8; 1024];
            rand::rng().fill(bytes.as_mut_slice());
            let mut data = Data {
                transaction: CeloCip64Receipt::arbitrary(&mut arbitrary::Unstructured::new(&bytes))
                    .unwrap(),
            };
            // ensure we don't have an invalid poststate variant
            data.transaction.inner.status = data.transaction.inner.status.coerce_status().into();

            let encoded = bincode::serde::encode_to_vec(&data, bincode::config::legacy()).unwrap();
            let (decoded, _) = bincode::serde::decode_from_slice::<Data<Log>, _>(
                &encoded,
                bincode::config::legacy(),
            )
            .unwrap();
            assert_eq!(decoded, data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Receipt;
    use alloy_primitives::{Bytes, Log, LogData, address, b256, bytes, hex};
    use alloy_rlp::{Decodable, Encodable};

    #[cfg(not(feature = "std"))]
    use std::{vec, vec::Vec};

    // Test vector from: https://eips.ethereum.org/EIPS/eip-2481
    #[test]
    fn decode_cip64_receipt() {
        let data = hex!(
            "f9016a8001b9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000f85ff85d940000000000000000000000000000000000000011f842a0000000000000000000000000000000000000000000000000000000000000deada0000000000000000000000000000000000000000000000000000000000000beef830100ff8343d573"
        );

        // EIP658Receipt
        let expected = CeloCip64ReceiptWithBloom {
            receipt: CeloCip64Receipt {
                inner: Receipt {
                    status: false.into(),
                    cumulative_gas_used: 0x1,
                    logs: vec![Log {
                        address: address!("0000000000000000000000000000000000000011"),
                        data: LogData::new_unchecked(
                            vec![
                                b256!(
                                    "000000000000000000000000000000000000000000000000000000000000dead"
                                ),
                                b256!(
                                    "000000000000000000000000000000000000000000000000000000000000beef"
                                ),
                            ],
                            bytes!("0100ff"),
                        ),
                    }],
                },
                base_fee: Some(4445555),
            },
            logs_bloom: [0; 256].into(),
        };

        let receipt = CeloCip64ReceiptWithBloom::decode(&mut &data[..]).unwrap();
        assert_eq!(receipt, expected);
    }

    #[test]
    fn decode_cip64_receipt_with_empty_base_fee() {
        let data = hex!(
            "f901668001b9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000f85ff85d940000000000000000000000000000000000000011f842a0000000000000000000000000000000000000000000000000000000000000deada0000000000000000000000000000000000000000000000000000000000000beef830100ff"
        );

        // EIP658Receipt
        let expected = CeloCip64ReceiptWithBloom {
            receipt: CeloCip64Receipt {
                inner: Receipt {
                    status: false.into(),
                    cumulative_gas_used: 0x1,
                    logs: vec![Log {
                        address: address!("0000000000000000000000000000000000000011"),
                        data: LogData::new_unchecked(
                            vec![
                                b256!(
                                    "000000000000000000000000000000000000000000000000000000000000dead"
                                ),
                                b256!(
                                    "000000000000000000000000000000000000000000000000000000000000beef"
                                ),
                            ],
                            bytes!("0100ff"),
                        ),
                    }],
                },
                base_fee: None,
            },
            logs_bloom: [0; 256].into(),
        };

        let receipt = CeloCip64ReceiptWithBloom::decode(&mut &data[..]).unwrap();
        assert_eq!(receipt, expected);
    }

    #[test]
    fn gigantic_cip64_receipt() {
        let receipt = CeloCip64Receipt {
            inner: Receipt {
                cumulative_gas_used: 16747627,
                status: true.into(),
                logs: vec![
                    Log {
                        address: address!("4bf56695415f725e43c3e04354b604bcfb6dfb6e"),
                        data: LogData::new_unchecked(
                            vec![b256!(
                                "c69dc3d7ebff79e41f525be431d5cd3cc08f80eaf0f7819054a726eeb7086eb9"
                            )],
                            Bytes::from(vec![1; 0xffffff]),
                        ),
                    },
                    Log {
                        address: address!("faca325c86bf9c2d5b413cd7b90b209be92229c2"),
                        data: LogData::new_unchecked(
                            vec![b256!(
                                "8cca58667b1e9ffa004720ac99a3d61a138181963b294d270d91c53d36402ae2"
                            )],
                            Bytes::from(vec![1; 0xffffff]),
                        ),
                    },
                ],
            },
            base_fee: Some(4445555),
        }
        .with_bloom();

        let mut data = vec![];

        receipt.encode(&mut data);
        let decoded = CeloCip64ReceiptWithBloom::decode(&mut &data[..]).unwrap();

        assert_eq!(decoded, receipt);
    }

    #[test]
    fn receipt_cip64_roundtrip() {
        let data = hex!(
            "f9010c0182b741b9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c08343d573"
        );

        // CIP-64 Receipt
        let expected = CeloCip64ReceiptWithBloom {
            receipt: CeloCip64Receipt {
                inner: Receipt::<Log> {
                    cumulative_gas_used: 46913,
                    logs: vec![],
                    status: true.into(),
                },
                base_fee: Some(4445555),
            },
            logs_bloom: [0; 256].into(),
        };

        let receipt = CeloCip64ReceiptWithBloom::decode(&mut &data[..]).unwrap();
        assert_eq!(receipt, expected);

        let mut buf = Vec::new();
        receipt.encode(&mut buf);
        assert_eq!(buf, &data[..]);
    }
}
