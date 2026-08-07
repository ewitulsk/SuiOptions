//! Cross-language conformance fixtures (§5.2).
//!
//! The vectors produced here are asserted in the Rust test suite
//! (`tests/conformance.rs`) AND hard-coded into the Move test suite
//! (`contracts/exchange/tests/conformance_tests.move`). Regenerate with:
//! `cargo run --example gen_fixtures` — and if anything changes, that is a
//! consensus break: every outstanding signature dies with it.

use crate::keys::{Ed25519Keypair, Secp256k1Keypair};
use crate::{order_digest, personal_message_signing_digest};
use exchange_types::order::SignatureScheme;
use exchange_types::{Order, SuiAddress};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FixtureVector {
    pub name: String,
    pub order: Order,
    pub registry_id: SuiAddress,
    pub scheme: SignatureScheme,
    /// Address that must be derived from `public_key` (maker, or the
    /// delegated signer for the delegated case).
    pub signer_address: SuiAddress,
    pub order_bcs_hex: String,
    pub digest_hex: String,
    pub signing_digest_hex: String,
    pub public_key_hex: String,
    pub signature_hex: String,
}

pub fn registry_id() -> SuiAddress {
    SuiAddress::parse("0x5c01aabbccddeeff00112233445566778899aabbccddeeff0011223344556677")
        .unwrap()
}

fn base_order(maker: SuiAddress) -> Order {
    Order {
        maker_token:
            "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI".into(),
        taker_token:
            "0x00000000000000000000000000000000000000000000000000000000000000aa::usdc::USDC"
                .into(),
        maker_amount: 50_000_000_000,
        taker_amount: 125_000_000,
        max_fee_bps: 10,
        maker,
        maker_manager_id: SuiAddress::parse(
            "0x71aa000000000000000000000000000000000000000000000000000000000001",
        )
        .unwrap(),
        taker: SuiAddress::ZERO,
        sender: SuiAddress::ZERO,
        expiry_ms: 1_754_330_000_000,
        salt: 1_754_329_100_123,
    }
}

pub fn generate() -> Vec<FixtureVector> {
    let registry = registry_id();
    let mut out = Vec::new();

    // 1. ed25519, maker signs their own order.
    let ed = Ed25519Keypair::from_seed([7u8; 32]);
    let order = base_order(ed.address());
    out.push(make_vector("ed25519-maker", &order, registry, SignatureScheme::Ed25519,
        ed.address(), ed.public_key(), ed.sign_personal_message(&order_digest(&order, &registry).0)));

    // 2. secp256k1, maker signs their own order.
    let k1 = Secp256k1Keypair::from_seed([9u8; 32]);
    let order = base_order(k1.address());
    out.push(make_vector("secp256k1-maker", &order, registry, SignatureScheme::Secp256k1,
        k1.address(), k1.public_key(), k1.sign_personal_message(&order_digest(&order, &registry).0)));

    // 3. ed25519 delegated signer: maker is the [7;32] key's address, but the
    // [2;32] hot key signs. Valid only when the hot key is in the maker's
    // approved-signer set.
    let hot = Ed25519Keypair::from_seed([2u8; 32]);
    let order = base_order(ed.address());
    out.push(make_vector("ed25519-delegated", &order, registry, SignatureScheme::Ed25519,
        hot.address(), hot.public_key(), hot.sign_personal_message(&order_digest(&order, &registry).0)));

    out
}

fn make_vector(
    name: &str,
    order: &Order,
    registry: SuiAddress,
    scheme: SignatureScheme,
    signer_address: SuiAddress,
    public_key: Vec<u8>,
    signature: Vec<u8>,
) -> FixtureVector {
    let digest = order_digest(order, &registry);
    FixtureVector {
        name: name.to_string(),
        order: order.clone(),
        registry_id: registry,
        scheme,
        signer_address,
        order_bcs_hex: hex::encode(order.to_bcs()),
        digest_hex: hex::encode(digest.0),
        signing_digest_hex: hex::encode(personal_message_signing_digest(&digest.0)),
        public_key_hex: hex::encode(public_key),
        signature_hex: hex::encode(signature),
    }
}
