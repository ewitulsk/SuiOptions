//! Order signing with the bot's Sui wallet key.
//!
//! The same ed25519 key owns the `BalanceManager`, pays gas, and signs
//! orders — no delegated-signer split on staging. Signatures follow the
//! wallet `signPersonalMessage` recipe re-implemented by `exchange-signing`;
//! the digest recipe is consensus-critical and covered by that crate's
//! conformance fixtures.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use exchange_signing::keys::Ed25519Keypair;
use exchange_signing::order_digest;
use exchange_types::order::{SignatureScheme, SignedOrder};
use exchange_types::{Digest, ObjectId, Order, SuiAddress};

/// Domain prefix for the orderbook's signed soft-cancel payload — mirrors
/// `handlers::CANCEL_DOMAIN_TAG` in the orderbook service.
pub const CANCEL_DOMAIN_TAG: &[u8] = b"SUI_HYBRID_EXCHANGE_CANCEL";

pub struct OrderSigner {
    kp: Ed25519Keypair,
    address: SuiAddress,
    public_key: Vec<u8>,
}

impl OrderSigner {
    /// Build from a Sui bech32 keypair export (`suiprivkey1…`). Fails closed
    /// on any non-ed25519 key: the exchange accepts secp256k1 too, but this
    /// bot's one-key design ties order signing to the gas/owner key, which
    /// the rest of the stack requires to be ed25519.
    pub fn from_sui_bech32(raw: &str) -> Result<Self> {
        use sui_types::crypto::EncodeDecodeBase64 as _;
        let raw = raw.trim();
        let kp = sui_types::crypto::SuiKeyPair::decode(raw)
            .map_err(|e| anyhow!("decoding suiprivkey bech32 key: {e}"))?;
        // `encode_base64()` yields `base64(flag || 32-byte secret)` for every
        // variant (same extraction as sui-tx's QuoteSigner).
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(kp.encode_base64())
            .context("base64-decoding suiprivkey re-encoding")?;
        if bytes.len() != 33 {
            return Err(anyhow!(
                "suiprivkey decoded to {} bytes, expected 33 (1 flag + 32 secret)",
                bytes.len()
            ));
        }
        if bytes[0] != 0x00 {
            return Err(anyhow!(
                "sui key has scheme flag {:#04x}; staging-mm-bot requires an ed25519 key",
                bytes[0]
            ));
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&bytes[1..33]);
        let kp = Ed25519Keypair::from_seed(seed);
        let address = kp.address();
        let public_key = kp.public_key();
        Ok(Self { kp, address, public_key })
    }

    /// The maker address (identical bytes to the Sui wallet address).
    pub fn address(&self) -> SuiAddress {
        self.address
    }

    /// Sign an order for one market: digest, personal-message signature,
    /// wire-ready `SignedOrder`.
    pub fn sign_order(&self, order: Order, registry_id: ObjectId) -> (Digest, SignedOrder) {
        let digest = order_digest(&order, &registry_id);
        let signature = self.kp.sign_personal_message(&digest.0);
        let signed = SignedOrder {
            order,
            registry_id,
            scheme: SignatureScheme::Ed25519,
            signature,
            public_key: self.public_key.clone(),
        };
        (digest, signed)
    }

    /// Signature over the soft-cancel payload `TAG ‖ digest_bytes`, plus the
    /// public key, both base64 as `DELETE /v1/orders/{digest}` expects.
    pub fn sign_cancel(&self, digest: &Digest) -> (String, String) {
        let mut message = CANCEL_DOMAIN_TAG.to_vec();
        message.extend_from_slice(&digest.0);
        let sig = self.kp.sign_personal_message(&message);
        let b64 = base64::engine::general_purpose::STANDARD;
        (b64.encode(sig), b64.encode(&self.public_key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange_signing::verify_order;

    // A throwaway devnet-style key generated for tests only.
    const TEST_KEY: [u8; 32] = [7u8; 32];

    /// Build a `suiprivkey1…` string without fastcrypto internals: base64
    /// the flag-prefixed secret, decode it into a `SuiKeyPair`, and ask the
    /// keypair to bech32-encode itself (same trick as sui-tx's QuoteSigner
    /// tests).
    fn build_suiprivkey(flag: u8, secret: &[u8; 32]) -> String {
        use sui_types::crypto::EncodeDecodeBase64 as _;
        let mut bytes = vec![flag];
        bytes.extend_from_slice(secret);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let kp = sui_types::crypto::SuiKeyPair::decode_base64(&b64).expect("base64 round-trip");
        kp.encode().expect("bech32 encode")
    }

    fn bech32_of(seed: [u8; 32]) -> String {
        build_suiprivkey(0, &seed)
    }

    fn sample_order(maker: SuiAddress, manager: ObjectId) -> Order {
        Order {
            maker_token:
                "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI"
                    .into(),
            taker_token:
                "0x00000000000000000000000000000000000000000000000000000000000000aa::usdc::USDC"
                    .into(),
            maker_amount: 50_000_000_000,
            taker_amount: 125_000_000,
            max_fee_bps: 50,
            maker,
            maker_manager_id: manager,
            taker: SuiAddress::ZERO,
            sender: SuiAddress::ZERO,
            expiry_ms: 1_754_330_000_000,
            salt: 1_754_329_100_123,
        }
    }

    #[test]
    fn bech32_roundtrip_matches_sui_address() {
        let raw = bech32_of(TEST_KEY);
        let signer = OrderSigner::from_sui_bech32(&raw).unwrap();
        // The derived address must equal what sui-types derives for the key.
        let kp = sui_types::crypto::SuiKeyPair::decode(&raw).unwrap();
        let sui_addr = sui_types::base_types::SuiAddress::from(&kp.public());
        assert_eq!(signer.address().to_hex(), sui_addr.to_string());
    }

    #[test]
    fn signed_orders_verify_as_the_chain_would() {
        let signer = OrderSigner::from_sui_bech32(&bech32_of(TEST_KEY)).unwrap();
        let manager = SuiAddress::parse("0x71").unwrap();
        let registry = SuiAddress::parse("0x5c").unwrap();
        let order = sample_order(signer.address(), manager);
        let (digest, signed) = signer.sign_order(order.clone(), registry);
        let got = verify_order(
            &order,
            &registry,
            SignatureScheme::Ed25519,
            &signed.signature,
            &signed.public_key,
            &[],
        )
        .unwrap();
        assert_eq!(got, digest);
    }

    #[test]
    fn rejects_non_ed25519_keys() {
        // A secp256k1 bech32 key must be refused, not silently accepted.
        let raw = build_suiprivkey(1, &[9u8; 32]);
        assert!(OrderSigner::from_sui_bech32(&raw).is_err());
    }
}
