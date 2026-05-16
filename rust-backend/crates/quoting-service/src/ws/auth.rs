//! MM auth via Ed25519 challenge-response.
//!
//! Flow (§5.4.1, §5.4.5–§5.4.6):
//!
//! 1. MM connects and sends `Hello { account_id, signing_pubkey }`.
//! 2. Service issues `AuthChallenge { challenge }` — 32 random bytes.
//! 3. MM signs the challenge bytes with its Account signing key and replies
//!    `AuthResponse { signature }`.
//! 4. Service verifies the signature against the pubkey the indexer has on
//!    file for that Account. If they match, session is authenticated. The
//!    pubkey supplied in the Hello must agree with the indexer's record —
//!    otherwise the MM is claiming an account it doesn't control.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    /// `signing_pubkey` doesn't match the on-chain key the indexer reports.
    PubkeyMismatch,
    /// Stored or supplied pubkey isn't a valid Ed25519 key.
    PubkeyInvalid,
    /// Signature isn't 64 bytes or doesn't verify.
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
/// `indexer_pubkey` is what the indexer reports for the claimed account;
/// `supplied_pubkey` is what the MM put in its Hello. They must match —
/// trusting the supplied key alone would let any MM claim any account.
pub fn verify_challenge_response(
    indexer_pubkey: &[u8],
    supplied_pubkey: &[u8],
    challenge: &[u8],
    signature: &[u8],
) -> Result<(), AuthError> {
    if indexer_pubkey != supplied_pubkey {
        return Err(AuthError::PubkeyMismatch);
    }
    if indexer_pubkey.len() != 32 {
        return Err(AuthError::PubkeyInvalid);
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(indexer_pubkey);
    let vk = VerifyingKey::from_bytes(&key).map_err(|_| AuthError::PubkeyInvalid)?;
    let sig = Signature::from_slice(signature).map_err(|_| AuthError::SignatureInvalid)?;
    vk.verify(challenge, &sig)
        .map_err(|_| AuthError::SignatureInvalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    #[test]
    fn happy_path() {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key().to_bytes().to_vec();
        let ch = random_challenge();
        let sig = sk.sign(&ch).to_bytes().to_vec();
        verify_challenge_response(&pk, &pk, &ch, &sig).unwrap();
    }

    #[test]
    fn supplied_pubkey_must_match_indexer() {
        let sk = SigningKey::generate(&mut OsRng);
        let other = SigningKey::generate(&mut OsRng).verifying_key().to_bytes().to_vec();
        let pk = sk.verifying_key().to_bytes().to_vec();
        let ch = random_challenge();
        let sig = sk.sign(&ch).to_bytes().to_vec();
        assert_eq!(
            verify_challenge_response(&pk, &other, &ch, &sig),
            Err(AuthError::PubkeyMismatch),
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
            verify_challenge_response(&pk, &pk, &tampered, &sig),
            Err(AuthError::SignatureInvalid),
        );
    }

    #[test]
    fn rejects_short_pubkey() {
        let sig = vec![0u8; 64];
        assert_eq!(
            verify_challenge_response(&[0; 16], &[0; 16], &[1, 2, 3], &sig),
            Err(AuthError::PubkeyInvalid),
        );
    }
}
