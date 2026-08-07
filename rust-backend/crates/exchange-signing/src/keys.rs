//! Keypair helpers: fixture generation, tests, and the relayer's transaction
//! signing. Both signers reproduce the exact byte recipes Sui wallets use.

use crate::{blake2b256, personal_message_signing_digest, TRANSACTION_DATA_INTENT};
use ed25519_dalek::Signer as _;
use k256::ecdsa::signature::hazmat::PrehashSigner as _;
use exchange_types::order::SignatureScheme;
use exchange_types::SuiAddress;
use sha2::{Digest as _, Sha256};

pub struct Ed25519Keypair {
    inner: ed25519_dalek::SigningKey,
}

impl Ed25519Keypair {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self { inner: ed25519_dalek::SigningKey::from_bytes(&seed) }
    }

    pub fn public_key(&self) -> Vec<u8> {
        self.inner.verifying_key().to_bytes().to_vec()
    }

    pub fn address(&self) -> SuiAddress {
        crate::derive_address(SignatureScheme::Ed25519, &self.public_key())
    }

    /// Sign a personal message (e.g. an order digest) exactly as a Sui wallet
    /// does: pure ed25519 over `blake2b256(intent ‖ bcs(message))`.
    pub fn sign_personal_message(&self, message: &[u8]) -> Vec<u8> {
        let signing_digest = personal_message_signing_digest(message);
        self.inner.sign(&signing_digest).to_bytes().to_vec()
    }

    /// Sign raw Sui `TransactionData` bytes for `sui_executeTransactionBlock`:
    /// returns the serialized signature `flag ‖ sig ‖ pk` (base64 it for RPC).
    pub fn sign_transaction_data(&self, tx_data: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(3 + tx_data.len());
        buf.extend_from_slice(&TRANSACTION_DATA_INTENT);
        buf.extend_from_slice(tx_data);
        let digest = blake2b256(&buf);
        let sig = self.inner.sign(&digest);
        let mut out = Vec::with_capacity(1 + 64 + 32);
        out.push(SignatureScheme::Ed25519.flag());
        out.extend_from_slice(&sig.to_bytes());
        out.extend_from_slice(&self.public_key());
        out
    }
}

pub struct Secp256k1Keypair {
    inner: k256::ecdsa::SigningKey,
}

impl Secp256k1Keypair {
    /// Seed must be a valid non-zero scalar (true for any low-value seed used
    /// in fixtures/tests).
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self { inner: k256::ecdsa::SigningKey::from_slice(&seed).expect("valid scalar") }
    }

    /// 33-byte compressed SEC1 point.
    pub fn public_key(&self) -> Vec<u8> {
        self.inner
            .verifying_key()
            .to_encoded_point(true)
            .as_bytes()
            .to_vec()
    }

    pub fn address(&self) -> SuiAddress {
        crate::derive_address(SignatureScheme::Secp256k1, &self.public_key())
    }

    /// ECDSA over `sha256(blake2b256(intent ‖ bcs(message)))`, low-s
    /// normalized (wallet/fastcrypto behavior).
    pub fn sign_personal_message(&self, message: &[u8]) -> Vec<u8> {
        let signing_digest = personal_message_signing_digest(message);
        let prehash = Sha256::digest(signing_digest);
        let sig: k256::ecdsa::Signature = self.inner.sign_prehash(&prehash).expect("sign");
        let sig = sig.normalize_s().unwrap_or(sig);
        sig.to_bytes().to_vec()
    }
}
