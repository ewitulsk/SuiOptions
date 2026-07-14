//! The MM quote payload whose **Borsh bytes must byte-match
//! `options_core::quote::Quote`** — the on-chain `execute_write` verifier
//! introspects the transaction's Ed25519SigVerify precompile instruction and
//! compares its message against exactly these bytes.
//!
//! This service is a main-workspace member, so it cannot depend on the
//! program crate (Anchor/solana-sdk live in the standalone Solana
//! workspaces). [`SolanaQuote`] is a plain-borsh mirror instead, locked by a
//! hand-computed golden vector below. `crates/solana-tx` carries the same
//! struct golden-tested against the program crate itself
//! (`solana_tx::quote::quote_bytes`, test `quote_borsh_layout_is_byte_exact`),
//! so drift against the deployed program is caught there.
//!
//! Field order is frozen and mirrors `options_core::quote::Quote`:
//!
//! ```text
//! protocol_id:            Pubkey (32 raw bytes)   // options_core Config PDA
//! signer_account:         Pubkey (32 raw bytes)   // the MM's MmAccount
//! signer_token_recipient: Pubkey (32 raw bytes)
//! bucket:                 Pubkey (32 raw bytes)
//! write_amount:           u64 (little-endian)
//! premium:                u64 (little-endian)
//! valid_until_ms:         u64 (little-endian)
//! nonce:                  u64 (little-endian)
//! ```
//!
//! JSON wire form ([`QuoteWire`]): base58 pubkeys, decimal-string ints —
//! matching `solana_tx::quote::QuoteWire` and the Sui twin's conventions.

use anyhow::{anyhow, Context, Result};
use borsh::BorshSerialize;
use serde::{Deserialize, Serialize};

use crate::coding::u64_string;

/// Borsh mirror of `options_core::quote::Quote`. Pubkeys are raw 32-byte
/// arrays (Borsh encodes `[u8; 32]` as the bare bytes, identical to Anchor's
/// `Pubkey` encoding — no length prefix).
#[derive(BorshSerialize, Clone, Debug, PartialEq, Eq)]
pub struct SolanaQuote {
    pub protocol_id: [u8; 32],
    pub signer_account: [u8; 32],
    pub signer_token_recipient: [u8; 32],
    pub bucket: [u8; 32],
    pub write_amount: u64,
    pub premium: u64,
    pub valid_until_ms: u64,
    pub nonce: u64,
}

impl SolanaQuote {
    /// Canonical quote bytes: the Borsh encoding — exactly the message the
    /// on-chain verifier compares against and the MM's ed25519 key signs.
    pub fn to_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("borsh-serializing a SolanaQuote into a Vec cannot fail")
    }
}

/// Decode a base58 pubkey string into its raw 32 bytes.
pub fn decode_base58_32(s: &str, field: &str) -> Result<[u8; 32]> {
    let bytes = bs58::decode(s)
        .into_vec()
        .with_context(|| format!("quote {field} is not base58: {s:?}"))?;
    bytes
        .try_into()
        .map_err(|_| anyhow!("quote {field} is not a 32-byte pubkey: {s:?}"))
}

/// JSON wire form of [`SolanaQuote`]: base58 pubkeys, decimal-string ints.
/// Field names match `solana_tx::quote::QuoteWire` so the mm-bot signs and
/// ships the identical shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteWire {
    pub protocol_id: String,
    pub signer_account: String,
    pub signer_token_recipient: String,
    pub bucket: String,
    #[serde(with = "u64_string")]
    pub write_amount: u64,
    #[serde(with = "u64_string")]
    pub premium: u64,
    #[serde(with = "u64_string")]
    pub valid_until_ms: u64,
    #[serde(with = "u64_string")]
    pub nonce: u64,
}

impl TryFrom<&QuoteWire> for SolanaQuote {
    type Error = anyhow::Error;

    fn try_from(w: &QuoteWire) -> Result<Self> {
        Ok(Self {
            protocol_id: decode_base58_32(&w.protocol_id, "protocol_id")?,
            signer_account: decode_base58_32(&w.signer_account, "signer_account")?,
            signer_token_recipient: decode_base58_32(
                &w.signer_token_recipient,
                "signer_token_recipient",
            )?,
            bucket: decode_base58_32(&w.bucket, "bucket")?,
            write_amount: w.write_amount,
            premium: w.premium,
            valid_until_ms: w.valid_until_ms,
            nonce: w.nonce,
        })
    }
}

/// Detached ed25519 verification over arbitrary bytes — used for both the
/// MM auth challenge and the canonical quote bytes. Returns false on any
/// malformed input (wrong key/signature length) — fail closed.
pub fn verify_ed25519(pubkey: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let Ok(pk_bytes) = <[u8; 32]>::try_from(pubkey) else {
        return false;
    };
    let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&pk_bytes) else {
        return false;
    };
    let Ok(sig_bytes) = <[u8; 64]>::try_from(signature) else {
        return false;
    };
    let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
    use ed25519_dalek::Verifier as _;
    vk.verify(message, &sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    fn sample_quote() -> SolanaQuote {
        SolanaQuote {
            protocol_id: [0x11; 32],
            signer_account: [0x22; 32],
            signer_token_recipient: [0x33; 32],
            bucket: [0x44; 32],
            write_amount: 10_000_000,
            premium: 50_000_000,
            valid_until_ms: 1_748_534_400_000,
            nonce: 42,
        }
    }

    /// GOLDEN: hand-built Borsh bytes for the fixture quote, locking the
    /// wire format this service verifies signatures over. The same fixture
    /// values appear in `crates/solana-tx/src/quote.rs`, whose golden test
    /// runs against the actual program type — if either breaks, on-chain
    /// signature verification breaks for every off-chain signer.
    #[test]
    fn borsh_layout_is_byte_exact() {
        let bytes = sample_quote().to_bytes();
        let mut expected = Vec::new();
        expected.extend_from_slice(&[0x11; 32]); // protocol_id
        expected.extend_from_slice(&[0x22; 32]); // signer_account
        expected.extend_from_slice(&[0x33; 32]); // signer_token_recipient
        expected.extend_from_slice(&[0x44; 32]); // bucket
        expected.extend_from_slice(&10_000_000u64.to_le_bytes()); // write_amount
        expected.extend_from_slice(&50_000_000u64.to_le_bytes()); // premium
        expected.extend_from_slice(&1_748_534_400_000u64.to_le_bytes()); // valid_until_ms
        expected.extend_from_slice(&42u64.to_le_bytes()); // nonce
        assert_eq!(bytes, expected);
        // 4 pubkeys + 4 u64s, no prefixes anywhere.
        assert_eq!(bytes.len(), 4 * 32 + 4 * 8);
    }

    #[test]
    fn wire_json_round_trips_with_decimal_strings_and_base58() {
        let q = sample_quote();
        let wire = QuoteWire {
            protocol_id: bs58::encode(q.protocol_id).into_string(),
            signer_account: bs58::encode(q.signer_account).into_string(),
            signer_token_recipient: bs58::encode(q.signer_token_recipient).into_string(),
            bucket: bs58::encode(q.bucket).into_string(),
            write_amount: q.write_amount,
            premium: q.premium,
            valid_until_ms: q.valid_until_ms,
            nonce: q.nonce,
        };
        let v: serde_json::Value = serde_json::to_value(&wire).unwrap();
        assert_eq!(v["write_amount"], "10000000");
        assert_eq!(v["premium"], "50000000");
        assert_eq!(v["valid_until_ms"], "1748534400000");
        assert_eq!(v["nonce"], "42");
        assert_eq!(v["protocol_id"], bs58::encode([0x11u8; 32]).into_string());

        let back: QuoteWire = serde_json::from_value(v).unwrap();
        assert_eq!(SolanaQuote::try_from(&back).unwrap(), q);
    }

    #[test]
    fn wire_rejects_malformed_pubkeys() {
        let mut wire = QuoteWire {
            protocol_id: bs58::encode([0u8; 32]).into_string(),
            signer_account: bs58::encode([0u8; 32]).into_string(),
            signer_token_recipient: bs58::encode([0u8; 32]).into_string(),
            bucket: bs58::encode([0u8; 32]).into_string(),
            write_amount: 1,
            premium: 1,
            valid_until_ms: 1,
            nonce: 1,
        };
        wire.bucket = "not-base58-0OIl".into();
        assert!(SolanaQuote::try_from(&wire).is_err());
        wire.bucket = bs58::encode([0u8; 31]).into_string(); // wrong length
        assert!(SolanaQuote::try_from(&wire).is_err());
    }

    #[test]
    fn ed25519_verifies_canonical_bytes_and_rejects_tampering() {
        let sk = SigningKey::generate(&mut OsRng);
        let q = sample_quote();
        let bytes = q.to_bytes();
        let sig = sk.sign(&bytes).to_bytes().to_vec();
        let pk = sk.verifying_key().to_bytes().to_vec();
        assert!(verify_ed25519(&pk, &bytes, &sig));

        let mut tampered = q.clone();
        tampered.premium += 1;
        assert!(!verify_ed25519(&pk, &tampered.to_bytes(), &sig));
        // Malformed inputs fail closed.
        assert!(!verify_ed25519(&pk[..31], &bytes, &sig));
        assert!(!verify_ed25519(&pk, &bytes, &sig[..63]));
    }
}
