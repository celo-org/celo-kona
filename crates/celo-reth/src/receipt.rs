//! Celo receipt type for execution and storage (bloomless).

use alloc::vec::Vec;
use alloy_consensus::{
    Eip658Value, Eip2718DecodableReceipt, Eip2718EncodableReceipt, Receipt, ReceiptWithBloom,
    RlpDecodableReceipt, RlpEncodableReceipt, TxReceipt, Typed2718,
};
use alloy_eips::eip2718::{Eip2718Error, Eip2718Result, IsTyped2718};
use alloy_primitives::{Bloom, Log};
use alloy_rlp::{Buf, BufMut, Decodable, Encodable, Header};
use celo_alloy_consensus::{CeloCip64Receipt, CeloTxType};
use op_alloy_consensus::OpDepositReceipt;
use reth_primitives_traits::InMemorySize;

/// Typed Celo transaction receipt (bloomless, for reth storage).
///
/// Receipt containing result of transaction execution, analogous to
/// [`OpReceipt`](op_alloy_consensus::OpReceipt) but with an additional `Cip64` variant.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum CeloReceipt<T = Log> {
    /// Legacy receipt
    #[serde(rename = "0x0", alias = "0x00")]
    Legacy(Receipt<T>),
    /// EIP-2930 receipt
    #[serde(rename = "0x1", alias = "0x01")]
    Eip2930(Receipt<T>),
    /// EIP-1559 receipt
    #[serde(rename = "0x2", alias = "0x02")]
    Eip1559(Receipt<T>),
    /// EIP-7702 receipt
    #[serde(rename = "0x4", alias = "0x04")]
    Eip7702(Receipt<T>),
    /// CIP-64 receipt (fee currency)
    #[serde(rename = "0x7b", alias = "0x7B")]
    Cip64(CeloCip64Receipt<T>),
    /// Deposit receipt
    #[serde(rename = "0x7e", alias = "0x7E")]
    Deposit(OpDepositReceipt<T>),
}

impl<T> CeloReceipt<T> {
    /// Returns [`CeloTxType`] of the receipt.
    pub const fn tx_type(&self) -> CeloTxType {
        match self {
            Self::Legacy(_) => CeloTxType::Legacy,
            Self::Eip2930(_) => CeloTxType::Eip2930,
            Self::Eip1559(_) => CeloTxType::Eip1559,
            Self::Eip7702(_) => CeloTxType::Eip7702,
            Self::Cip64(_) => CeloTxType::Cip64,
            Self::Deposit(_) => CeloTxType::Deposit,
        }
    }

    /// Returns inner [`Receipt`].
    pub const fn as_receipt(&self) -> &Receipt<T> {
        match self {
            Self::Legacy(receipt) |
            Self::Eip2930(receipt) |
            Self::Eip1559(receipt) |
            Self::Eip7702(receipt) => receipt,
            Self::Cip64(receipt) => &receipt.inner,
            Self::Deposit(receipt) => &receipt.inner,
        }
    }

    /// Returns a mutable reference to the inner [`Receipt`].
    pub const fn as_receipt_mut(&mut self) -> &mut Receipt<T> {
        match self {
            Self::Legacy(receipt) |
            Self::Eip2930(receipt) |
            Self::Eip1559(receipt) |
            Self::Eip7702(receipt) => receipt,
            Self::Cip64(receipt) => &mut receipt.inner,
            Self::Deposit(receipt) => &mut receipt.inner,
        }
    }

    /// Returns length of RLP-encoded receipt fields with the given [`Bloom`] without an RLP header.
    pub fn rlp_encoded_fields_length(&self, bloom: &Bloom) -> usize
    where
        T: Encodable,
    {
        match self {
            Self::Legacy(receipt) |
            Self::Eip2930(receipt) |
            Self::Eip1559(receipt) |
            Self::Eip7702(receipt) => receipt.rlp_encoded_fields_length_with_bloom(bloom),
            Self::Cip64(receipt) => receipt.rlp_encoded_fields_length_with_bloom(bloom),
            Self::Deposit(receipt) => receipt.rlp_encoded_fields_length_with_bloom(bloom),
        }
    }

    /// RLP-encodes receipt fields with the given [`Bloom`] without an RLP header.
    pub fn rlp_encode_fields(&self, bloom: &Bloom, out: &mut dyn BufMut)
    where
        T: Encodable,
    {
        match self {
            Self::Legacy(receipt) |
            Self::Eip2930(receipt) |
            Self::Eip1559(receipt) |
            Self::Eip7702(receipt) => receipt.rlp_encode_fields_with_bloom(bloom, out),
            Self::Cip64(receipt) => receipt.rlp_encode_fields_with_bloom(bloom, out),
            Self::Deposit(receipt) => receipt.rlp_encode_fields_with_bloom(bloom, out),
        }
    }

    /// Returns RLP header for inner encoding.
    pub fn rlp_header_inner(&self, bloom: &Bloom) -> Header
    where
        T: Encodable,
    {
        Header { list: true, payload_length: self.rlp_encoded_fields_length(bloom) }
    }

    /// Returns RLP header for inner encoding without bloom.
    pub fn rlp_header_without_bloom(&self) -> Header
    where
        T: Encodable,
    {
        Header { list: true, payload_length: self.rlp_encoded_fields_length_without_bloom() }
    }

    /// RLP-decodes the receipt from the provided buffer. This does not expect a type byte or
    /// network header.
    pub fn rlp_decode_inner(
        buf: &mut &[u8],
        tx_type: CeloTxType,
    ) -> alloy_rlp::Result<ReceiptWithBloom<Self>>
    where
        T: Decodable,
    {
        match tx_type {
            CeloTxType::Legacy |
            CeloTxType::Eip2930 |
            CeloTxType::Eip1559 |
            CeloTxType::Eip7702 => {
                let ReceiptWithBloom { receipt, logs_bloom } =
                    RlpDecodableReceipt::rlp_decode_with_bloom(buf)?;
                let receipt = match tx_type {
                    CeloTxType::Legacy => Self::Legacy(receipt),
                    CeloTxType::Eip2930 => Self::Eip2930(receipt),
                    CeloTxType::Eip1559 => Self::Eip1559(receipt),
                    CeloTxType::Eip7702 => Self::Eip7702(receipt),
                    _ => unreachable!(),
                };
                Ok(ReceiptWithBloom { receipt, logs_bloom })
            }
            CeloTxType::Cip64 => {
                let ReceiptWithBloom { receipt, logs_bloom } =
                    RlpDecodableReceipt::rlp_decode_with_bloom(buf)?;
                Ok(ReceiptWithBloom { receipt: Self::Cip64(receipt), logs_bloom })
            }
            CeloTxType::Deposit => {
                let ReceiptWithBloom { receipt, logs_bloom } =
                    RlpDecodableReceipt::rlp_decode_with_bloom(buf)?;
                Ok(ReceiptWithBloom { receipt: Self::Deposit(receipt), logs_bloom })
            }
        }
    }

    /// RLP-encodes receipt fields without an RLP header.
    pub fn rlp_encode_fields_without_bloom(&self, out: &mut dyn BufMut)
    where
        T: Encodable,
    {
        Into::<u8>::into(self.tx_type()).encode(out);
        let inner = self.as_receipt();
        inner.status.encode(out);
        inner.cumulative_gas_used.encode(out);
        inner.logs.encode(out);
        // Encode variant-specific extra fields.
        match self {
            Self::Cip64(receipt) => {
                if let Some(base_fee) = receipt.base_fee {
                    base_fee.encode(out);
                }
            }
            Self::Deposit(receipt) => {
                if let Some(nonce) = receipt.deposit_nonce {
                    nonce.encode(out);
                }
                if let Some(version) = receipt.deposit_receipt_version {
                    version.encode(out);
                }
            }
            _ => {}
        }
    }

    /// Returns length of RLP-encoded receipt fields without an RLP header.
    pub fn rlp_encoded_fields_length_without_bloom(&self) -> usize
    where
        T: Encodable,
    {
        let inner = self.as_receipt();
        Into::<u8>::into(self.tx_type()).length() +
            inner.status.length() +
            inner.cumulative_gas_used.length() +
            inner.logs.length() +
            match self {
                Self::Cip64(receipt) => receipt.base_fee.map_or(0, |base_fee| base_fee.length()),
                Self::Deposit(receipt) => {
                    receipt.deposit_nonce.map_or(0, |nonce| nonce.length()) +
                        receipt.deposit_receipt_version.map_or(0, |version| version.length())
                }
                _ => 0,
            }
    }

    /// RLP-decodes the receipt from the provided buffer without bloom.
    pub fn rlp_decode_fields_without_bloom(buf: &mut &[u8]) -> alloy_rlp::Result<Self>
    where
        T: Decodable,
    {
        let ty_byte = u8::decode(buf)?;
        let tx_type = CeloTxType::try_from(ty_byte)
            .map_err(|_| alloy_rlp::Error::Custom("invalid celo tx type"))?;
        let status = Decodable::decode(buf)?;
        let cumulative_gas_used = Decodable::decode(buf)?;
        let logs = Decodable::decode(buf)?;

        match tx_type {
            CeloTxType::Legacy |
            CeloTxType::Eip2930 |
            CeloTxType::Eip1559 |
            CeloTxType::Eip7702 => {
                let receipt = Receipt { status, cumulative_gas_used, logs };
                Ok(match tx_type {
                    CeloTxType::Legacy => Self::Legacy(receipt),
                    CeloTxType::Eip2930 => Self::Eip2930(receipt),
                    CeloTxType::Eip1559 => Self::Eip1559(receipt),
                    CeloTxType::Eip7702 => Self::Eip7702(receipt),
                    _ => unreachable!(),
                })
            }
            CeloTxType::Cip64 => {
                let base_fee = (!buf.is_empty()).then(|| Decodable::decode(buf)).transpose()?;
                Ok(Self::Cip64(CeloCip64Receipt {
                    inner: Receipt { status, cumulative_gas_used, logs },
                    base_fee,
                }))
            }
            CeloTxType::Deposit => {
                let mut deposit_nonce = None;
                let mut deposit_receipt_version = None;
                if !buf.is_empty() {
                    deposit_nonce = Some(Decodable::decode(buf)?);
                    if !buf.is_empty() {
                        deposit_receipt_version = Some(Decodable::decode(buf)?);
                    }
                }
                Ok(Self::Deposit(OpDepositReceipt {
                    inner: Receipt { status, cumulative_gas_used, logs },
                    deposit_nonce,
                    deposit_receipt_version,
                }))
            }
        }
    }
}

impl<T: Encodable> Eip2718EncodableReceipt for CeloReceipt<T> {
    fn eip2718_encoded_length_with_bloom(&self, bloom: &Bloom) -> usize {
        !matches!(self, Self::Legacy(_)) as usize +
            self.rlp_header_inner(bloom).length_with_payload()
    }

    fn eip2718_encode_with_bloom(&self, bloom: &Bloom, out: &mut dyn BufMut) {
        if !matches!(self, Self::Legacy(_)) {
            out.put_u8(self.tx_type().into());
        }
        self.rlp_header_inner(bloom).encode(out);
        self.rlp_encode_fields(bloom, out);
    }
}

impl<T: Decodable> Eip2718DecodableReceipt for CeloReceipt<T> {
    fn typed_decode_with_bloom(ty: u8, buf: &mut &[u8]) -> Eip2718Result<ReceiptWithBloom<Self>> {
        let tx_type = CeloTxType::try_from(ty).map_err(|_| Eip2718Error::UnexpectedType(ty))?;
        Ok(Self::rlp_decode_inner(buf, tx_type)?)
    }

    fn fallback_decode_with_bloom(buf: &mut &[u8]) -> Eip2718Result<ReceiptWithBloom<Self>> {
        Ok(Self::rlp_decode_inner(buf, CeloTxType::Legacy)?)
    }
}

impl<T: Encodable> RlpEncodableReceipt for CeloReceipt<T> {
    fn rlp_encoded_length_with_bloom(&self, bloom: &Bloom) -> usize {
        let mut len = self.eip2718_encoded_length_with_bloom(bloom);
        if !matches!(self, Self::Legacy(_)) {
            len += Header {
                list: false,
                payload_length: self.eip2718_encoded_length_with_bloom(bloom),
            }
            .length();
        }

        len
    }

    fn rlp_encode_with_bloom(&self, bloom: &Bloom, out: &mut dyn BufMut) {
        if !matches!(self, Self::Legacy(_)) {
            Header { list: false, payload_length: self.eip2718_encoded_length_with_bloom(bloom) }
                .encode(out);
        }
        self.eip2718_encode_with_bloom(bloom, out);
    }
}

impl<T: Decodable> RlpDecodableReceipt for CeloReceipt<T> {
    fn rlp_decode_with_bloom(buf: &mut &[u8]) -> alloy_rlp::Result<ReceiptWithBloom<Self>> {
        let header_buf = &mut &**buf;
        let header = Header::decode(header_buf)?;

        // Legacy receipt, reuse initial buffer without advancing
        if header.list {
            return Self::rlp_decode_inner(buf, CeloTxType::Legacy);
        }

        // Otherwise, advance the buffer and try decoding type flag followed by receipt
        *buf = *header_buf;

        let remaining = buf.len();
        let ty_byte = u8::decode(buf)?;
        let tx_type = CeloTxType::try_from(ty_byte)
            .map_err(|_| alloy_rlp::Error::Custom("invalid celo tx type"))?;
        let this = Self::rlp_decode_inner(buf, tx_type)?;

        if buf.len() + header.payload_length != remaining {
            return Err(alloy_rlp::Error::UnexpectedLength);
        }

        Ok(this)
    }
}

impl<T: Encodable + Send + Sync> Encodable for CeloReceipt<T> {
    fn encode(&self, out: &mut dyn BufMut) {
        self.rlp_header_without_bloom().encode(out);
        self.rlp_encode_fields_without_bloom(out);
    }

    fn length(&self) -> usize {
        self.rlp_header_without_bloom().length_with_payload()
    }
}

impl<T: Decodable> Decodable for CeloReceipt<T> {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }

        if buf.len() < header.payload_length {
            return Err(alloy_rlp::Error::InputTooShort);
        }
        let mut fields_buf = &buf[..header.payload_length];
        let this = Self::rlp_decode_fields_without_bloom(&mut fields_buf)?;

        if !fields_buf.is_empty() {
            return Err(alloy_rlp::Error::UnexpectedLength);
        }

        buf.advance(header.payload_length);

        Ok(this)
    }
}

impl<T: Send + Sync + Clone + core::fmt::Debug + Eq + AsRef<Log>> TxReceipt for CeloReceipt<T> {
    type Log = T;

    fn status_or_post_state(&self) -> Eip658Value {
        self.as_receipt().status_or_post_state()
    }

    fn status(&self) -> bool {
        self.as_receipt().status()
    }

    fn bloom(&self) -> Bloom {
        self.as_receipt().bloom()
    }

    fn cumulative_gas_used(&self) -> u64 {
        self.as_receipt().cumulative_gas_used()
    }

    fn logs(&self) -> &[Self::Log] {
        self.as_receipt().logs()
    }

    fn into_logs(self) -> Vec<Self::Log> {
        match self {
            Self::Legacy(receipt) |
            Self::Eip2930(receipt) |
            Self::Eip1559(receipt) |
            Self::Eip7702(receipt) => receipt.logs,
            Self::Cip64(receipt) => receipt.inner.logs,
            Self::Deposit(receipt) => receipt.inner.logs,
        }
    }
}

impl<T> Typed2718 for CeloReceipt<T> {
    fn ty(&self) -> u8 {
        self.tx_type().into()
    }
}

impl<T> IsTyped2718 for CeloReceipt<T> {
    fn is_type(type_id: u8) -> bool {
        <CeloTxType as IsTyped2718>::is_type(type_id)
    }
}

impl InMemorySize for CeloReceipt {
    fn size(&self) -> usize {
        match self {
            Self::Legacy(receipt) |
            Self::Eip2930(receipt) |
            Self::Eip1559(receipt) |
            Self::Eip7702(receipt) => receipt.size(),
            Self::Cip64(receipt) => receipt.inner.size() + core::mem::size_of::<Option<u128>>(),
            Self::Deposit(receipt) => receipt.size(),
        }
    }
}

use celo_alloy_consensus::CeloReceiptEnvelope;

impl From<CeloReceiptEnvelope> for CeloReceipt {
    fn from(envelope: CeloReceiptEnvelope) -> Self {
        match envelope {
            CeloReceiptEnvelope::Legacy(receipt) => Self::Legacy(receipt.receipt),
            CeloReceiptEnvelope::Eip2930(receipt) => Self::Eip2930(receipt.receipt),
            CeloReceiptEnvelope::Eip1559(receipt) => Self::Eip1559(receipt.receipt),
            CeloReceiptEnvelope::Eip7702(receipt) => Self::Eip7702(receipt.receipt),
            CeloReceiptEnvelope::Cip64(receipt) => Self::Cip64(receipt.receipt),
            CeloReceiptEnvelope::Deposit(receipt) => Self::Deposit(OpDepositReceipt {
                deposit_nonce: receipt.receipt.deposit_nonce,
                deposit_receipt_version: receipt.receipt.deposit_receipt_version,
                inner: receipt.receipt.inner,
            }),
        }
    }
}

use reth_db_api::table::{Compress, Decompress};
use reth_primitives_traits::serde_bincode_compat::RlpBincode;

/// CeloReceipt already implements `alloy_rlp::Encodable` and `alloy_rlp::Decodable`.
/// `RlpBincode` gives a blanket `SerdeBincodeCompat` impl.
impl RlpBincode for CeloReceipt {}

impl Compress for CeloReceipt {
    type Compressed = Vec<u8>;

    fn compress_to_buf<B: bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        let _ = reth_codecs::Compact::to_compact(self, buf);
    }
}

impl Decompress for CeloReceipt {
    fn decompress(value: &[u8]) -> Result<Self, reth_db_api::DatabaseError> {
        let (obj, _) = reth_codecs::Compact::from_compact(value, value.len());
        Ok(obj)
    }
}

impl reth_codecs::Compact for CeloReceipt {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        let start = buf.as_mut().len();
        alloy_rlp::Encodable::encode(self, buf);
        buf.as_mut().len() - start
    }

    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        let mut data = buf;
        let receipt = <Self as alloy_rlp::Decodable>::decode(&mut data)
            .expect("Failed to decode CeloReceipt from compact");
        (receipt, data)
    }
}

use reth_optimism_primitives::DepositReceipt;

impl DepositReceipt for CeloReceipt {
    fn as_deposit_receipt_mut(&mut self) -> Option<&mut OpDepositReceipt> {
        match self {
            Self::Deposit(receipt) => Some(receipt),
            _ => None,
        }
    }

    fn as_deposit_receipt(&self) -> Option<&OpDepositReceipt> {
        match self {
            Self::Deposit(receipt) => Some(receipt),
            _ => None,
        }
    }
}
