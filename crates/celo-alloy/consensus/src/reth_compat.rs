//! Reth compatibility implementations for Celo consensus types.
//!
//! Provides [`InMemorySize`], [`SignedTransaction`], [`Compact`], and
//! [`SerdeBincodeCompat`] trait implementations required by reth's node framework.

use crate::transaction::{
    CeloTxEnvelope, CeloTxType, CeloTypedTransaction, cip64::TxCip64,
    pooled::CeloPooledTransaction,
};
use alloy_consensus::{Sealed, crypto::RecoveryError, transaction::{SignerRecoverable, TxHashRef}};
use alloy_primitives::B256;
use op_alloy_consensus::{OpTransaction, TxDeposit};
use reth_primitives_traits::{InMemorySize, SignedTransaction};

impl InMemorySize for TxCip64 {
    #[inline]
    fn size(&self) -> usize {
        TxCip64::size(self)
    }
}

impl InMemorySize for CeloTxEnvelope {
    fn size(&self) -> usize {
        match self {
            Self::Legacy(tx) => tx.size(),
            Self::Eip2930(tx) => tx.size(),
            Self::Eip1559(tx) => tx.size(),
            Self::Eip7702(tx) => tx.size(),
            Self::Cip64(tx) => tx.size(),
            Self::Deposit(tx) => tx.size(),
        }
    }
}

impl InMemorySize for CeloTypedTransaction {
    fn size(&self) -> usize {
        match self {
            Self::Legacy(tx) => tx.size(),
            Self::Eip2930(tx) => tx.size(),
            Self::Eip1559(tx) => tx.size(),
            Self::Eip7702(tx) => tx.size(),
            Self::Cip64(tx) => TxCip64::size(tx),
            Self::Deposit(tx) => tx.size(),
        }
    }
}

impl OpTransaction for CeloTxEnvelope {
    fn is_deposit(&self) -> bool {
        matches!(self, Self::Deposit(_))
    }

    fn as_deposit(&self) -> Option<&Sealed<TxDeposit>> {
        match self {
            Self::Deposit(tx) => Some(tx),
            _ => None,
        }
    }
}

impl TxHashRef for CeloTxEnvelope {
    fn tx_hash(&self) -> &B256 {
        CeloTxEnvelope::hash(self)
    }
}

impl SignerRecoverable for CeloTxEnvelope {
    fn recover_signer(&self) -> Result<alloy_primitives::Address, RecoveryError> {
        let signature_hash = match self {
            Self::Legacy(tx) => tx.signature_hash(),
            Self::Eip2930(tx) => tx.signature_hash(),
            Self::Eip1559(tx) => tx.signature_hash(),
            Self::Eip7702(tx) => tx.signature_hash(),
            Self::Cip64(tx) => tx.signature_hash(),
            Self::Deposit(tx) => return Ok(tx.from),
        };
        let signature = match self {
            Self::Legacy(tx) => tx.signature(),
            Self::Eip2930(tx) => tx.signature(),
            Self::Eip1559(tx) => tx.signature(),
            Self::Eip7702(tx) => tx.signature(),
            Self::Cip64(tx) => tx.signature(),
            Self::Deposit(_) => unreachable!(),
        };
        alloy_consensus::crypto::secp256k1::recover_signer(signature, signature_hash)
    }

    fn recover_signer_unchecked(&self) -> Result<alloy_primitives::Address, RecoveryError> {
        let signature_hash = match self {
            Self::Legacy(tx) => tx.signature_hash(),
            Self::Eip2930(tx) => tx.signature_hash(),
            Self::Eip1559(tx) => tx.signature_hash(),
            Self::Eip7702(tx) => tx.signature_hash(),
            Self::Cip64(tx) => tx.signature_hash(),
            Self::Deposit(tx) => return Ok(tx.from),
        };
        let signature = match self {
            Self::Legacy(tx) => tx.signature(),
            Self::Eip2930(tx) => tx.signature(),
            Self::Eip1559(tx) => tx.signature(),
            Self::Eip7702(tx) => tx.signature(),
            Self::Cip64(tx) => tx.signature(),
            Self::Deposit(_) => unreachable!(),
        };
        alloy_consensus::crypto::secp256k1::recover_signer_unchecked(signature, signature_hash)
    }
}

impl SignedTransaction for CeloTxEnvelope {}

// ---------------------------------------------------------------------------
// Pool-related: CeloPooledTransaction reth trait impls
// ---------------------------------------------------------------------------

impl InMemorySize for CeloPooledTransaction {
    fn size(&self) -> usize {
        match self {
            Self::Legacy(tx) => tx.size(),
            Self::Eip2930(tx) => tx.size(),
            Self::Eip1559(tx) => tx.size(),
            Self::Eip7702(tx) => tx.size(),
            Self::Cip64(tx) => tx.size(),
        }
    }
}

impl SignedTransaction for CeloPooledTransaction {}

// ---------------------------------------------------------------------------
// reth Compact encoding: CeloTxType, CeloTxEnvelope
// ---------------------------------------------------------------------------

use alloy_consensus::{Signed, TxEip1559, TxEip2930, TxEip7702, TxLegacy};
use alloy_primitives::{Signature, U256};
use alloy_rlp::BufMut;
use reth_codecs::{
    Compact,
    alloy::transaction::{CompactEnvelope, Envelope, FromTxCompact, ToTxCompact},
};

const COMPACT_IDENTIFIER_LEGACY: usize = 0;
const COMPACT_IDENTIFIER_EIP2930: usize = 1;
const COMPACT_IDENTIFIER_EIP1559: usize = 2;
const COMPACT_EXTENDED_IDENTIFIER_FLAG: usize = 3;

impl Compact for CeloTxType {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        match self {
            Self::Legacy => COMPACT_IDENTIFIER_LEGACY,
            Self::Eip2930 => COMPACT_IDENTIFIER_EIP2930,
            Self::Eip1559 => COMPACT_IDENTIFIER_EIP1559,
            Self::Eip7702 | Self::Cip64 | Self::Deposit => {
                buf.put_u8(*self as u8);
                COMPACT_EXTENDED_IDENTIFIER_FLAG
            }
        }
    }

    fn from_compact(mut buf: &[u8], identifier: usize) -> (Self, &[u8]) {
        use alloy_rlp::bytes::Buf;
        (
            match identifier {
                COMPACT_IDENTIFIER_LEGACY => Self::Legacy,
                COMPACT_IDENTIFIER_EIP2930 => Self::Eip2930,
                COMPACT_IDENTIFIER_EIP1559 => Self::Eip1559,
                COMPACT_EXTENDED_IDENTIFIER_FLAG => {
                    let extended = buf.get_u8();
                    match extended {
                        4 => Self::Eip7702,
                        123 => Self::Cip64,
                        126 => Self::Deposit,
                        _ => panic!("Unsupported CeloTxType identifier: {extended}"),
                    }
                }
                _ => panic!("Unknown compact identifier for CeloTxType: {identifier}"),
            },
            buf,
        )
    }
}

impl ToTxCompact for CeloTxEnvelope {
    fn to_tx_compact(&self, buf: &mut (impl BufMut + AsMut<[u8]>)) {
        match self {
            Self::Legacy(tx) => tx.tx().to_compact(buf),
            Self::Eip2930(tx) => tx.tx().to_compact(buf),
            Self::Eip1559(tx) => tx.tx().to_compact(buf),
            Self::Eip7702(tx) => tx.tx().to_compact(buf),
            Self::Cip64(tx) => tx.tx().to_compact(buf),
            Self::Deposit(tx) => tx.to_compact(buf),
        };
    }
}

impl FromTxCompact for CeloTxEnvelope {
    type TxType = CeloTxType;

    fn from_tx_compact(buf: &[u8], tx_type: CeloTxType, signature: Signature) -> (Self, &[u8]) {
        match tx_type {
            CeloTxType::Legacy => {
                let (tx, buf) = TxLegacy::from_compact(buf, buf.len());
                (Self::Legacy(Signed::new_unhashed(tx, signature)), buf)
            }
            CeloTxType::Eip2930 => {
                let (tx, buf) = TxEip2930::from_compact(buf, buf.len());
                (Self::Eip2930(Signed::new_unhashed(tx, signature)), buf)
            }
            CeloTxType::Eip1559 => {
                let (tx, buf) = TxEip1559::from_compact(buf, buf.len());
                (Self::Eip1559(Signed::new_unhashed(tx, signature)), buf)
            }
            CeloTxType::Eip7702 => {
                let (tx, buf) = TxEip7702::from_compact(buf, buf.len());
                (Self::Eip7702(Signed::new_unhashed(tx, signature)), buf)
            }
            CeloTxType::Cip64 => {
                let (tx, buf) = TxCip64::from_compact(buf, buf.len());
                (Self::Cip64(Signed::new_unhashed(tx, signature)), buf)
            }
            CeloTxType::Deposit => {
                let (tx, buf) = TxDeposit::from_compact(buf, buf.len());
                (Self::Deposit(Sealed::new(tx)), buf)
            }
        }
    }
}

const DEPOSIT_SIGNATURE: Signature = Signature::new(U256::ZERO, U256::ZERO, false);

impl Envelope for CeloTxEnvelope {
    fn signature(&self) -> &Signature {
        match self {
            Self::Legacy(tx) => tx.signature(),
            Self::Eip2930(tx) => tx.signature(),
            Self::Eip1559(tx) => tx.signature(),
            Self::Eip7702(tx) => tx.signature(),
            Self::Cip64(tx) => tx.signature(),
            Self::Deposit(_) => &DEPOSIT_SIGNATURE,
        }
    }

    fn tx_type(&self) -> Self::TxType {
        CeloTxEnvelope::tx_type(self)
    }
}

impl Compact for CeloTxEnvelope {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        CompactEnvelope::to_compact(self, buf)
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        CompactEnvelope::from_compact(buf, len)
    }
}

// ---------------------------------------------------------------------------
// SerdeBincodeCompat via RlpBincode marker
// ---------------------------------------------------------------------------

use reth_primitives_traits::serde_bincode_compat::RlpBincode;

impl RlpBincode for CeloTxEnvelope {}

// ---------------------------------------------------------------------------
// Database compression: CeloTxEnvelope Compress + Decompress
// ---------------------------------------------------------------------------

use reth_db_api::table::{Compress, Decompress};

impl Compress for CeloTxEnvelope {
    type Compressed = Vec<u8>;

    fn compress_to_buf<B: bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        let _ = Compact::to_compact(self, buf);
    }
}

impl Decompress for CeloTxEnvelope {
    fn decompress(value: &[u8]) -> Result<Self, reth_db_api::DatabaseError> {
        let (obj, _) = Compact::from_compact(value, value.len());
        Ok(obj)
    }
}
