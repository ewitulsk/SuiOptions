//! Verify a Sui wallet `signPersonalMessage` signature and recover the signer
//! address.
//!
//! dapp-kit's `signPersonalMessage` returns `{ signature, bytes }` (both
//! base64). `bytes` is the raw message that was shown to the user; `signature`
//! is Sui's serialized signature: `flag(1) || sig(64) || pubkey(N)`.
//!
//! Sui signs `blake2b256( intent || bcs(PersonalMessage{ message }) )`, where
//! `intent = [3, 0, 0]` (scope=PersonalMessage, version=V0, app=Sui) and the
//! BCS of `PersonalMessage` is `uleb128(len) || message`. For Ed25519 the
//! scheme signs that 32-byte digest directly; for Secp256k1/r1 it signs
//! `sha256(digest)` — which is exactly what [`protocol_types::verify_signature`]
//! does internally, so we hand it the blake2b digest as the message.
//!
//! The signer address is recovered as `blake2b256( flag || pubkey )` — never
//! trusted from the client.

use anyhow::{anyhow, bail, Result};
use base64::Engine;
use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};

use protocol_types::{verify_signature, SigningScheme};

type Blake2b256 = Blake2b<U32>;

const INTENT_PERSONAL_MESSAGE: [u8; 3] = [3, 0, 0];

fn blake2b256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Blake2b256::new();
    for p in parts {
        h.update(p);
    }
    let out = h.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    arr
}

/// Minimal ULEB128 encoder for the BCS length prefix.
fn uleb128(mut n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (n & 0x7f) as u8;
        n >>= 7;
        if n != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if n == 0 {
            break;
        }
    }
    out
}

/// Verify `signature_b64` over `message` and return the `0x`-prefixed signer
/// address. Errors if the signature is malformed, uses an unsupported scheme,
/// or doesn't verify.
pub fn recover_and_verify(signature_b64: &str, message: &[u8]) -> Result<String> {
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(signature_b64.trim())
        .map_err(|e| anyhow!("signature is not valid base64: {e}"))?;

    let flag = *sig_bytes.first().ok_or_else(|| anyhow!("empty signature"))?;
    let scheme = SigningScheme::from_u8(flag).map_err(|_| {
        anyhow!("unsupported signature scheme flag {flag} (expected 0/1/2)")
    })?;

    let pk_len = scheme.pubkey_len();
    let want_len = 1 + 64 + pk_len;
    if sig_bytes.len() != want_len {
        bail!(
            "serialized signature length {} != expected {} for {}",
            sig_bytes.len(),
            want_len,
            scheme.as_str()
        );
    }
    let sig = &sig_bytes[1..65];
    let pubkey = &sig_bytes[65..65 + pk_len];

    // Recover the address from the embedded pubkey (never trust the client).
    let addr = blake2b256(&[&[flag], pubkey]);
    let address = format!("0x{}", hex::encode(addr));

    // Reconstruct the signed digest and verify.
    let len_prefix = uleb128(message.len() as u64);
    let digest = blake2b256(&[&INTENT_PERSONAL_MESSAGE, &len_prefix, message]);
    verify_signature(scheme, pubkey, &digest, sig)
        .map_err(|_| anyhow!("signature did not verify for {address}"))?;

    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    /// Build a Sui serialized Ed25519 personal-message signature the way a
    /// wallet would, then check we recover the address and verify.
    #[test]
    fn ed25519_personal_message_round_trip() {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key().to_bytes();
        let message = b"SuiOptions admin login \xe2\x80\x94 nonce: deadbeef";

        let len_prefix = uleb128(message.len() as u64);
        let digest = blake2b256(&[&INTENT_PERSONAL_MESSAGE, &len_prefix, message]);
        let sig = sk.sign(&digest).to_bytes();

        // flag(0) || sig(64) || pk(32)
        let mut serialized = Vec::with_capacity(97);
        serialized.push(0u8);
        serialized.extend_from_slice(&sig);
        serialized.extend_from_slice(&pk);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&serialized);

        let expected_addr = format!("0x{}", hex::encode(blake2b256(&[&[0u8], &pk])));
        let got = recover_and_verify(&b64, message).unwrap();
        assert_eq!(got, expected_addr);
    }

    #[test]
    fn rejects_tampered_message() {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key().to_bytes();
        let message = b"original";
        let len_prefix = uleb128(message.len() as u64);
        let digest = blake2b256(&[&INTENT_PERSONAL_MESSAGE, &len_prefix, message]);
        let sig = sk.sign(&digest).to_bytes();
        let mut serialized = vec![0u8];
        serialized.extend_from_slice(&sig);
        serialized.extend_from_slice(&pk);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&serialized);

        assert!(recover_and_verify(&b64, b"tampered").is_err());
    }
}
