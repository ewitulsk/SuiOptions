//! Reconstruct a canonical [`CrossChainMessage`] from an on-chain
//! `MessageCommitted` event's `parsed_json` (the Move `sui_bridge::events`
//! struct). The decoder re-derives the keccak digest and checks it against the
//! `message_hash` the event carried — a malformed or tampered event is rejected
//! before it ever reaches the signer.
//!
//! Sui renders a Move `vector<u8>` as a JSON array of numbers and a `u64` as a
//! string; both forms (plus `0x`-hex for bytes) are accepted for robustness.

use bridge_types::{Bytes32, CrossChainMessage};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EventDecodeError {
    #[error("missing field `{0}`")]
    MissingField(&'static str),
    #[error("field `{0}` has the wrong shape")]
    BadField(&'static str),
    #[error("field `{field}` must be {expected} bytes, got {got}")]
    BadLength { field: &'static str, expected: usize, got: usize },
    #[error("event message_hash does not match the recomputed digest")]
    HashMismatch,
}

pub fn decode_message_committed(
    parsed: &Value,
    domain_sep: &Bytes32,
) -> Result<CrossChainMessage, EventDecodeError> {
    let src_chain_id = u32_field(parsed, "src_chain_id")?;
    let dst_chain_id = u32_field(parsed, "dst_chain_id")?;
    let nonce = u64_field(parsed, "nonce")?;
    let src_app = bytes32_field(parsed, "src_app")?;
    let dst_app = bytes32_field(parsed, "dst_app")?;
    let payload = bytes_field(parsed, "payload")?;
    let claimed_hash = bytes32_field(parsed, "message_hash")?;

    let message =
        CrossChainMessage::new(src_chain_id, dst_chain_id, nonce, src_app, dst_app, payload);

    // The event is untrusted plumbing — verify its hash matches the canonical
    // (domain-separated) digest of the fields we just reconstructed.
    if message.digest(domain_sep) != claimed_hash {
        return Err(EventDecodeError::HashMismatch);
    }
    Ok(message)
}

fn field<'a>(v: &'a Value, name: &'static str) -> Result<&'a Value, EventDecodeError> {
    v.get(name).ok_or(EventDecodeError::MissingField(name))
}

fn u32_field(v: &Value, name: &'static str) -> Result<u32, EventDecodeError> {
    let f = field(v, name)?;
    let n = match f {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse::<u64>().ok(),
        _ => None,
    }
    .ok_or(EventDecodeError::BadField(name))?;
    u32::try_from(n).map_err(|_| EventDecodeError::BadField(name))
}

fn u64_field(v: &Value, name: &'static str) -> Result<u64, EventDecodeError> {
    let f = field(v, name)?;
    match f {
        Value::Number(n) => n.as_u64().ok_or(EventDecodeError::BadField(name)),
        Value::String(s) => s.parse::<u64>().map_err(|_| EventDecodeError::BadField(name)),
        _ => Err(EventDecodeError::BadField(name)),
    }
}

fn bytes_field(v: &Value, name: &'static str) -> Result<Vec<u8>, EventDecodeError> {
    match field(v, name)? {
        Value::Array(items) => items
            .iter()
            .map(|x| {
                x.as_u64()
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or(EventDecodeError::BadField(name))
            })
            .collect(),
        Value::String(s) => {
            hex::decode(s.trim_start_matches("0x")).map_err(|_| EventDecodeError::BadField(name))
        }
        _ => Err(EventDecodeError::BadField(name)),
    }
}

fn bytes32_field(v: &Value, name: &'static str) -> Result<Bytes32, EventDecodeError> {
    let bytes = bytes_field(v, name)?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| EventDecodeError::BadLength { field: name, expected: 32, got: 0 })
        .map(|b: &[u8; 32]| *b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bridge_types::chain_id;
    use bridge_types::message::derive_domain_sep;
    use serde_json::json;

    const TEST_SALT: Bytes32 = [0x01; 32];

    fn test_domain_sep() -> Bytes32 {
        derive_domain_sep(&TEST_SALT)
    }

    /// Domain-separated digest of the known vector under TEST_SALT (matches the
    /// bridge-types and cross-language parity vectors).
    fn known_hash_hex() -> &'static str {
        "535392536947463d04988702a5480f431f34efed3cf557dc12aa434c2decd707"
    }

    /// Array-of-numbers form (how Sui actually renders `vector<u8>`), with the
    /// `u64` nonce as a string. Reconstructs the known-vector message.
    #[test]
    fn decodes_array_form() {
        let hash: Vec<u8> = hex::decode(known_hash_hex()).unwrap();
        let parsed = json!({
            "outbox_id": "0xabc",
            "message_hash": hash,
            "src_chain_id": 268_436_454u64,
            "dst_chain_id": 134_217_728u64,
            "nonce": "7",
            "src_app": vec![0xabu8; 32],
            "dst_app": vec![0xcdu8; 32],
            "payload": b"hello-bridge".to_vec(),
        });
        let m = decode_message_committed(&parsed, &test_domain_sep()).unwrap();
        assert_eq!(m.src_chain_id, chain_id::encode(chain_id::FAMILY_EVM, 998).unwrap());
        assert_eq!(m.dst_chain_id, chain_id::encode(chain_id::FAMILY_SUI, 0).unwrap());
        assert_eq!(m.nonce, 7);
        assert_eq!(m.payload, b"hello-bridge");
        assert_eq!(hex::encode(m.digest(&test_domain_sep())), known_hash_hex());
    }

    /// `0x`-hex form for byte fields is also accepted.
    #[test]
    fn decodes_hex_string_form() {
        let parsed = json!({
            "message_hash": format!("0x{}", known_hash_hex()),
            "src_chain_id": 268_436_454u64,
            "dst_chain_id": 134_217_728u64,
            "nonce": "7",
            "src_app": format!("0x{}", "ab".repeat(32)),
            "dst_app": format!("0x{}", "cd".repeat(32)),
            "payload": "0x68656c6c6f2d627269646765",
        });
        let m = decode_message_committed(&parsed, &test_domain_sep()).unwrap();
        assert_eq!(hex::encode(m.digest(&test_domain_sep())), known_hash_hex());
    }

    /// A message_hash that doesn't match the reconstructed fields is rejected.
    #[test]
    fn rejects_hash_mismatch() {
        let parsed = json!({
            "message_hash": vec![0u8; 32],
            "src_chain_id": 268_436_454u64,
            "dst_chain_id": 134_217_728u64,
            "nonce": "7",
            "src_app": vec![0xabu8; 32],
            "dst_app": vec![0xcdu8; 32],
            "payload": b"hello-bridge".to_vec(),
        });
        assert_eq!(
            decode_message_committed(&parsed, &test_domain_sep()),
            Err(EventDecodeError::HashMismatch)
        );
    }
}
