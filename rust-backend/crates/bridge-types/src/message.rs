//! Canonical cross-chain message + keccak256 digest. The encoding MUST be
//! byte-identical to `sui_bridge::message` (Move) and `Message.sol` (Solidity)
//! so one threshold signature verifies on every chain (bridge-spec.md §2.2).
//!
//! Fixed big-endian packed layout:
//!
//! ```text
//! version (u8) | src_chain_id (u32) | dst_chain_id (u32) | nonce (u64)
//!   | src_app (32) | dst_app (32) | payload_len (u32) | payload
//! ```
//!
//! `digest = keccak256(encode(message))`. Signers sign over the 32-byte digest
//! directly (Ed25519 over the digest on Sui; ECDSA/ecrecover over the digest on
//! EVM) — no chain-specific prefix.

use serde::{Deserialize, Serialize};
use tiny_keccak::{Hasher, Keccak};

/// Current canonical format version (start at 1).
pub const VERSION: u8 = 1;

/// 32-byte app address (Sui object/package id, or a left-padded EVM address).
pub type Bytes32 = [u8; 32];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossChainMessage {
    pub version: u8,
    pub src_chain_id: u32,
    pub dst_chain_id: u32,
    pub nonce: u64,
    #[serde(with = "hex_bytes32")]
    pub src_app: Bytes32,
    #[serde(with = "hex_bytes32")]
    pub dst_app: Bytes32,
    #[serde(with = "hex_vec")]
    pub payload: Vec<u8>,
}

impl CrossChainMessage {
    /// Construct a message at the current format version.
    pub fn new(
        src_chain_id: u32,
        dst_chain_id: u32,
        nonce: u64,
        src_app: Bytes32,
        dst_app: Bytes32,
        payload: Vec<u8>,
    ) -> Self {
        Self { version: VERSION, src_chain_id, dst_chain_id, nonce, src_app, dst_app, payload }
    }

    /// Canonical big-endian packed serialization (see module docs).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + 4 + 4 + 8 + 32 + 32 + 4 + self.payload.len());
        out.push(self.version);
        out.extend_from_slice(&self.src_chain_id.to_be_bytes());
        out.extend_from_slice(&self.dst_chain_id.to_be_bytes());
        out.extend_from_slice(&self.nonce.to_be_bytes());
        out.extend_from_slice(&self.src_app);
        out.extend_from_slice(&self.dst_app);
        out.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// `keccak256(encode())` — the 32-byte digest signers sign over.
    pub fn digest(&self) -> Bytes32 {
        keccak256(&self.encode())
    }
}

/// Left-pad a 20-byte EVM address into a 32-byte app identity (spec §2.2).
pub fn left_pad_address(addr: [u8; 20]) -> Bytes32 {
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(&addr);
    out
}

pub fn keccak256(data: &[u8]) -> Bytes32 {
    let mut hasher = Keccak::v256();
    let mut out = [0u8; 32];
    hasher.update(data);
    hasher.finalize(&mut out);
    out
}

// --- hex (de)serialization for JSON transport ---

pub(crate) mod hex_bytes32 {
    use super::Bytes32;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(b: &Bytes32, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("0x{}", hex::encode(b)))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Bytes32, D::Error> {
        let s = String::deserialize(d)?;
        let bytes = hex::decode(s.trim_start_matches("0x")).map_err(serde::de::Error::custom)?;
        bytes.try_into().map_err(|_| serde::de::Error::custom("expected 32 bytes"))
    }
}

pub(crate) mod hex_vec {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(b: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("0x{}", hex::encode(b)))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        hex::decode(s.trim_start_matches("0x")).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain_id;

    fn vector_message() -> CrossChainMessage {
        CrossChainMessage::new(
            chain_id::encode(chain_id::FAMILY_EVM, 998).unwrap(), // 268436454
            chain_id::encode(chain_id::FAMILY_SUI, 0).unwrap(),   // 134217728
            7,
            [0xab; 32],
            [0xcd; 32],
            b"hello-bridge".to_vec(),
        )
    }

    /// Three-way parity lock: this exact message hashes to the same digest in
    /// `sui_bridge::message_tests::known_digest_vector` (Move) and
    /// `MessageTest.test_known_digest_matches_sui` (Solidity).
    #[test]
    fn known_digest_vector() {
        let expected = "7b767c416104fbef99880be0416fa07353493afb6547ad67d700029ce09572af";
        assert_eq!(hex::encode(vector_message().digest()), expected);
    }

    #[test]
    fn encode_length_is_fixed_header_plus_payload() {
        let m = vector_message();
        assert_eq!(m.encode().len(), 1 + 4 + 4 + 8 + 32 + 32 + 4 + m.payload.len());
    }

    #[test]
    fn digest_is_field_sensitive() {
        let a = vector_message();
        let mut b = a.clone();
        b.nonce = 8;
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn json_round_trips() {
        let m = vector_message();
        let json = serde_json::to_string(&m).unwrap();
        let back: CrossChainMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }
}
