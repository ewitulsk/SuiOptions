//! Layer 2 transfer payload — the bytes a Locker puts in
//! `CrossChainMessage.payload` (bridge-spec.md §3.2/§3.3, NTT-style).
//!
//! Fixed big-endian packed layout (72 bytes), byte-identical to the Move
//! `locker::transfer_payload` and Solidity `TransferPayload` implementations:
//!
//! ```text
//! asset_id   bytes32           32 bytes
//! amount     u64  big-endian    8 bytes   (in fixed WIRE_DECIMALS, see below)
//! recipient  bytes32           32 bytes
//! ```
//!
//! `amount` is a *wire amount* in a fixed decimal precision shared by both
//! chains (`WIRE_DECIMALS`), so an 18-dec ERC-20 and an N-dec `Coin<T>` agree on
//! the integer carried across. Each Locker scales between its local decimals and
//! the wire precision, rejecting any remainder (dust) — the NTT "trimmed amount"
//! approach. The codec itself is agnostic to decimals; it just carries the u64.

use crate::message::Bytes32;
use thiserror::Error;

/// Shared wire precision for cross-chain amounts (Locker scaling target).
pub const WIRE_DECIMALS: u8 = 8;

/// Encoded length: 32 + 8 + 32.
pub const ENCODED_LEN: usize = 72;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferPayload {
    pub asset_id: Bytes32,
    pub amount: u64,
    pub recipient: Bytes32,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TransferDecodeError {
    #[error("transfer payload must be {ENCODED_LEN} bytes, got {0}")]
    BadLength(usize),
}

impl TransferPayload {
    pub fn new(asset_id: Bytes32, amount: u64, recipient: Bytes32) -> Self {
        Self { asset_id, amount, recipient }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ENCODED_LEN);
        out.extend_from_slice(&self.asset_id);
        out.extend_from_slice(&self.amount.to_be_bytes());
        out.extend_from_slice(&self.recipient);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, TransferDecodeError> {
        if bytes.len() != ENCODED_LEN {
            return Err(TransferDecodeError::BadLength(bytes.len()));
        }
        let mut asset_id = [0u8; 32];
        asset_id.copy_from_slice(&bytes[0..32]);
        let mut amount_be = [0u8; 8];
        amount_be.copy_from_slice(&bytes[32..40]);
        let mut recipient = [0u8; 32];
        recipient.copy_from_slice(&bytes[40..72]);
        Ok(Self { asset_id, amount: u64::from_be_bytes(amount_be), recipient })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parity vector: this exact payload encodes identically in the Move and
    /// Solidity codecs.
    #[test]
    fn known_encoding_vector() {
        let p = TransferPayload::new([0x11; 32], 123_456_789, [0x22; 32]);
        let expected = format!("{}{}{}", "11".repeat(32), "00000000075bcd15", "22".repeat(32));
        assert_eq!(hex::encode(p.encode()), expected);
        assert_eq!(p.encode().len(), ENCODED_LEN);
    }

    #[test]
    fn round_trips() {
        let p = TransferPayload::new([0xab; 32], u64::MAX, [0xcd; 32]);
        assert_eq!(TransferPayload::decode(&p.encode()).unwrap(), p);
    }

    #[test]
    fn rejects_bad_length() {
        assert_eq!(TransferPayload::decode(&[0u8; 71]), Err(TransferDecodeError::BadLength(71)));
    }
}
