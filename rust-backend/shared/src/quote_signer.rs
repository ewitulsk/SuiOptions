//! Scheme-aware quote signing for the MM bot.
//!
//! All three schemes start from the same 32-byte secret. For Ed25519 that's
//! the raw seed; for Secp256k1 / Secp256r1 it's a scalar (interpreted big-
//! endian via the curve's `from_bytes`).
//!
//! For k1/r1, the contract calls `secp256k1_verify(sig, pk, msg, hash=1)`
//! where `1` selects SHA-256 — so we hash the BCS payload here and sign
//! the digest.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use ed25519_dalek::Signer as EdSigner;
use sha2::{Digest, Sha256};
use sui_types::crypto::{EncodeDecodeBase64, SuiKeyPair};

use crate::SigningScheme;

pub enum QuoteSigner {
    Ed25519(ed25519_dalek::SigningKey),
    Secp256k1(k256::ecdsa::SigningKey),
    Secp256r1(p256::ecdsa::SigningKey),
}

impl QuoteSigner {
    /// Build a signer from a 32-byte secret + scheme. Errors if the secret
    /// is the wrong length or the scalar is out of range for the curve.
    pub fn from_secret(scheme: SigningScheme, secret: &[u8]) -> Result<Self> {
        if secret.len() != 32 {
            return Err(anyhow!(
                "quote secret must be 32 bytes (got {})",
                secret.len()
            ));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(secret);
        Ok(match scheme {
            SigningScheme::Ed25519 => {
                Self::Ed25519(ed25519_dalek::SigningKey::from_bytes(&arr))
            }
            SigningScheme::Secp256k1 => Self::Secp256k1(
                k256::ecdsa::SigningKey::from_slice(&arr)
                    .context("k256 scalar out of range")?,
            ),
            SigningScheme::Secp256r1 => Self::Secp256r1(
                p256::ecdsa::SigningKey::from_slice(&arr)
                    .context("p256 scalar out of range")?,
            ),
        })
    }

    /// Parse a quote secret from either of two formats:
    ///
    /// - **Sui bech32 keypair (`suiprivkey1…`)** — the format `sui keytool
    ///   export` produces. The scheme is encoded in the key's flag byte and
    ///   must equal `expected_scheme`, otherwise we fail closed (the user
    ///   set `signing_scheme` in mm-bot.toml inconsistently with the key).
    /// - **Raw 32-byte secret in hex** (`0x…` prefix optional). Interpreted
    ///   per `expected_scheme`.
    pub fn from_secret_str(s: &str, expected_scheme: SigningScheme) -> Result<Self> {
        let s = s.trim();
        if s.starts_with("suiprivkey1") {
            let kp = SuiKeyPair::decode(s)
                .map_err(|e| anyhow!("decoding suiprivkey bech32 key: {e}"))?;
            // `encode_base64()` round-trip yields `base64(flag || 32-byte
            // secret)` for every variant — extracting the bytes from
            // there avoids poking each per-variant `Ed25519KeyPair` etc.
            // directly (those impls live in `fastcrypto` and aren't a
            // direct dep of this crate).
            let b64 = kp.encode_base64();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&b64)
                .context("base64-decoding suiprivkey re-encoding")?;
            if bytes.len() != 33 {
                return Err(anyhow!(
                    "suiprivkey decoded to {} bytes, expected 33 (1 flag + 32 secret)",
                    bytes.len()
                ));
            }
            let actual = SigningScheme::from_u8(bytes[0])?;
            if actual != expected_scheme {
                return Err(anyhow!(
                    "secrets.toml mm_bot.quote_key is a {} bech32 key, but \
                     mm-bot.toml signing_scheme = {}",
                    actual.as_str(),
                    expected_scheme.as_str()
                ));
            }
            Self::from_secret(expected_scheme, &bytes[1..])
        } else {
            let stripped = s.strip_prefix("0x").unwrap_or(s);
            let bytes = hex::decode(stripped).context("decoding hex quote secret")?;
            Self::from_secret(expected_scheme, &bytes)
        }
    }

    pub fn scheme(&self) -> SigningScheme {
        match self {
            Self::Ed25519(_) => SigningScheme::Ed25519,
            Self::Secp256k1(_) => SigningScheme::Secp256k1,
            Self::Secp256r1(_) => SigningScheme::Secp256r1,
        }
    }

    /// Compressed SEC1 / raw Ed25519 verifying key bytes — exactly what
    /// the contract stores as `Account.signing_pubkey`.
    pub fn public_bytes(&self) -> Vec<u8> {
        match self {
            Self::Ed25519(sk) => sk.verifying_key().to_bytes().to_vec(),
            Self::Secp256k1(sk) => sk
                .verifying_key()
                .to_encoded_point(true)
                .as_bytes()
                .to_vec(),
            Self::Secp256r1(sk) => sk
                .verifying_key()
                .to_encoded_point(true)
                .as_bytes()
                .to_vec(),
        }
    }

    /// Sign `msg`. For Ed25519 the bytes are signed raw; for the ECDSA
    /// schemes we SHA-256 the input first so the on-chain
    /// `secp256(k1|r1)_verify(.., hash=1)` accepts it.
    pub fn sign(&self, msg: &[u8]) -> Result<Vec<u8>> {
        Ok(match self {
            Self::Ed25519(sk) => sk.sign(msg).to_bytes().to_vec(),
            Self::Secp256k1(sk) => {
                use k256::ecdsa::signature::hazmat::PrehashSigner;
                use k256::ecdsa::Signature;
                let digest = Sha256::digest(msg);
                let (sig, _): (Signature, _) =
                    sk.sign_prehash(&digest).context("k256 sign_prehash")?;
                sig.to_bytes().to_vec()
            }
            Self::Secp256r1(sk) => {
                use p256::ecdsa::signature::hazmat::PrehashSigner;
                use p256::ecdsa::Signature;
                let digest = Sha256::digest(msg);
                let (sig, _): (Signature, _) =
                    sk.sign_prehash(&digest).context("p256 sign_prehash")?;
                sig.to_bytes().to_vec()
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_scheme_round_trips_with_protocol_verify() {
        let secret = [7u8; 32];
        for scheme in [
            SigningScheme::Ed25519,
            SigningScheme::Secp256k1,
            SigningScheme::Secp256r1,
        ] {
            let signer = QuoteSigner::from_secret(scheme, &secret).unwrap();
            let msg = b"some bcs-encoded quote payload";
            let sig = signer.sign(msg).unwrap();
            crate::verify_signature(scheme, &signer.public_bytes(), msg, &sig)
                .unwrap_or_else(|e| panic!("verify failed for {scheme:?}: {e:?}"));
        }
    }

    #[test]
    fn wrong_secret_length_errors() {
        assert!(QuoteSigner::from_secret(SigningScheme::Ed25519, &[0u8; 31]).is_err());
    }

    /// Build a `suiprivkey1…` bech32 string for a given scheme + secret
    /// without depending on `fastcrypto`'s internals: encode the flag-
    /// prefixed bytes as base64, hand them to `SuiKeyPair::decode_base64`,
    /// and ask the keypair to bech32-encode itself.
    fn build_suiprivkey(flag: u8, secret: &[u8; 32]) -> String {
        let mut bytes = vec![flag];
        bytes.extend_from_slice(secret);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let kp = SuiKeyPair::decode_base64(&b64).expect("base64 round-trip");
        kp.encode().expect("bech32 encode")
    }

    #[test]
    fn from_str_accepts_hex_with_and_without_prefix() {
        let s_plain = "1111111111111111111111111111111111111111111111111111111111111111";
        let s_prefixed = format!("0x{s_plain}");
        QuoteSigner::from_secret_str(s_plain, SigningScheme::Ed25519).unwrap();
        QuoteSigner::from_secret_str(&s_prefixed, SigningScheme::Ed25519).unwrap();
    }

    #[test]
    fn from_str_accepts_suiprivkey_bech32() {
        for (scheme, flag) in [
            (SigningScheme::Ed25519, 0u8),
            (SigningScheme::Secp256k1, 1u8),
            (SigningScheme::Secp256r1, 2u8),
        ] {
            let s = build_suiprivkey(flag, &[7u8; 32]);
            let signer = QuoteSigner::from_secret_str(&s, scheme).unwrap();
            assert_eq!(signer.scheme(), scheme);
            // Same input via from_secret should produce the same pubkey.
            let direct = QuoteSigner::from_secret(scheme, &[7u8; 32]).unwrap();
            assert_eq!(signer.public_bytes(), direct.public_bytes());
        }
    }

    #[test]
    fn from_str_rejects_scheme_mismatch() {
        // Ed25519 bech32 but caller expects Secp256k1 — fail closed.
        let s = build_suiprivkey(0, &[7u8; 32]);
        let msg = match QuoteSigner::from_secret_str(&s, SigningScheme::Secp256k1) {
            Ok(_) => panic!("expected scheme-mismatch error, got Ok"),
            Err(e) => format!("{e}"),
        };
        assert!(msg.contains("ed25519"), "msg = {msg}");
        assert!(msg.contains("secp256k1"), "msg = {msg}");
    }
}
