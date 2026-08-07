//! The consensus-critical mirror of `exchange::order` (Move).
//!
//! Re-implements order hashing (§4.2) and signature verification (§4.3)
//! byte-for-byte. Any drift between this crate and `order.move` breaks every
//! signature in the system; the conformance fixtures in `fixtures/` are
//! asserted by BOTH the Rust and the Move test suites and are release-blocking.
//!
//! One deliberate deviation from spec draft v0.3 §4.3's table: the secp256k1
//! path verifies over `blake2b256(intent ‖ bcs(digest))` (with the native's
//! internal sha256 on top), not over the raw intent preimage. This matches
//! what Sui wallets/fastcrypto actually produce for personal messages: every
//! scheme signs the 32-byte blake2b intent digest, and the secp256k1 signer
//! then sha256-hashes that digest internally (k256/fastcrypto behavior). The
//! Move side implements the identical recipe.

use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest as _};
use ed25519_dalek::Verifier as _;
use k256::ecdsa::signature::hazmat::PrehashVerifier as _;
use exchange_types::order::SignatureScheme;
use exchange_types::{Digest, ObjectId, Order, SuiAddress};
use sha2::Sha256;

pub mod fixtures;
pub mod keys;

type Blake2b256 = Blake2b<U32>;

/// Domain separation, mirrored from `order.move`.
pub const DOMAIN_TAG: &[u8] = b"SUI_HYBRID_EXCHANGE_ORDER";
pub const DOMAIN_VERSION: u8 = 1;

/// Sui intent bytes for a personal message: scope=PersonalMessage(3),
/// version=V0(0), app=Sui(0).
pub const PERSONAL_MESSAGE_INTENT: [u8; 3] = [0x03, 0x00, 0x00];
/// Sui intent bytes for transaction data (used by the settlement submitter).
pub const TRANSACTION_DATA_INTENT: [u8; 3] = [0x00, 0x00, 0x00];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VerifyError {
    #[error("signature must be {expected} bytes, got {got}")]
    BadSignatureLength { expected: usize, got: usize },
    #[error("public key must be {expected} bytes, got {got}")]
    BadPublicKeyLength { expected: usize, got: usize },
    #[error("malformed public key")]
    MalformedPublicKey,
    #[error("secp256k1 signature is not low-s normalized")]
    HighS,
    #[error("signature verification failed")]
    Invalid,
    #[error("derived signer address {derived} is not the maker or an approved signer")]
    SignerNotAuthorized { derived: SuiAddress },
}

pub fn blake2b256(data: &[u8]) -> [u8; 32] {
    let mut h = Blake2b256::new();
    h.update(data);
    h.finalize().into()
}

/// §4.2 — the order digest: `blake2b256(TAG ‖ VERSION ‖ bcs(registry_id) ‖ bcs(order))`.
/// The registry object ID binds the order to one market on one deployment on
/// one network (the `chainId`+`verifyingContract` role in 0x).
pub fn order_digest(order: &Order, registry_id: &ObjectId) -> Digest {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(DOMAIN_TAG);
    buf.push(DOMAIN_VERSION);
    buf.extend_from_slice(&bcs::to_bytes(registry_id).unwrap());
    buf.extend_from_slice(&order.to_bcs());
    Digest(blake2b256(&buf))
}

/// The 32-byte digest a Sui wallet actually signs for `signPersonalMessage`:
/// `blake2b256(intent ‖ bcs(message))` where message is the order digest as a
/// `vector<u8>` (so BCS prepends the ULEB length, 0x20).
pub fn personal_message_signing_digest(message: &[u8]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(3 + 1 + message.len());
    buf.extend_from_slice(&PERSONAL_MESSAGE_INTENT);
    buf.extend_from_slice(&bcs::to_bytes(&message.to_vec()).unwrap());
    blake2b256(&buf)
}

/// Derive the Sui address for a public key under the given scheme:
/// `blake2b256(flag ‖ pk)`.
pub fn derive_address(scheme: SignatureScheme, public_key: &[u8]) -> SuiAddress {
    let mut buf = Vec::with_capacity(1 + public_key.len());
    buf.push(scheme.flag());
    buf.extend_from_slice(public_key);
    SuiAddress(blake2b256(&buf))
}

/// secp256k1 half curve order: low-s means `s <= n/2` (byte-lexicographic
/// compare on the 32-byte big-endian s works).
const SECP256K1_HALF_ORDER: [u8; 32] = [
    0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0x5d, 0x57, 0x6e, 0x73, 0x57, 0xa4, 0x50, 0x1d, 0xdf, 0xe9, 0x2f, 0x46, 0x68, 0x1b,
    0x20, 0xa0,
];

pub fn is_low_s(sig64: &[u8]) -> bool {
    sig64.len() == 64 && sig64[32..64] <= SECP256K1_HALF_ORDER[..]
}

/// Verify a raw (un-prefixed) 64-byte signature over `message` (the order
/// digest) for `public_key` under `scheme`. Returns the derived signer
/// address on success; the caller checks it against
/// `{maker} ∪ approved_signers` (delegated order signers, §4.3).
pub fn verify_signature(
    scheme: SignatureScheme,
    message: &[u8],
    signature: &[u8],
    public_key: &[u8],
) -> Result<SuiAddress, VerifyError> {
    if signature.len() != 64 {
        return Err(VerifyError::BadSignatureLength {
            expected: 64,
            got: signature.len(),
        });
    }
    let signing_digest = personal_message_signing_digest(message);
    match scheme {
        SignatureScheme::Ed25519 => {
            if public_key.len() != 32 {
                return Err(VerifyError::BadPublicKeyLength {
                    expected: 32,
                    got: public_key.len(),
                });
            }
            let vk = ed25519_dalek::VerifyingKey::from_bytes(
                public_key.try_into().unwrap(),
            )
            .map_err(|_| VerifyError::MalformedPublicKey)?;
            let sig = ed25519_dalek::Signature::from_bytes(signature.try_into().unwrap());
            vk.verify(&signing_digest, &sig)
                .map_err(|_| VerifyError::Invalid)?;
        }
        SignatureScheme::Secp256k1 => {
            if public_key.len() != 33 {
                return Err(VerifyError::BadPublicKeyLength {
                    expected: 33,
                    got: public_key.len(),
                });
            }
            if !is_low_s(signature) {
                return Err(VerifyError::HighS);
            }
            let vk = k256::ecdsa::VerifyingKey::from_sec1_bytes(public_key)
                .map_err(|_| VerifyError::MalformedPublicKey)?;
            let sig = k256::ecdsa::Signature::from_slice(signature)
                .map_err(|_| VerifyError::Invalid)?;
            // Wallets sign sha256(blake2b_intent_digest); the on-chain native
            // does the same sha256 internally over the passed message.
            let prehash = Sha256::digest(signing_digest);
            vk.verify_prehash(&prehash, &sig)
                .map_err(|_| VerifyError::Invalid)?;
        }
    }
    Ok(derive_address(scheme, public_key))
}

/// Full order-signature check as the chain performs it: verify the signature
/// over the domain-separated digest and require the derived address to be the
/// maker or one of the maker's approved delegated signers.
pub fn verify_order(
    order: &Order,
    registry_id: &ObjectId,
    scheme: SignatureScheme,
    signature: &[u8],
    public_key: &[u8],
    approved_signers: &[SuiAddress],
) -> Result<Digest, VerifyError> {
    let digest = order_digest(order, registry_id);
    let signer = verify_signature(scheme, &digest.0, signature, public_key)?;
    if signer != order.maker && !approved_signers.contains(&signer) {
        return Err(VerifyError::SignerNotAuthorized { derived: signer });
    }
    Ok(digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{Ed25519Keypair, Secp256k1Keypair};

    fn sample_order(maker: SuiAddress) -> Order {
        Order {
            maker_token:
                "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI"
                    .into(),
            taker_token:
                "0x00000000000000000000000000000000000000000000000000000000000000aa::usdc::USDC"
                    .into(),
            maker_amount: 50_000_000_000,
            taker_amount: 125_000_000,
            max_fee_bps: 10,
            maker,
            maker_manager_id: SuiAddress::parse("0x71").unwrap(),
            taker: SuiAddress::ZERO,
            sender: SuiAddress::ZERO,
            expiry_ms: 1_754_330_000_000,
            salt: 1_754_329_100_123,
        }
    }

    #[test]
    fn ed25519_roundtrip() {
        let kp = Ed25519Keypair::from_seed([7u8; 32]);
        let order = sample_order(kp.address());
        let registry = SuiAddress::parse("0x5c").unwrap();
        let digest = order_digest(&order, &registry);
        let sig = kp.sign_personal_message(&digest.0);
        let got =
            verify_order(&order, &registry, SignatureScheme::Ed25519, &sig, &kp.public_key(), &[])
                .unwrap();
        assert_eq!(got, digest);

        // wrong registry => different digest => verification fails
        let other = SuiAddress::parse("0x5d").unwrap();
        assert!(verify_order(
            &order,
            &other,
            SignatureScheme::Ed25519,
            &sig,
            &kp.public_key(),
            &[]
        )
        .is_err());
    }

    #[test]
    fn ed25519_delegated_signer() {
        let owner = Ed25519Keypair::from_seed([1u8; 32]);
        let hot = Ed25519Keypair::from_seed([2u8; 32]);
        let order = sample_order(owner.address());
        let registry = SuiAddress::parse("0x5c").unwrap();
        let digest = order_digest(&order, &registry);
        let sig = hot.sign_personal_message(&digest.0);

        // not approved => rejected
        assert_eq!(
            verify_order(&order, &registry, SignatureScheme::Ed25519, &sig, &hot.public_key(), &[]),
            Err(VerifyError::SignerNotAuthorized { derived: hot.address() })
        );
        // approved => ok
        verify_order(
            &order,
            &registry,
            SignatureScheme::Ed25519,
            &sig,
            &hot.public_key(),
            &[hot.address()],
        )
        .unwrap();
    }

    #[test]
    fn secp256k1_roundtrip_and_low_s() {
        let kp = Secp256k1Keypair::from_seed([9u8; 32]);
        let order = sample_order(kp.address());
        let registry = SuiAddress::parse("0x5c").unwrap();
        let digest = order_digest(&order, &registry);
        let sig = kp.sign_personal_message(&digest.0);
        assert!(is_low_s(&sig), "signer must emit low-s");
        verify_order(
            &order,
            &registry,
            SignatureScheme::Secp256k1,
            &sig,
            &kp.public_key(),
            &[],
        )
        .unwrap();

        // Forge the malleable twin (s' = n - s): must be rejected as high-s.
        let mut high = sig.clone();
        let parsed = k256::ecdsa::Signature::from_slice(&sig).unwrap();
        let s: k256::Scalar = *parsed.s().as_ref();
        let neg = -s;
        high[32..64].copy_from_slice(&neg.to_bytes());
        assert_eq!(
            verify_order(
                &order,
                &registry,
                SignatureScheme::Secp256k1,
                &high,
                &kp.public_key(),
                &[]
            ),
            Err(VerifyError::HighS)
        );
    }

    #[test]
    fn tampered_order_fails() {
        let kp = Ed25519Keypair::from_seed([7u8; 32]);
        let mut order = sample_order(kp.address());
        let registry = SuiAddress::parse("0x5c").unwrap();
        let sig = kp.sign_personal_message(&order_digest(&order, &registry).0);
        order.taker_amount -= 1; // better price for taker
        assert_eq!(
            verify_order(&order, &registry, SignatureScheme::Ed25519, &sig, &kp.public_key(), &[]),
            Err(VerifyError::Invalid)
        );
    }
}
