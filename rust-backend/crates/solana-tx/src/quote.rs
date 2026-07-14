//! The MM quote payload + ed25519 helpers — the port of sui-tx's
//! `quote_signer` and `protocol_types::Quote` for the Solana programs.
//!
//! The canonical type IS the program's (`options_core::quote::Quote`,
//! re-exported here) — the Borsh bytes we sign are byte-identical to what
//! `execute_write` verifies, by construction. [`QuoteWire`] is the JSON
//! shape for the quoting-service / mm-bot wire: base58 pubkeys, decimal
//! strings for u64 (the Sui stack's conventions).

use anchor_lang::AnchorSerialize;
use anyhow::{anyhow, Context, Result};
use ed25519_dalek::Signer as _;
use serde::{Deserialize, Serialize};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;

pub use options_core::quote::{FlowKind, Quote, ED25519_PROGRAM_ID};

/// Canonical quote bytes: the Borsh (AnchorSerialize) encoding — exactly
/// the message the on-chain verifier compares against.
pub fn quote_bytes(quote: &Quote) -> Vec<u8> {
    let mut v = Vec::new();
    quote
        .serialize(&mut v)
        .expect("borsh-serializing a Quote into a Vec cannot fail");
    v
}

/// Sign the canonical quote bytes with a 32-byte ed25519 seed (the
/// `[mm_bot] quote_key` secret). Returns the detached 64-byte signature.
pub fn sign_quote(seed: &[u8], quote: &Quote) -> Result<[u8; 64]> {
    let seed: &[u8; 32] = seed
        .try_into()
        .map_err(|_| anyhow!("quote key must be a 32-byte ed25519 seed (got {})", seed.len()))?;
    let sk = ed25519_dalek::SigningKey::from_bytes(seed);
    Ok(sk.sign(&quote_bytes(quote)).to_bytes())
}

/// The verifying key for a 32-byte seed — what `create_account` registers
/// as the MmAccount's `signing_pubkey`.
pub fn quote_pubkey(seed: &[u8]) -> Result<[u8; 32]> {
    let seed: &[u8; 32] = seed
        .try_into()
        .map_err(|_| anyhow!("quote key must be a 32-byte ed25519 seed (got {})", seed.len()))?;
    Ok(ed25519_dalek::SigningKey::from_bytes(seed)
        .verifying_key()
        .to_bytes())
}

/// Build the native Ed25519SigVerify instruction the way
/// `options_core::quote::verify_ed25519_quote_ix` demands: exactly one
/// signature, and all three instruction-index fields == `u16::MAX`
/// (self-contained — pubkey/signature/message live in this instruction's
/// own data). `new_ed25519_instruction_with_signature` produces exactly
/// that layout (locked by the offset test below).
pub fn ed25519_verify_ix(pubkey: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> Instruction {
    solana_ed25519_program::new_ed25519_instruction_with_signature(message, signature, pubkey)
}

/// JSON wire form of [`Quote`]: base58 pubkeys, decimal-string ints.
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

impl From<&Quote> for QuoteWire {
    fn from(q: &Quote) -> Self {
        Self {
            protocol_id: q.protocol_id.to_string(),
            signer_account: q.signer_account.to_string(),
            signer_token_recipient: q.signer_token_recipient.to_string(),
            bucket: q.bucket.to_string(),
            write_amount: q.write_amount,
            premium: q.premium,
            valid_until_ms: q.valid_until_ms,
            nonce: q.nonce,
        }
    }
}

impl TryFrom<&QuoteWire> for Quote {
    type Error = anyhow::Error;

    fn try_from(w: &QuoteWire) -> Result<Self> {
        let key = |s: &str, field: &str| -> Result<Pubkey> {
            s.parse()
                .with_context(|| format!("quote {field} is not a base58 pubkey: {s}"))
        };
        Ok(Self {
            protocol_id: key(&w.protocol_id, "protocol_id")?,
            signer_account: key(&w.signer_account, "signer_account")?,
            signer_token_recipient: key(&w.signer_token_recipient, "signer_token_recipient")?,
            bucket: key(&w.bucket, "bucket")?,
            write_amount: w.write_amount,
            premium: w.premium,
            valid_until_ms: w.valid_until_ms,
            nonce: w.nonce,
        })
    }
}

mod u64_string {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_quote() -> Quote {
        Quote {
            protocol_id: Pubkey::new_from_array([0x11; 32]),
            signer_account: Pubkey::new_from_array([0x22; 32]),
            signer_token_recipient: Pubkey::new_from_array([0x33; 32]),
            bucket: Pubkey::new_from_array([0x44; 32]),
            write_amount: 10_000_000,
            premium: 50_000_000,
            valid_until_ms: 1_748_534_400_000,
            nonce: 42,
        }
    }

    /// GOLDEN: the canonical bytes are the program type's Borsh encoding —
    /// hand-built here byte for byte. If this breaks, on-chain signature
    /// verification breaks for every off-chain signer.
    #[test]
    fn quote_borsh_layout_is_byte_exact() {
        let bytes = quote_bytes(&sample_quote());
        let mut expected = Vec::new();
        expected.extend_from_slice(&[0x11; 32]); // protocol_id
        expected.extend_from_slice(&[0x22; 32]); // signer_account
        expected.extend_from_slice(&[0x33; 32]); // signer_token_recipient
        expected.extend_from_slice(&[0x44; 32]); // bucket
        expected.extend_from_slice(&10_000_000u64.to_le_bytes());
        expected.extend_from_slice(&50_000_000u64.to_le_bytes());
        expected.extend_from_slice(&1_748_534_400_000u64.to_le_bytes());
        expected.extend_from_slice(&42u64.to_le_bytes());
        assert_eq!(bytes, expected);

        // And AnchorSerialize (what the wire wrapper round-trips through)
        // agrees with plain `.try_to_vec()`-style serialization.
        let mut via_try = Vec::new();
        sample_quote().serialize(&mut via_try).unwrap();
        assert_eq!(bytes, via_try);
    }

    /// The ed25519 instruction must pass the exact checks
    /// `options_core::quote::verify_ed25519_quote_ix` performs (replicated
    /// here): one signature, all instruction indices == u16::MAX, pubkey
    /// and message recoverable at the declared offsets.
    #[test]
    fn ed25519_ix_offsets_match_program_verifier() {
        let quote = sample_quote();
        let msg = quote_bytes(&quote);
        let seed = [42u8; 32];
        let sig = sign_quote(&seed, &quote).unwrap();
        let pk = quote_pubkey(&seed).unwrap();

        let ix = ed25519_verify_ix(&pk, &msg, &sig);
        assert_eq!(ix.program_id, ED25519_PROGRAM_ID);

        let data = &ix.data;
        // Header: [num_signatures, padding], then one 14-byte offsets record.
        assert!(data.len() >= 16);
        assert_eq!(data[0], 1, "exactly one signature");

        let off = |i: usize| u16::from_le_bytes([data[i], data[i + 1]]);
        let signature_offset = off(2) as usize;
        let signature_instruction_index = off(4);
        let public_key_offset = off(6) as usize;
        let public_key_instruction_index = off(8);
        let message_data_offset = off(10) as usize;
        let message_data_size = off(12) as usize;
        let message_instruction_index = off(14);

        assert_eq!(signature_instruction_index, u16::MAX, "self-contained sig");
        assert_eq!(public_key_instruction_index, u16::MAX, "self-contained pk");
        assert_eq!(message_instruction_index, u16::MAX, "self-contained msg");

        assert_eq!(&data[public_key_offset..public_key_offset + 32], &pk);
        assert_eq!(&data[signature_offset..signature_offset + 64], &sig);
        assert_eq!(
            &data[message_data_offset..message_data_offset + message_data_size],
            msg.as_slice()
        );

        // And the signature actually verifies over the canonical bytes.
        use ed25519_dalek::Verifier as _;
        let vk = ed25519_dalek::VerifyingKey::from_bytes(&pk).unwrap();
        vk.verify(&msg, &ed25519_dalek::Signature::from_bytes(&sig))
            .unwrap();
    }

    #[test]
    fn wire_json_round_trips_with_decimal_strings() {
        let q = sample_quote();
        let wire = QuoteWire::from(&q);
        let v: serde_json::Value = serde_json::to_value(&wire).unwrap();
        assert_eq!(v["write_amount"], "10000000");
        assert_eq!(v["premium"], "50000000");
        assert_eq!(v["valid_until_ms"], "1748534400000");
        assert_eq!(v["nonce"], "42");
        assert_eq!(
            v["protocol_id"],
            Pubkey::new_from_array([0x11; 32]).to_string()
        );

        let back: QuoteWire = serde_json::from_value(v).unwrap();
        assert_eq!(Quote::try_from(&back).unwrap(), q);
    }

    #[test]
    fn sign_quote_rejects_bad_seed_length() {
        assert!(sign_quote(&[0u8; 31], &sample_quote()).is_err());
        assert!(quote_pubkey(&[0u8; 33]).is_err());
    }
}
