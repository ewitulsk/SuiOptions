//! The signed quote payload. This is the one type whose BCS encoding **must**
//! byte-match the Move struct in §3.2.7 — the chain hashes these bytes and
//! `ed25519_verify`s the signature against the Account's registered pubkey.
//!
//! Field order matches the Move declaration:
//!
//! ```text
//! protocol_id:            vector<u8>           // ULEB128 len + bytes
//! signer_account_id:      ID (32 bytes)
//! signer_token_recipient: address (32 bytes)
//! bucket_id:              ID (32 bytes)
//! write_amount:           u64 (little-endian)
//! premium:                u64 (little-endian)
//! valid_until_ms:         u64 (little-endian)
//! nonce:                  u64 (little-endian)
//! ```
//!
//! JSON encoding (§4.3) is the human-readable variant: hex for byte arrays,
//! decimal strings for u64.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::coding::{bytes_hex, u64_string};
use crate::errors::ProtocolError;
use crate::ids::{ObjectId, SuiAddress};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quote {
    #[serde(with = "bytes_hex")]
    pub protocol_id: Vec<u8>,
    pub signer_account_id: ObjectId,
    pub signer_token_recipient: SuiAddress,
    pub bucket_id: ObjectId,
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
    /// Verify the Ed25519 signature against the given pubkey, plus the
    /// surface-level invariants that don't need on-chain state (protocol_id
    /// match, expiry, signer account id). Nonce uniqueness and bucket
    /// existence are checked elsewhere (Account / state).
    pub fn verify(
        &self,
        signing_pubkey: &VerifyingKey,
        expected_protocol_id: &[u8],
        expected_signer_account_id: ObjectId,
        now_ms: u64,
    ) -> Result<(), ProtocolError> {
        if self.quote.protocol_id != expected_protocol_id {
            return Err(ProtocolError::QuoteProtocolMismatch);
        }
        if self.quote.signer_account_id != expected_signer_account_id {
            return Err(ProtocolError::QuoteAccountMismatch);
        }
        if now_ms >= self.quote.valid_until_ms {
            return Err(ProtocolError::QuoteExpired);
        }
        let payload = self.quote.to_bcs_bytes()?;
        let sig = Signature::from_slice(&self.signature)
            .map_err(|_| ProtocolError::QuoteSignatureInvalid)?;
        signing_pubkey
            .verify(&payload, &sig)
            .map_err(|_| ProtocolError::QuoteSignatureInvalid)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    fn sample_quote() -> Quote {
        Quote {
            protocol_id: vec![0xa1, 0xb2, 0xc3],
            signer_account_id: ObjectId::new([0x11; 32]),
            signer_token_recipient: SuiAddress::new([0x22; 32]),
            bucket_id: ObjectId::new([0x33; 32]),
            write_amount: 10_000_000,
            premium: 50_000_000,
            valid_until_ms: 1_748_534_400_000,
            nonce: 42,
        }
    }

    /// Hand-built BCS bytes for `sample_quote`, locking in the wire format.
    /// If this test breaks, signature verification breaks on chain.
    #[test]
    fn bcs_layout_is_byte_exact() {
        let bytes = sample_quote().to_bcs_bytes().unwrap();
        let mut expected = Vec::new();
        // protocol_id: ULEB128(3) + bytes
        expected.push(0x03);
        expected.extend_from_slice(&[0xa1, 0xb2, 0xc3]);
        // signer_account_id: 32 bytes
        expected.extend_from_slice(&[0x11; 32]);
        // signer_token_recipient: 32 bytes
        expected.extend_from_slice(&[0x22; 32]);
        // bucket_id: 32 bytes
        expected.extend_from_slice(&[0x33; 32]);
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

    #[test]
    fn json_matches_spec_shape() {
        let q = sample_quote();
        let v: serde_json::Value = serde_json::to_value(&q).unwrap();
        assert_eq!(v["protocol_id"], "0xa1b2c3");
        assert_eq!(v["write_amount"], "10000000");
        assert_eq!(v["premium"], "50000000");
        assert_eq!(v["valid_until_ms"], "1748534400000");
        assert_eq!(v["nonce"], "42");
        // ids are 0x-prefixed hex strings of 64 hex chars.
        let bucket = v["bucket_id"].as_str().unwrap();
        assert_eq!(bucket.len(), 66);
        assert!(bucket.starts_with("0x33"));

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

    #[test]
    fn verify_accepts_a_real_signature() {
        let sk = SigningKey::generate(&mut OsRng);
        let vk = sk.verifying_key();
        let quote = sample_quote();
        let payload = quote.to_bcs_bytes().unwrap();
        let signature = sk.sign(&payload).to_bytes().to_vec();
        let signed = SignedQuote { quote, signature };
        signed
            .verify(&vk, &[0xa1, 0xb2, 0xc3], ObjectId::new([0x11; 32]), 0)
            .unwrap();
    }

    #[test]
    fn verify_rejects_tampering() {
        let sk = SigningKey::generate(&mut OsRng);
        let vk = sk.verifying_key();
        let quote = sample_quote();
        let payload = quote.to_bcs_bytes().unwrap();
        let signature = sk.sign(&payload).to_bytes().to_vec();
        let mut signed = SignedQuote { quote, signature };
        // Bump the premium — signature should no longer verify.
        signed.quote.premium += 1;
        assert_eq!(
            signed.verify(&vk, &[0xa1, 0xb2, 0xc3], ObjectId::new([0x11; 32]), 0),
            Err(ProtocolError::QuoteSignatureInvalid),
        );
    }

    #[test]
    fn verify_rejects_expired() {
        let sk = SigningKey::generate(&mut OsRng);
        let vk = sk.verifying_key();
        let quote = sample_quote();
        let payload = quote.to_bcs_bytes().unwrap();
        let signature = sk.sign(&payload).to_bytes().to_vec();
        let signed = SignedQuote { quote, signature };
        let now = signed.quote.valid_until_ms;
        assert_eq!(
            signed.verify(&vk, &[0xa1, 0xb2, 0xc3], ObjectId::new([0x11; 32]), now),
            Err(ProtocolError::QuoteExpired),
        );
    }

    #[test]
    fn verify_rejects_wrong_protocol_id() {
        let sk = SigningKey::generate(&mut OsRng);
        let vk = sk.verifying_key();
        let quote = sample_quote();
        let payload = quote.to_bcs_bytes().unwrap();
        let signature = sk.sign(&payload).to_bytes().to_vec();
        let signed = SignedQuote { quote, signature };
        assert_eq!(
            signed.verify(&vk, &[0x00], ObjectId::new([0x11; 32]), 0),
            Err(ProtocolError::QuoteProtocolMismatch),
        );
    }

    #[test]
    fn verify_rejects_wrong_account() {
        let sk = SigningKey::generate(&mut OsRng);
        let vk = sk.verifying_key();
        let quote = sample_quote();
        let payload = quote.to_bcs_bytes().unwrap();
        let signature = sk.sign(&payload).to_bytes().to_vec();
        let signed = SignedQuote { quote, signature };
        assert_eq!(
            signed.verify(&vk, &[0xa1, 0xb2, 0xc3], ObjectId::new([0xaa; 32]), 0),
            Err(ProtocolError::QuoteAccountMismatch),
        );
    }
}
