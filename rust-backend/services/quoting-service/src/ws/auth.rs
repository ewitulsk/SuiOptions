//! MM auth via signature challenge-response.
//!
//! Flow (§5.4.1, §5.4.5–§5.4.6):
//!
//! 1. MM connects and sends
//!    `Hello { account_id, signing_scheme, signing_pubkey }`.
//! 2. Service issues `AuthChallenge { challenge }` — 32 random bytes.
//! 3. MM signs those bytes with its Account signing key (using whichever of
//!    Ed25519 / Secp256k1 / Secp256r1 it registered on chain) and replies
//!    `AuthResponse { signature }`.
//! 4. Service verifies via [`shared::protocol_types::verify_signature`]. The pubkey
//!    + scheme supplied in the Hello must agree with what the indexer
//!    holds on file for that Account — otherwise the MM is claiming an
//!    account it doesn't control.

use shared::protocol_types::SigningScheme;

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    /// `(scheme, signing_pubkey)` doesn't match the on-chain registration
    /// the indexer reports.
    PubkeyMismatch,
    /// The indexer hasn't recorded a scheme for this account yet.
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
/// `indexer_*` come from `AccountMirror` (what the chain registered).
/// `supplied_*` come from the MM's `Hello`. Both must match before we
/// verify the signature so an MM can't claim an account it doesn't own.
pub fn verify_challenge_response(
    indexer_scheme: Option<SigningScheme>,
    indexer_pubkey: &[u8],
    supplied_scheme: SigningScheme,
    supplied_pubkey: &[u8],
    challenge: &[u8],
    signature: &[u8],
) -> Result<(), AuthError> {
    let indexer_scheme = indexer_scheme.ok_or(AuthError::SchemeUnknown)?;
    if indexer_scheme != supplied_scheme || indexer_pubkey != supplied_pubkey {
        return Err(AuthError::PubkeyMismatch);
    }
    shared::protocol_types::verify_signature(indexer_scheme, indexer_pubkey, challenge, signature)
        .map_err(|_| AuthError::SignatureInvalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;
    use sha2::{Digest, Sha256};

    #[test]
    fn happy_path_ed25519() {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key().to_bytes().to_vec();
        let ch = random_challenge();
        let sig = sk.sign(&ch).to_bytes().to_vec();
        verify_challenge_response(
            Some(SigningScheme::Ed25519),
            &pk,
            SigningScheme::Ed25519,
            &pk,
            &ch,
            &sig,
        )
        .unwrap();
    }

    #[test]
    fn happy_path_secp256k1() {
        use k256::ecdsa::signature::hazmat::PrehashSigner;
        use k256::ecdsa::{Signature, SigningKey as K1Key};
        let sk = K1Key::random(&mut OsRng);
        let pk = sk.verifying_key().to_encoded_point(true).as_bytes().to_vec();
        let ch = random_challenge();
        let digest = Sha256::digest(&ch);
        let (sig, _): (Signature, _) = sk.sign_prehash(&digest).unwrap();
        let sig_bytes = sig.to_bytes().to_vec();
        verify_challenge_response(
            Some(SigningScheme::Secp256k1),
            &pk,
            SigningScheme::Secp256k1,
            &pk,
            &ch,
            &sig_bytes,
        )
        .unwrap();
    }

    #[test]
    fn happy_path_secp256r1() {
        use p256::ecdsa::signature::hazmat::PrehashSigner;
        use p256::ecdsa::{Signature, SigningKey as R1Key};
        let sk = R1Key::random(&mut OsRng);
        let pk = sk.verifying_key().to_encoded_point(true).as_bytes().to_vec();
        let ch = random_challenge();
        let digest = Sha256::digest(&ch);
        let (sig, _): (Signature, _) = sk.sign_prehash(&digest).unwrap();
        let sig_bytes = sig.to_bytes().to_vec();
        verify_challenge_response(
            Some(SigningScheme::Secp256r1),
            &pk,
            SigningScheme::Secp256r1,
            &pk,
            &ch,
            &sig_bytes,
        )
        .unwrap();
    }

    #[test]
    fn supplied_pubkey_must_match_indexer() {
        let sk = SigningKey::generate(&mut OsRng);
        let other = SigningKey::generate(&mut OsRng).verifying_key().to_bytes().to_vec();
        let pk = sk.verifying_key().to_bytes().to_vec();
        let ch = random_challenge();
        let sig = sk.sign(&ch).to_bytes().to_vec();
        assert_eq!(
            verify_challenge_response(
                Some(SigningScheme::Ed25519),
                &pk,
                SigningScheme::Ed25519,
                &other,
                &ch,
                &sig,
            ),
            Err(AuthError::PubkeyMismatch),
        );
    }

    #[test]
    fn supplied_scheme_must_match_indexer() {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key().to_bytes().to_vec();
        let ch = random_challenge();
        let sig = sk.sign(&ch).to_bytes().to_vec();
        // Indexer thinks the account is k1, MM claims Ed25519 with valid
        // Ed25519 inputs. Must reject before signature verification.
        assert_eq!(
            verify_challenge_response(
                Some(SigningScheme::Secp256k1),
                &pk,
                SigningScheme::Ed25519,
                &pk,
                &ch,
                &sig,
            ),
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
            verify_challenge_response(
                Some(SigningScheme::Ed25519),
                &pk,
                SigningScheme::Ed25519,
                &pk,
                &tampered,
                &sig,
            ),
            Err(AuthError::SignatureInvalid),
        );
    }

    #[test]
    fn indexer_with_unset_scheme_fails_closed() {
        assert_eq!(
            verify_challenge_response(None, &[0; 32], SigningScheme::Ed25519, &[0; 32], &[], &[]),
            Err(AuthError::SchemeUnknown),
        );
    }
}
