//! MM auth via signature challenge-response — ed25519 only.
//!
//! Flow (unchanged from the Sui twin):
//!
//! 1. MM connects and sends
//!    `Hello { account_id, signing_scheme, signing_pubkey }`.
//! 2. Service issues `AuthChallenge { challenge }` — 32 random bytes.
//! 3. MM signs those bytes with its MmAccount signing key and replies
//!    `AuthResponse { signature }`.
//! 4. Service verifies with ed25519-dalek. The pubkey + scheme supplied in
//!    the Hello must agree with what the indexer holds on file for that
//!    MmAccount — otherwise the MM is claiming an account it doesn't
//!    control.
//!
//! Program v1 registers only Ed25519 (`signing_scheme == 0`). Any other
//! scheme — from the indexer or the Hello — is fatal `auth_scheme_unknown`:
//! the service has no verifier for it and the chain would reject the quote
//! anyway.

use crate::quote::verify_ed25519;

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    /// `(scheme, signing_pubkey)` doesn't match the on-chain registration
    /// the indexer reports.
    PubkeyMismatch,
    /// The indexer hasn't recorded this account yet, or the registered /
    /// supplied scheme isn't Ed25519 (0) — nothing we can verify against.
    SchemeUnknown,
    /// Signature didn't verify (wrong size, wrong key, or tampered
    /// challenge).
    SignatureInvalid,
}

pub fn random_challenge() -> Vec<u8> {
    use rand::RngCore;
    let mut out = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut out);
    out.to_vec()
}

/// Verify a challenge response.
///
/// `indexer_*` come from the indexer's `account` query (what the chain
/// registered; `None` scheme means the account is unknown). `supplied_*`
/// come from the MM's `Hello`. Both must match before we verify the
/// signature so an MM can't claim an account it doesn't own.
pub fn verify_challenge_response(
    indexer_scheme: Option<u8>,
    indexer_pubkey: &[u8],
    supplied_scheme: u8,
    supplied_pubkey: &[u8],
    challenge: &[u8],
    signature: &[u8],
) -> Result<(), AuthError> {
    let indexer_scheme = indexer_scheme.ok_or(AuthError::SchemeUnknown)?;
    // ed25519 only: any non-zero scheme tag is unverifiable — fail closed
    // before comparing keys.
    if indexer_scheme != 0 || supplied_scheme != 0 {
        return Err(AuthError::SchemeUnknown);
    }
    if indexer_pubkey != supplied_pubkey {
        return Err(AuthError::PubkeyMismatch);
    }
    if verify_ed25519(indexer_pubkey, challenge, signature) {
        Ok(())
    } else {
        Err(AuthError::SignatureInvalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    #[test]
    fn happy_path_ed25519() {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key().to_bytes().to_vec();
        let ch = random_challenge();
        let sig = sk.sign(&ch).to_bytes().to_vec();
        verify_challenge_response(Some(0), &pk, 0, &pk, &ch, &sig).unwrap();
    }

    #[test]
    fn supplied_pubkey_must_match_indexer() {
        let sk = SigningKey::generate(&mut OsRng);
        let other = SigningKey::generate(&mut OsRng).verifying_key().to_bytes().to_vec();
        let pk = sk.verifying_key().to_bytes().to_vec();
        let ch = random_challenge();
        let sig = sk.sign(&ch).to_bytes().to_vec();
        assert_eq!(
            verify_challenge_response(Some(0), &pk, 0, &other, &ch, &sig),
            Err(AuthError::PubkeyMismatch),
        );
    }

    #[test]
    fn non_ed25519_scheme_fails_closed() {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key().to_bytes().to_vec();
        let ch = random_challenge();
        let sig = sk.sign(&ch).to_bytes().to_vec();
        // Indexer reports a non-zero scheme: unverifiable, even with a
        // valid ed25519 signature over the challenge.
        assert_eq!(
            verify_challenge_response(Some(1), &pk, 1, &pk, &ch, &sig),
            Err(AuthError::SchemeUnknown),
        );
        // MM claims a non-zero scheme against an ed25519 registration.
        assert_eq!(
            verify_challenge_response(Some(0), &pk, 2, &pk, &ch, &sig),
            Err(AuthError::SchemeUnknown),
        );
    }

    #[test]
    fn rejects_tampered_challenge() {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key().to_bytes().to_vec();
        let ch = random_challenge();
        let sig = sk.sign(&ch).to_bytes().to_vec();
        let mut tampered = ch.clone();
        tampered[0] ^= 0x01;
        assert_eq!(
            verify_challenge_response(Some(0), &pk, 0, &pk, &tampered, &sig),
            Err(AuthError::SignatureInvalid),
        );
    }

    #[test]
    fn indexer_with_unknown_account_fails_closed() {
        assert_eq!(
            verify_challenge_response(None, &[0; 32], 0, &[0; 32], &[], &[]),
            Err(AuthError::SchemeUnknown),
        );
    }
}
