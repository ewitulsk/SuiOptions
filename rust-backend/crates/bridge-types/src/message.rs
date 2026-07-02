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
//! `digest = keccak256(DOMAIN_SEP || encode(message))`, where
//! `DOMAIN_SEP = keccak256(DOMAIN_TAG || deployment_salt)` binds every signed
//! message to one logical deployment (bridge-spec.md §2.2). Without it, a
//! redeploy that reuses the registry ids (fresh `consumed` set) would let every
//! previously signed message replay. Signers sign over the 32-byte digest
//! directly (Ed25519 over the digest on Sui; ECDSA/ecrecover over the digest on
//! EVM) — no chain-specific prefix.

use serde::{Deserialize, Serialize};
use tiny_keccak::{Hasher, Keccak};

/// Current canonical format version (start at 1).
pub const VERSION: u8 = 1;

/// Domain-separation tag hashed with the per-deployment salt to form
/// `DOMAIN_SEP`. Bump the version suffix only on a breaking digest change.
pub const DOMAIN_TAG: &[u8] = b"XCHAIN_MSG_V1";

/// `DOMAIN_SEP = keccak256(DOMAIN_TAG || deployment_salt)`. Derived once per
/// deployment; both chains' contracts store the derived value and the services
/// derive it identically from the salt in config. Contracts derive it on-chain
/// at construction so the stored separator is auditable.
pub fn derive_domain_sep(deployment_salt: &Bytes32) -> Bytes32 {
    let mut hasher = Keccak::v256();
    let mut out = [0u8; 32];
    hasher.update(DOMAIN_TAG);
    hasher.update(deployment_salt);
    hasher.finalize(&mut out);
    out
}

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

    /// `keccak256(domain_sep || encode())` — the 32-byte digest signers sign
    /// over. `domain_sep` is [`derive_domain_sep`] of the deployment salt.
    pub fn digest(&self, domain_sep: &Bytes32) -> Bytes32 {
        let mut hasher = Keccak::v256();
        let mut out = [0u8; 32];
        hasher.update(domain_sep);
        hasher.update(&self.encode());
        hasher.finalize(&mut out);
        out
    }

    /// Standard **BCS** serialization matching `sui_bridge::message::from_bcs` —
    /// the bytes a relayer passes as a plain `vector<u8>` arg to the Sui
    /// `bridge_receive` (relayer-dispatch-design §3.1). Field order: version,
    /// src_chain_id, dst_chain_id, nonce, src_app, dst_app, payload. This is
    /// distinct from [`encode`](Self::encode) (the big-endian keccak preimage);
    /// BCS is little-endian with ULEB128 length prefixes.
    pub fn to_move_bcs(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(self.version);
        out.extend_from_slice(&self.src_chain_id.to_le_bytes());
        out.extend_from_slice(&self.dst_chain_id.to_le_bytes());
        out.extend_from_slice(&self.nonce.to_le_bytes());
        push_bcs_bytes(&mut out, &self.src_app);
        push_bcs_bytes(&mut out, &self.dst_app);
        push_bcs_bytes(&mut out, &self.payload);
        out
    }
}

/// Append a BCS ULEB128 length prefix.
pub(crate) fn push_uleb128(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
}

/// Append a BCS `vector<u8>`: ULEB128 length then the raw bytes.
fn push_bcs_bytes(out: &mut Vec<u8>, b: &[u8]) {
    push_uleb128(out, b.len() as u64);
    out.extend_from_slice(b);
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

    /// Fixed dummy salt for cross-language test vectors (NOT a deployment salt).
    /// Mirrored in the Move and Solidity parity tests.
    pub const TEST_SALT: Bytes32 = [0x01; 32];

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

    /// Three-way parity lock: this exact message + TEST_SALT hashes to the same
    /// digest in `sui_bridge::message_tests::known_digest_vector` (Move) and
    /// `MessageTest.test_known_digest_matches_sui` (Solidity). Regenerate all
    /// three together via `cargo run -p bridge-signer --example group_keys`.
    #[test]
    fn known_digest_vector() {
        let expected = "535392536947463d04988702a5480f431f34efed3cf557dc12aa434c2decd707";
        let ds = derive_domain_sep(&TEST_SALT);
        assert_eq!(hex::encode(vector_message().digest(&ds)), expected);
    }

    #[test]
    fn encode_length_is_fixed_header_plus_payload() {
        let m = vector_message();
        assert_eq!(m.encode().len(), 1 + 4 + 4 + 8 + 32 + 32 + 4 + m.payload.len());
    }

    #[test]
    fn digest_is_field_sensitive() {
        let ds = derive_domain_sep(&TEST_SALT);
        let a = vector_message();
        let mut b = a.clone();
        b.nonce = 8;
        assert_ne!(a.digest(&ds), b.digest(&ds));
    }

    #[test]
    fn digest_is_domain_separated() {
        let a = derive_domain_sep(&[0x01; 32]);
        let b = derive_domain_sep(&[0x02; 32]);
        let m = vector_message();
        assert_ne!(m.digest(&a), m.digest(&b));
    }

    /// BCS parity: matches the bytes `sui_bridge::message::from_bcs` decodes in
    /// `message_tests::from_bcs_decodes_to_known_digest`.
    #[test]
    fn to_move_bcs_known_vector() {
        let expected = "01e603001000000008070000000000000020\
            abababababababababababababababababababababababababababababababab20\
            cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd0c\
            68656c6c6f2d627269646765";
        assert_eq!(hex::encode(vector_message().to_move_bcs()), expected);
    }

    #[test]
    fn uleb128_multibyte() {
        let mut out = vec![];
        push_uleb128(&mut out, 300); // 0xAC 0x02
        assert_eq!(out, vec![0xac, 0x02]);
    }

    #[test]
    fn json_round_trips() {
        let m = vector_message();
        let json = serde_json::to_string(&m).unwrap();
        let back: CrossChainMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }
}
