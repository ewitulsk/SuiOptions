//! Off-chain signing for the Layer 1 transport.
//!
//! Given a [`CrossChainMessage`], the signer computes its keccak256 digest and
//! signs it with the scheme the *destination* family verifies (bridge-spec.md
//! §2.3):
//! - Sui  → Ed25519 over the digest bytes (the Move `ed25519_verify` path).
//! - EVM  → recoverable ECDSA/secp256k1 over the digest (the Solidity
//!   `ecrecover` path): 65 bytes `r || s || v`, low-`s`, `v ∈ {27, 28}`.
//!
//! At M1 this is a single keypair per curve ("1-of-1"); at M3 the same surface
//! becomes threshold share-signing, with the aggregated signature shaped
//! identically so nothing on-chain changes.

use bridge_types::chain_id;
use bridge_types::message::keccak256;
use bridge_types::{Bytes32, CrossChainMessage, Scheme, SignatureEnvelope};
use ed25519_dalek::Signer as _;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SignerError {
    #[error("destination family {0} has no supported signature scheme")]
    UnsupportedDstFamily(u8),
    #[error("ecdsa signing failed: {0}")]
    Ecdsa(#[from] k256::ecdsa::Error),
}

pub struct ThresholdSigner {
    ed25519: ed25519_dalek::SigningKey,
    secp256k1: k256::ecdsa::SigningKey,
}

impl ThresholdSigner {
    /// Build a signer from one 32-byte seed per curve. The secp256k1 seed must
    /// be a valid non-zero scalar below the curve order.
    pub fn from_seeds(ed25519_seed: [u8; 32], secp256k1_seed: [u8; 32]) -> Result<Self, SignerError> {
        Ok(Self {
            ed25519: ed25519_dalek::SigningKey::from_bytes(&ed25519_seed),
            secp256k1: k256::ecdsa::SigningKey::from_slice(&secp256k1_seed)?,
        })
    }

    /// The 32-byte Ed25519 group public key (registered as the Sui group key).
    pub fn ed25519_group_pubkey(&self) -> Bytes32 {
        self.ed25519.verifying_key().to_bytes()
    }

    /// The 20-byte ECDSA group address (registered as the EVM group key);
    /// `keccak256(uncompressed_pubkey[1..])[12..]`, i.e. the Ethereum address.
    pub fn ecdsa_group_address(&self) -> [u8; 20] {
        let vk = self.secp256k1.verifying_key();
        let point = vk.to_encoded_point(false); // 0x04 || X || Y
        let hash = keccak256(&point.as_bytes()[1..]);
        let mut addr = [0u8; 20];
        addr.copy_from_slice(&hash[12..]);
        addr
    }

    /// Sign `message` for its destination chain, returning a ready envelope.
    /// `domain_sep` is [`bridge_types::message::derive_domain_sep`] of the
    /// deployment salt — the digest is domain-separated (spec §2.2).
    pub fn sign(
        &self,
        message: &CrossChainMessage,
        domain_sep: &Bytes32,
        group_pubkey_id: u32,
    ) -> Result<SignatureEnvelope, SignerError> {
        let family = chain_id::family(message.dst_chain_id);
        let scheme =
            Scheme::for_family(family).ok_or(SignerError::UnsupportedDstFamily(family))?;
        let digest = message.digest(domain_sep);
        let signature = match scheme {
            Scheme::Ed25519 => self.sign_ed25519(&digest).to_vec(),
            Scheme::EcdsaSecp256k1 => self.sign_ecdsa_recoverable(&digest)?.to_vec(),
        };
        Ok(SignatureEnvelope::new(scheme, group_pubkey_id, signature))
    }

    /// Raw 64-byte Ed25519 signature over the digest (PureEdDSA).
    pub fn sign_ed25519(&self, digest: &Bytes32) -> [u8; 64] {
        self.ed25519.sign(digest).to_bytes()
    }

    /// 65-byte recoverable ECDSA signature `r || s || v` over the prehashed
    /// digest, low-`s` normalized, `v ∈ {27, 28}` for `ecrecover`.
    pub fn sign_ecdsa_recoverable(&self, digest: &Bytes32) -> Result<[u8; 65], SignerError> {
        let (sig, recid) = self.secp256k1.sign_prehash_recoverable(digest)?;
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&sig.to_bytes());
        out[64] = 27 + recid.to_byte();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bridge_types::message::derive_domain_sep;

    /// Fixed dummy salt shared with the Move + Solidity parity tests.
    const TEST_SALT: Bytes32 = [0x01; 32];

    fn vector_message_to_sui() -> CrossChainMessage {
        CrossChainMessage::new(
            chain_id::encode(chain_id::FAMILY_EVM, 998).unwrap(),
            chain_id::encode(chain_id::FAMILY_SUI, 0).unwrap(),
            7,
            [0xab; 32],
            [0xcd; 32],
            b"hello-bridge".to_vec(),
        )
    }

    /// The Ed25519 path reproduces the EXACT vector the Move + Solidity tests
    /// embed (key seed [0x42; 32] over the known digest). Ed25519 is
    /// deterministic (RFC 8032), so a correct signer is byte-identical to the
    /// signature the Sui Inbox verifies.
    #[test]
    fn ed25519_matches_onchain_vector() {
        let signer = ThresholdSigner::from_seeds([0x42; 32], [0x11; 32]).unwrap();
        assert_eq!(
            hex::encode(signer.ed25519_group_pubkey()),
            "2152f8d19b791d24453242e15f2eab6cb7cffa7b6a5ed30097960e069881db12"
        );
        let ds = derive_domain_sep(&TEST_SALT);
        let env = signer.sign(&vector_message_to_sui(), &ds, 1).unwrap();
        assert_eq!(env.scheme_tag, bridge_types::envelope::SCHEME_ED25519);
        assert_eq!(
            hex::encode(&env.signature),
            "12bc85a949906a86bdea305aa6bc32ef704e77de62ea5fb65a3df3a39902e533\
             98ca95da28a3c34aa8187edcf8f6936330c94016e1e1c4d3f2f7b80027190001"
        );
    }

    /// The ECDSA path produces a recoverable signature that recovers to the
    /// registered group address — i.e. exactly what Solidity `ecrecover` checks.
    #[test]
    fn ecdsa_recovers_to_group_address() {
        let signer = ThresholdSigner::from_seeds([0x42; 32], [0x11; 32]).unwrap();
        // Destination EVM → ECDSA scheme selected.
        let message = CrossChainMessage::new(
            chain_id::encode(chain_id::FAMILY_SUI, 0).unwrap(),
            chain_id::encode(chain_id::FAMILY_EVM, 998).unwrap(),
            0,
            [0x11; 32],
            [0x22; 32],
            b"p".to_vec(),
        );
        let ds = derive_domain_sep(&TEST_SALT);
        let digest = message.digest(&ds);
        let env = signer.sign(&message, &ds, 1).unwrap();
        assert_eq!(env.scheme_tag, bridge_types::envelope::SCHEME_ECDSA_SECP256K1);
        assert_eq!(env.signature.len(), 65);
        let v = env.signature[64];
        assert!(v == 27 || v == 28);

        // Recover the signer address from (r, s, v) and compare to the group key.
        let recid = k256::ecdsa::RecoveryId::from_byte(v - 27).unwrap();
        let sig = k256::ecdsa::Signature::from_slice(&env.signature[..64]).unwrap();
        let vk = k256::ecdsa::VerifyingKey::recover_from_prehash(&digest, &sig, recid).unwrap();
        let point = vk.to_encoded_point(false);
        let hash = keccak256(&point.as_bytes()[1..]);
        assert_eq!(&hash[12..], signer.ecdsa_group_address());
    }

    #[test]
    fn rejects_unknown_destination_family() {
        let signer = ThresholdSigner::from_seeds([0x42; 32], [0x11; 32]).unwrap();
        // dst_chain_id with family bits = 3 (Solana) — no Sui/EVM verifier here.
        let message = CrossChainMessage::new(
            chain_id::encode(chain_id::FAMILY_EVM, 1).unwrap(),
            chain_id::encode(chain_id::FAMILY_SOLANA, 1).unwrap(),
            0,
            [0u8; 32],
            [0u8; 32],
            vec![],
        );
        let ds = derive_domain_sep(&TEST_SALT);
        assert!(matches!(
            signer.sign(&message, &ds, 1),
            Err(SignerError::UnsupportedDstFamily(chain_id::FAMILY_SOLANA))
        ));
    }
}
