//! Signature scheme tag.
//!
//! On chain, every `Account` carries `signing_scheme: u8` alongside its
//! `signing_pubkey: vector<u8>`. The numeric values must match
//! `quote.move`'s constants and the contract's dispatch table:
//!
//! ```text
//! 0 = Ed25519             — pubkey 32 B, sig 64 B, verifier hashes msg
//!                           internally (SHA-512).
//! 1 = Secp256k1 (sha256)  — compressed pubkey 33 B, sig 64 B (r||s).
//! 2 = Secp256r1 (sha256)  — compressed pubkey 33 B, sig 64 B (r||s).
//! ```
//!
//! For Secp256k1 / Secp256r1 the off-chain signer must hash the BCS-encoded
//! quote payload with SHA-256 before signing, then ship the raw 64-byte
//! ECDSA signature. The contract calls `secp256k1_verify(sig, pk, msg, 1)`
//! where `1` selects SHA-256.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::errors::ProtocolError;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[repr(u8)]
pub enum SigningScheme {
    Ed25519 = 0,
    Secp256k1 = 1,
    Secp256r1 = 2,
}

impl SigningScheme {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn from_u8(byte: u8) -> Result<Self, ProtocolError> {
        match byte {
            0 => Ok(Self::Ed25519),
            1 => Ok(Self::Secp256k1),
            2 => Ok(Self::Secp256r1),
            _ => Err(ProtocolError::InvalidSigningScheme),
        }
    }

    /// Expected pubkey length in bytes.
    pub fn pubkey_len(self) -> usize {
        match self {
            Self::Ed25519 => 32,
            Self::Secp256k1 | Self::Secp256r1 => 33,
        }
    }

    /// Expected signature length in bytes (Sui's verifiers all take
    /// 64-byte compact ECDSA / Ed25519 signatures).
    pub fn signature_len(self) -> usize {
        64
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ed25519 => "ed25519",
            Self::Secp256k1 => "secp256k1",
            Self::Secp256r1 => "secp256r1",
        }
    }
}

impl std::str::FromStr for SigningScheme {
    type Err = ProtocolError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "ed25519" | "0" => Ok(Self::Ed25519),
            "secp256k1" | "1" => Ok(Self::Secp256k1),
            "secp256r1" | "2" => Ok(Self::Secp256r1),
            _ => Err(ProtocolError::InvalidSigningScheme),
        }
    }
}

// JSON: snake-case string. BCS: a single u8 byte (matches the Move struct
// field's wire encoding so any indexer reading the chain event can deser
// straight into the off-chain mirror).
impl Serialize for SigningScheme {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        if ser.is_human_readable() {
            ser.serialize_str(self.as_str())
        } else {
            ser.serialize_u8(self.as_u8())
        }
    }
}

impl<'de> Deserialize<'de> for SigningScheme {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        if de.is_human_readable() {
            let s = String::deserialize(de)?;
            s.parse::<Self>().map_err(serde::de::Error::custom)
        } else {
            let b = u8::deserialize(de)?;
            Self::from_u8(b).map_err(serde::de::Error::custom)
        }
    }
}

/// Verify a signature according to `scheme`, matching the on-chain dispatch
/// in `quote.move`'s `verify_and_consume_quote`. Returns `Ok(())` on
/// success, `Err(ProtocolError::QuoteSignatureInvalid)` on any failure.
///
/// For k1/r1, the verifier expects the *unhashed* message (we hash with
/// SHA-256 internally, mirroring the contract's `hash = 1` flag).
pub fn verify_signature(
    scheme: SigningScheme,
    pubkey: &[u8],
    msg: &[u8],
    signature: &[u8],
) -> Result<(), ProtocolError> {
    if pubkey.len() != scheme.pubkey_len() || signature.len() != scheme.signature_len() {
        return Err(ProtocolError::QuoteSignatureInvalid);
    }
    match scheme {
        SigningScheme::Ed25519 => verify_ed25519(pubkey, msg, signature),
        SigningScheme::Secp256k1 => verify_secp256k1_sha256(pubkey, msg, signature),
        SigningScheme::Secp256r1 => verify_secp256r1_sha256(pubkey, msg, signature),
    }
}

fn verify_ed25519(pubkey: &[u8], msg: &[u8], signature: &[u8]) -> Result<(), ProtocolError> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    let mut pk = [0u8; 32];
    pk.copy_from_slice(pubkey);
    let vk = VerifyingKey::from_bytes(&pk).map_err(|_| ProtocolError::QuoteSignatureInvalid)?;
    let sig = Signature::from_slice(signature).map_err(|_| ProtocolError::QuoteSignatureInvalid)?;
    vk.verify(msg, &sig)
        .map_err(|_| ProtocolError::QuoteSignatureInvalid)
}

fn verify_secp256k1_sha256(
    pubkey: &[u8],
    msg: &[u8],
    signature: &[u8],
) -> Result<(), ProtocolError> {
    use k256::ecdsa::signature::hazmat::PrehashVerifier;
    use k256::ecdsa::{Signature, VerifyingKey};
    let vk = VerifyingKey::from_sec1_bytes(pubkey)
        .map_err(|_| ProtocolError::QuoteSignatureInvalid)?;
    let sig = Signature::from_slice(signature)
        .map_err(|_| ProtocolError::QuoteSignatureInvalid)?;
    let digest = Sha256::digest(msg);
    vk.verify_prehash(&digest, &sig)
        .map_err(|_| ProtocolError::QuoteSignatureInvalid)
}

fn verify_secp256r1_sha256(
    pubkey: &[u8],
    msg: &[u8],
    signature: &[u8],
) -> Result<(), ProtocolError> {
    use p256::ecdsa::signature::hazmat::PrehashVerifier;
    use p256::ecdsa::{Signature, VerifyingKey};
    let vk = VerifyingKey::from_sec1_bytes(pubkey)
        .map_err(|_| ProtocolError::QuoteSignatureInvalid)?;
    let sig = Signature::from_slice(signature)
        .map_err(|_| ProtocolError::QuoteSignatureInvalid)?;
    let digest = Sha256::digest(msg);
    vk.verify_prehash(&digest, &sig)
        .map_err(|_| ProtocolError::QuoteSignatureInvalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer as EdSigner;
    use rand::rngs::OsRng;

    #[test]
    fn bcs_is_one_byte() {
        let bytes = bcs::to_bytes(&SigningScheme::Ed25519).unwrap();
        assert_eq!(bytes, vec![0]);
        let bytes = bcs::to_bytes(&SigningScheme::Secp256k1).unwrap();
        assert_eq!(bytes, vec![1]);
        let bytes = bcs::to_bytes(&SigningScheme::Secp256r1).unwrap();
        assert_eq!(bytes, vec![2]);
    }

    #[test]
    fn json_is_snake_case_string() {
        assert_eq!(
            serde_json::to_string(&SigningScheme::Ed25519).unwrap(),
            "\"ed25519\""
        );
        let back: SigningScheme = serde_json::from_str("\"secp256k1\"").unwrap();
        assert_eq!(back, SigningScheme::Secp256k1);
    }

    #[test]
    fn rejects_invalid_byte() {
        assert!(SigningScheme::from_u8(9).is_err());
        // BCS decode of garbage should reject too.
        assert!(bcs::from_bytes::<SigningScheme>(&[7]).is_err());
    }

    #[test]
    fn ed25519_sign_then_verify_round_trip() {
        let sk = ed25519_dalek::SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key().to_bytes().to_vec();
        let msg = b"hello-bcs-quote";
        let sig = sk.sign(msg).to_bytes().to_vec();
        verify_signature(SigningScheme::Ed25519, &pk, msg, &sig).unwrap();

        // Tamper the message — must reject.
        assert!(verify_signature(SigningScheme::Ed25519, &pk, b"hello-bcs-quotE", &sig).is_err());
    }

    #[test]
    fn secp256k1_sign_then_verify_round_trip() {
        use k256::ecdsa::signature::hazmat::PrehashSigner;
        use k256::ecdsa::{Signature, SigningKey};
        let sk = SigningKey::random(&mut OsRng);
        // Compressed SEC1 — Sui's `ecdsa_k1::secp256k1_verify` expects 33 bytes.
        let pk = sk.verifying_key().to_encoded_point(true).as_bytes().to_vec();
        assert_eq!(pk.len(), 33);
        let msg = b"hello-bcs-quote";
        let digest = Sha256::digest(msg);
        let (sig, _): (Signature, _) = sk.sign_prehash(&digest).unwrap();
        let sig_bytes = sig.to_bytes().to_vec();
        verify_signature(SigningScheme::Secp256k1, &pk, msg, &sig_bytes).unwrap();

        // Wrong scheme should reject even with valid k1 inputs.
        assert!(verify_signature(SigningScheme::Secp256r1, &pk, msg, &sig_bytes).is_err());
    }

    #[test]
    fn secp256r1_sign_then_verify_round_trip() {
        use p256::ecdsa::signature::hazmat::PrehashSigner;
        use p256::ecdsa::{Signature, SigningKey};
        let sk = SigningKey::random(&mut OsRng);
        let pk = sk.verifying_key().to_encoded_point(true).as_bytes().to_vec();
        assert_eq!(pk.len(), 33);
        let msg = b"hello-bcs-quote";
        let digest = Sha256::digest(msg);
        let (sig, _): (Signature, _) = sk.sign_prehash(&digest).unwrap();
        let sig_bytes = sig.to_bytes().to_vec();
        verify_signature(SigningScheme::Secp256r1, &pk, msg, &sig_bytes).unwrap();
    }

    #[test]
    fn pubkey_length_must_match_scheme() {
        // 32 bytes claimed as k1 → too short, must reject.
        let pk = vec![0u8; 32];
        let sig = vec![0u8; 64];
        assert!(verify_signature(SigningScheme::Secp256k1, &pk, b"x", &sig).is_err());
    }
}
