//! The signed quote payload. This is the one type whose BCS encoding **must**
//! byte-match the Move struct in `options_core::quote::Quote` — the chain
//! hashes these bytes and verifies the signature against the QuoteSigner's
//! registered pubkey.
//!
//! Field order matches the Move declaration (NORMATIVE):
//!
//! ```text
//! protocol_id:            vector<u8>           // ULEB128 len + bytes
//! signer_id:              ID (32 bytes)        // the QuoteSigner object
//! collateral_source:      ID (32 bytes)        // object release() debits
//! release_package:        address (32 bytes)   // package holding release()
//! release_module:         String               // ULEB128 len + utf8 bytes
//! signer_token_recipient: address (32 bytes)
//! spec.asset:             TypeName             // ULEB128 len + ascii bytes
//! spec.settlement:        TypeName             // ULEB128 len + ascii bytes
//! spec.expiry_ms:         u64 (little-endian)
//! spec.sig:               u64 (little-endian)
//! spec.exp:               u8
//! spec.is_put:            bool (1 byte)
//! max_total_written:      u128 (little-endian)
//! write_amount:           u64 (little-endian)
//! premium:                u64 (little-endian)
//! valid_until_ms:         u64 (little-endian)
//! nonce:                  u64 (little-endian)
//! ```
//!
//! `spec` is a nested struct; BCS concatenates its fields inline with no tag
//! or length prefix for the struct itself. Its `TypeName`s must be in CHAIN
//! form — see [`crate::bucket_spec`], which is why `BucketSpec::new` is the
//! only constructor that belongs on a signing path.
//!
//! # Why a spec and not a bucket id
//!
//! Buckets are created just-in-time, so the transaction that fills a quote is
//! often the one that creates its bucket. Binding an object id would make a
//! fresh strike unquotable until somebody had paid to create it. Binding the
//! economics is safe because `bucket_registry` admits exactly one bucket per
//! spec, and it is what lets an MM price a strike that does not exist yet.
//!
//! JSON encoding (§4.3) is the human-readable variant: hex for byte arrays,
//! decimal strings for u64, plain strings for the module name.

use serde::{Deserialize, Serialize};

use super::bucket_spec::BucketSpec;
use super::coding::{bytes_hex, u128_string, u64_string};
use super::errors::ProtocolError;
use super::ids::{ObjectId, SuiAddress};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quote {
    #[serde(with = "bytes_hex")]
    pub protocol_id: Vec<u8>,
    /// The `QuoteSigner` whose key + nonce table authorize this quote.
    pub signer_id: ObjectId,
    /// The collateral object the MM's `release()` implementation debits.
    pub collateral_source: ObjectId,
    /// Package + module containing the standardized `release` function. The
    /// full routing is INSIDE the signature so no intermediary can corrupt it.
    pub release_package: SuiAddress,
    pub release_module: String,
    pub signer_token_recipient: SuiAddress,
    /// The bucket's economic identity — what this quote is actually a price
    /// for. Verified on chain against the bucket's own fields.
    pub spec: BucketSpec,
    /// Assignment-risk bound: the fill is refused if the bucket already has
    /// more than this written ahead of it. `u128::MAX` opts out.
    #[serde(with = "u128_string")]
    pub max_total_written: u128,
    #[serde(with = "u64_string")]
    pub write_amount: u64,
    #[serde(with = "u64_string")]
    pub premium: u64,
    #[serde(with = "u64_string")]
    pub valid_until_ms: u64,
    #[serde(with = "u64_string")]
    pub nonce: u64,
}

impl Quote {
    /// Canonical BCS bytes — the payload the MM signs over.
    pub fn to_bcs_bytes(&self) -> Result<Vec<u8>, ProtocolError> {
        bcs::to_bytes(self).map_err(|e| ProtocolError::Bcs(e.to_string()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedQuote {
    pub quote: Quote,
    #[serde(with = "bytes_hex")]
    pub signature: Vec<u8>,
}

impl SignedQuote {
    /// Verify the signature against the given pubkey + scheme, plus the
    /// surface-level invariants that don't need on-chain state (protocol_id
    /// match, expiry, signer id). Nonce uniqueness and bucket existence are
    /// checked elsewhere (QuoteSigner / state).
    ///
    /// For Ed25519 (`scheme == 0`) we sign/verify the raw BCS payload.
    /// For Secp256k1 / Secp256r1 the verifier hashes the BCS payload with
    /// SHA-256 internally — matching the on-chain `hash = 1` flag passed
    /// to `ecdsa_k1::secp256k1_verify` / `ecdsa_r1::secp256r1_verify`.
    pub fn verify(
        &self,
        scheme: crate::SigningScheme,
        signing_pubkey: &[u8],
        expected_protocol_id: &[u8],
        expected_signer_id: ObjectId,
        now_ms: u64,
    ) -> Result<(), ProtocolError> {
        if self.quote.protocol_id != expected_protocol_id {
            return Err(ProtocolError::QuoteProtocolMismatch);
        }
        if self.quote.signer_id != expected_signer_id {
            return Err(ProtocolError::QuoteAccountMismatch);
        }
        if now_ms >= self.quote.valid_until_ms {
            return Err(ProtocolError::QuoteExpired);
        }
        let payload = self.quote.to_bcs_bytes()?;
        crate::verify_signature(scheme, signing_pubkey, &payload, &self.signature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SigningScheme;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;
    use sha2::{Digest, Sha256};

    fn sample_quote() -> Quote {
        Quote {
            protocol_id: vec![0xa1, 0xb2, 0xc3],
            signer_id: ObjectId::new([0x11; 32]),
            collateral_source: ObjectId::new([0x44; 32]),
            release_package: SuiAddress::new([0x55; 32]),
            release_module: "mm_collateral".to_string(),
            signer_token_recipient: SuiAddress::new([0x22; 32]),
            spec: BucketSpec::new(
                "0x9::tbtc::TBTC",
                "0x9::tusdc::TUSDC",
                1_782_345_600_000,
                85_000,
                0,
                false,
            )
            .unwrap(),
            max_total_written: u128::MAX,
            write_amount: 10_000_000,
            premium: 50_000_000,
            valid_until_ms: 1_748_534_400_000,
            nonce: 42,
        }
    }

    /// Hand-built golden BCS bytes for `sample_quote`, locking in the wire
    /// format. If this test breaks, signature verification breaks on chain —
    /// every off-chain signer (mm-bot, frontend) must produce exactly these
    /// bytes for this quote.
    #[test]
    fn bcs_layout_is_byte_exact() {
        let bytes = sample_quote().to_bcs_bytes().unwrap();
        let mut expected = Vec::new();
        // protocol_id: ULEB128(3) + bytes
        expected.push(0x03);
        expected.extend_from_slice(&[0xa1, 0xb2, 0xc3]);
        // signer_id: 32 bytes
        expected.extend_from_slice(&[0x11; 32]);
        // collateral_source: 32 bytes
        expected.extend_from_slice(&[0x44; 32]);
        // release_package: 32 bytes
        expected.extend_from_slice(&[0x55; 32]);
        // release_module: ULEB128 len + utf8 bytes ("mm_collateral" = 13)
        expected.push(13);
        expected.extend_from_slice(b"mm_collateral");
        // signer_token_recipient: 32 bytes
        expected.extend_from_slice(&[0x22; 32]);
        // spec.asset: ULEB128 len + ascii bytes, CHAIN FORM (no 0x, 64-padded)
        let asset = format!("{:0>64}::tbtc::TBTC", "9");
        expected.push(asset.len() as u8);
        expected.extend_from_slice(asset.as_bytes());
        // spec.settlement: same
        let settlement = format!("{:0>64}::tusdc::TUSDC", "9");
        expected.push(settlement.len() as u8);
        expected.extend_from_slice(settlement.as_bytes());
        // spec.expiry_ms: u64 LE
        expected.extend_from_slice(&1_782_345_600_000u64.to_le_bytes());
        // spec.sig: u64 LE (normalized significand)
        expected.extend_from_slice(&85_000u64.to_le_bytes());
        // spec.exp: u8
        expected.push(0);
        // spec.is_put: bool
        expected.push(0);
        // max_total_written: u128 LE
        expected.extend_from_slice(&u128::MAX.to_le_bytes());
        // write_amount: u64 LE
        expected.extend_from_slice(&10_000_000u64.to_le_bytes());
        // premium: u64 LE
        expected.extend_from_slice(&50_000_000u64.to_le_bytes());
        // valid_until_ms: u64 LE
        expected.extend_from_slice(&1_748_534_400_000u64.to_le_bytes());
        // nonce: u64 LE
        expected.extend_from_slice(&42u64.to_le_bytes());
        assert_eq!(bytes, expected);
    }

    /// Golden-bytes serialization of a fully-known quote, hex-pinned. Guards
    /// the cross-language signing contract (Move `bcs::to_bytes(&quote)` must
    /// hash these exact bytes).
    #[test]
    fn bcs_golden_bytes() {
        let bytes = sample_quote().to_bcs_bytes().unwrap();
        let expected_hex = concat!(
            "03a1b2c3",
            "1111111111111111111111111111111111111111111111111111111111111111",
            "4444444444444444444444444444444444444444444444444444444444444444",
            "5555555555555555555555555555555555555555555555555555555555555555",
            "0d6d6d5f636f6c6c61746572616c",
            "2222222222222222222222222222222222222222222222222222222222222222",
            // spec.asset: ULEB(76) + chain-form type (no 0x, 64-padded)
            "4c303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030393a3a746274633a3a54425443",
            // spec.settlement: ULEB(78) + chain-form type
            "4e303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030393a3a74757364633a3a5455534443",
            "008c13fc9e010000", // spec.expiry_ms
            "084c010000000000", // spec.sig (normalized significand)
            "00",                 // spec.exp
            "00",                 // spec.is_put
            "ffffffffffffffffffffffffffffffff", // max_total_written
            "8096980000000000",
            "80f0fa0200000000",
            "0094c51c97010000",
            "2a00000000000000",
        );
        assert_eq!(hex::encode(&bytes), expected_hex);
    }

    #[test]
    fn json_matches_spec_shape() {
        let q = sample_quote();
        let v: serde_json::Value = serde_json::to_value(&q).unwrap();
        assert_eq!(v["protocol_id"], "0xa1b2c3");
        assert_eq!(v["write_amount"], "10000000");
        assert_eq!(v["premium"], "50000000");
        assert_eq!(v["valid_until_ms"], "1748534400000");
        assert_eq!(v["nonce"], "42");
        assert_eq!(v["release_module"], "mm_collateral");
        assert_eq!(v["max_total_written"], u128::MAX.to_string());
        // The spec's types travel in CHAIN form — no 0x — because that is
        // what the signed bytes carry.
        let asset = v["spec"]["asset"].as_str().unwrap();
        assert!(!asset.starts_with("0x"), "{asset}");
        assert!(asset.ends_with("::tbtc::TBTC"));
        assert_eq!(v["spec"]["sig"], "85000");
        assert_eq!(v["spec"]["exp"], 0);
        assert_eq!(v["spec"]["is_put"], false);
        // ids are 0x-prefixed hex strings of 64 hex chars.
        let source = v["collateral_source"].as_str().unwrap();
        assert!(source.starts_with("0x44"));
        let pkg = v["release_package"].as_str().unwrap();
        assert!(pkg.starts_with("0x55"));

        let round: Quote = serde_json::from_value(v).unwrap();
        assert_eq!(round, q);
    }

    #[test]
    fn signed_quote_round_trips_json() {
        let signed = SignedQuote {
            quote: sample_quote(),
            signature: vec![0xff; 64],
        };
        let j = serde_json::to_string(&signed).unwrap();
        let back: SignedQuote = serde_json::from_str(&j).unwrap();
        assert_eq!(back, signed);
    }

    fn sign_ed25519(q: &Quote) -> (SigningScheme, Vec<u8>, Vec<u8>) {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key().to_bytes().to_vec();
        let sig = sk.sign(&q.to_bcs_bytes().unwrap()).to_bytes().to_vec();
        (SigningScheme::Ed25519, pk, sig)
    }

    fn sign_secp256k1(q: &Quote) -> (SigningScheme, Vec<u8>, Vec<u8>) {
        use k256::ecdsa::signature::hazmat::PrehashSigner;
        use k256::ecdsa::{Signature, SigningKey};
        let sk = SigningKey::random(&mut OsRng);
        let pk = sk.verifying_key().to_encoded_point(true).as_bytes().to_vec();
        let digest = Sha256::digest(q.to_bcs_bytes().unwrap());
        let (sig, _): (Signature, _) = sk.sign_prehash(&digest).unwrap();
        (SigningScheme::Secp256k1, pk, sig.to_bytes().to_vec())
    }

    fn sign_secp256r1(q: &Quote) -> (SigningScheme, Vec<u8>, Vec<u8>) {
        use p256::ecdsa::signature::hazmat::PrehashSigner;
        use p256::ecdsa::{Signature, SigningKey};
        let sk = SigningKey::random(&mut OsRng);
        let pk = sk.verifying_key().to_encoded_point(true).as_bytes().to_vec();
        let digest = Sha256::digest(q.to_bcs_bytes().unwrap());
        let (sig, _): (Signature, _) = sk.sign_prehash(&digest).unwrap();
        (SigningScheme::Secp256r1, pk, sig.to_bytes().to_vec())
    }

    #[test]
    fn verify_accepts_each_scheme() {
        for signer in [sign_ed25519, sign_secp256k1, sign_secp256r1] {
            let quote = sample_quote();
            let (scheme, pk, sig) = signer(&quote);
            let signed = SignedQuote {
                quote,
                signature: sig,
            };
            signed
                .verify(scheme, &pk, &[0xa1, 0xb2, 0xc3], ObjectId::new([0x11; 32]), 0)
                .unwrap();
        }
    }

    #[test]
    fn verify_rejects_tampering() {
        let quote = sample_quote();
        let (scheme, pk, sig) = sign_ed25519(&quote);
        let mut signed = SignedQuote {
            quote,
            signature: sig,
        };
        // Bump the premium — signature should no longer verify.
        signed.quote.premium += 1;
        assert_eq!(
            signed.verify(scheme, &pk, &[0xa1, 0xb2, 0xc3], ObjectId::new([0x11; 32]), 0),
            Err(ProtocolError::QuoteSignatureInvalid),
        );
    }

    #[test]
    fn verify_rejects_routing_tampering() {
        // The collateral routing is inside the signature: swapping any of
        // collateral_source / release_package / release_module invalidates it.
        let quote = sample_quote();
        let (scheme, pk, sig) = sign_ed25519(&quote);
        let base = SignedQuote {
            quote,
            signature: sig,
        };
        let mut a = base.clone();
        a.quote.collateral_source = ObjectId::new([0xde; 32]);
        let mut b = base.clone();
        b.quote.release_package = SuiAddress::new([0xad; 32]);
        let mut c = base;
        c.quote.release_module = "evil".to_string();
        for tampered in [a, b, c] {
            assert_eq!(
                tampered.verify(scheme, &pk, &[0xa1, 0xb2, 0xc3], ObjectId::new([0x11; 32]), 0),
                Err(ProtocolError::QuoteSignatureInvalid),
            );
        }
    }

    #[test]
    fn verify_rejects_expired() {
        let quote = sample_quote();
        let valid_until = quote.valid_until_ms;
        let (scheme, pk, sig) = sign_ed25519(&quote);
        let signed = SignedQuote {
            quote,
            signature: sig,
        };
        assert_eq!(
            signed.verify(scheme, &pk, &[0xa1, 0xb2, 0xc3], ObjectId::new([0x11; 32]), valid_until),
            Err(ProtocolError::QuoteExpired),
        );
    }

    #[test]
    fn verify_rejects_wrong_protocol_id() {
        let quote = sample_quote();
        let (scheme, pk, sig) = sign_ed25519(&quote);
        let signed = SignedQuote {
            quote,
            signature: sig,
        };
        assert_eq!(
            signed.verify(scheme, &pk, &[0x00], ObjectId::new([0x11; 32]), 0),
            Err(ProtocolError::QuoteProtocolMismatch),
        );
    }

    #[test]
    fn verify_rejects_wrong_signer() {
        let quote = sample_quote();
        let (scheme, pk, sig) = sign_ed25519(&quote);
        let signed = SignedQuote {
            quote,
            signature: sig,
        };
        assert_eq!(
            signed.verify(scheme, &pk, &[0xa1, 0xb2, 0xc3], ObjectId::new([0xaa; 32]), 0),
            Err(ProtocolError::QuoteAccountMismatch),
        );
    }

    #[test]
    fn verify_rejects_pubkey_for_wrong_scheme() {
        // Sign with Ed25519, claim it's k1 — must reject (key length is wrong).
        let quote = sample_quote();
        let (_, pk, sig) = sign_ed25519(&quote);
        let signed = SignedQuote {
            quote,
            signature: sig,
        };
        assert_eq!(
            signed.verify(SigningScheme::Secp256k1, &pk, &[0xa1, 0xb2, 0xc3], ObjectId::new([0x11; 32]), 0),
            Err(ProtocolError::QuoteSignatureInvalid),
        );
    }
}
