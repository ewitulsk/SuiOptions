//! Verify a Solana wallet `signMessage` signature.
//!
//! `@solana/wallet-adapter`'s `signMessage` produces a detached **ed25519
//! signature over the raw message bytes** — no digest, no intent prefix, no
//! envelope. Unlike Sui's serialized signature the pubkey is not embedded, so
//! the login request supplies it explicitly (base58 — a Solana address IS the
//! base58-encoded ed25519 pubkey).
//!
//! Verification: decode signature (base64, 64 bytes) and pubkey (base58,
//! 32 bytes), then `ed25519_dalek` verify over the raw message. The recovered
//! address is the base58 pubkey string — but only after the signature proves
//! the caller holds its secret key, so it is never trusted bare.
//!
//! Note: some wallets (Ledger via Phantom) wrap messages in the Solana
//! off-chain message envelope (`\xffsolana offchain`). v1 verifies raw bytes
//! only; the Ledger limitation is documented in the frontend guide.

use anyhow::{anyhow, bail, Result};
use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};

/// Verify `signature_b64` over `message` for `pubkey_b58` and return the
/// base58 signer address. Errors if the signature or pubkey is malformed or
/// the signature doesn't verify.
pub fn verify(signature_b64: &str, message: &[u8], pubkey_b58: &str) -> Result<String> {
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(signature_b64.trim())
        .map_err(|e| anyhow!("signature is not valid base64: {e}"))?;
    let sig_arr: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("signature length {} != expected 64", sig_bytes.len()))?;
    let sig = Signature::from_bytes(&sig_arr);

    let pubkey_b58 = pubkey_b58.trim();
    let pk_bytes = bs58::decode(pubkey_b58)
        .into_vec()
        .map_err(|e| anyhow!("pubkey is not valid base58: {e}"))?;
    let pk_arr: [u8; 32] = pk_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("pubkey length {} != expected 32", pk_bytes.len()))?;
    let vk = VerifyingKey::from_bytes(&pk_arr)
        .map_err(|_| anyhow!("pubkey is not a valid ed25519 point"))?;

    if vk.verify_strict(message, &sig).is_err() {
        bail!("signature did not verify for {pubkey_b58}");
    }
    Ok(pubkey_b58.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    fn keypair() -> (SigningKey, String) {
        let sk = SigningKey::generate(&mut OsRng);
        let addr = bs58::encode(sk.verifying_key().to_bytes()).into_string();
        (sk, addr)
    }

    fn sign_b64(sk: &SigningKey, message: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(sk.sign(message).to_bytes())
    }

    /// Sign a challenge the way wallet-adapter's `signMessage` does (raw
    /// bytes, detached ed25519), then check we verify and return the address.
    #[test]
    fn raw_message_round_trip() {
        let (sk, addr) = keypair();
        let message = b"SuiOptions admin login (solana)\nnonce: deadbeef";
        let got = verify(&sign_b64(&sk, message), message, &addr).unwrap();
        assert_eq!(got, addr);
    }

    #[test]
    fn rejects_tampered_message() {
        let (sk, addr) = keypair();
        assert!(verify(&sign_b64(&sk, b"original"), b"tampered", &addr).is_err());
    }

    #[test]
    fn rejects_wrong_pubkey() {
        let (sk, _) = keypair();
        let (_, other_addr) = keypair();
        let message = b"SuiOptions admin login (solana)\nnonce: deadbeef";
        assert!(verify(&sign_b64(&sk, message), message, &other_addr).is_err());
    }

    #[test]
    fn rejects_garbage_signature() {
        let (_, addr) = keypair();
        let message = b"msg";
        // valid base64, wrong bytes
        let garbage = base64::engine::general_purpose::STANDARD.encode([7u8; 64]);
        assert!(verify(&garbage, message, &addr).is_err());
        // not base64 at all
        assert!(verify("!!not-base64!!", message, &addr).is_err());
        // wrong length
        let short = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        assert!(verify(&short, message, &addr).is_err());
    }

    #[test]
    fn rejects_malformed_pubkey() {
        let (sk, _) = keypair();
        let message = b"msg";
        let sig = sign_b64(&sk, message);
        assert!(verify(&sig, message, "0Ol-not-base58").is_err());
        // valid base58 but wrong length
        let short = bs58::encode([1u8; 16]).into_string();
        assert!(verify(&sig, message, &short).is_err());
    }
}
