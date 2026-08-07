/// Order struct, BCS decoding, domain-separated hashing, and maker signature
/// verification (spec §4.1–4.3).
///
/// This module is the consensus-critical mirror of the Rust
/// `orderbook-signing` crate: the two MUST stay byte-for-byte identical, and
/// the cross-language conformance fixtures (tests/conformance_tests.move and
/// the crate's fixtures/conformance.json) are the guard.
module exchange::order;

use std::string::{Self, String};
use std::type_name;
use sui::address;
use sui::bcs;
use sui::ecdsa_k1;
use sui::ed25519;
use sui::hash;

// === Errors ===

const ETrailingBytes: u64 = 1;
const EBadSignatureLength: u64 = 2;
const EBadPublicKeyLength: u64 = 3;
const EUnsupportedScheme: u64 = 4;
const ENotLowS: u64 = 5;

// === Domain separation (§4.2) ===

/// Never reorder or insert `Order` fields, and never change the encoding,
/// without bumping this version: BCS layout is consensus-critical.
const DOMAIN_VERSION: u8 = 1;
const DOMAIN_TAG: vector<u8> = b"SUI_HYBRID_EXCHANGE_ORDER";

// === Signature schemes (§4.3) ===

/// Scheme prefix byte == Sui address-derivation flag.
const SCHEME_ED25519: u8 = 0x00;
const SCHEME_SECP256K1: u8 = 0x01;
/// `sui::ecdsa_k1` internal-hash selector for sha256.
const K1_HASH_SHA256: u8 = 1;

/// secp256k1 half curve order (big-endian). Low-s ⇔ s <= this.
const SECP256K1_HALF_ORDER: vector<u8> = vector[
    0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0x5d, 0x57, 0x6e, 0x73, 0x57, 0xa4, 0x50, 0x1d, 0xdf, 0xe9, 0x2f, 0x46, 0x68, 0x1b,
    0x20, 0xa0,
];

/// Sui personal-message intent: scope=PersonalMessage(3), version=V0, app=Sui.
const INTENT_PERSONAL_MESSAGE: vector<u8> = vector[0x03, 0x00, 0x00];

/// Mirror of 0x v4 `LimitOrder` (§4.1). Never stored on chain; reconstructed
/// from `order_bytes` inside the fill transaction.
///
/// Field order is consensus-critical: BCS encoding depends on it.
public struct Order has copy, drop {
    // -- economic terms --
    maker_token: String,
    taker_token: String,
    maker_amount: u64,
    taker_amount: u64,
    /// Maker's signed CEILING on the fee rate applied to their received
    /// amount; settlement charges `min(this, registry fee)` (§4.6 step 10).
    max_fee_bps: u64,
    // -- parties & permissions --
    maker: address,
    maker_manager_id: ID,
    /// @0x0 = any taker may fill.
    taker: address,
    /// @0x0 = any tx sender may submit; else restricted (relayer mode).
    sender: address,
    // -- validity --
    expiry_ms: u64,
    /// Uniqueness + bulk-cancel watermark; monotonic per maker per market.
    salt: u64,
}

// === Decoding ===

/// Deserialize an `Order` from its exact BCS bytes. Aborts on trailing bytes
/// so a given order has a single accepted encoding.
public fun from_bytes(bytes: vector<u8>): Order {
    let mut r = bcs::new(bytes);
    let maker_token = string::utf8(r.peel_vec_u8());
    let taker_token = string::utf8(r.peel_vec_u8());
    let maker_amount = r.peel_u64();
    let taker_amount = r.peel_u64();
    let max_fee_bps = r.peel_u64();
    let maker = r.peel_address();
    let maker_manager_id = r.peel_address().to_id();
    let taker = r.peel_address();
    let sender = r.peel_address();
    let expiry_ms = r.peel_u64();
    let salt = r.peel_u64();
    assert!(r.into_remainder_bytes().is_empty(), ETrailingBytes);
    Order {
        maker_token,
        taker_token,
        maker_amount,
        taker_amount,
        max_fee_bps,
        maker,
        maker_manager_id,
        taker,
        sender,
        expiry_ms,
        salt,
    }
}

// === Hashing (§4.2) ===

/// Domain-separated order digest: `blake2b256(TAG ‖ VERSION ‖ bcs(registry_id)
/// ‖ bcs(order))`. The registry object ID simultaneously prevents
/// cross-market, cross-deployment and cross-network replay. The digest is
/// always computed over the canonical re-serialization of the struct.
public fun digest(order: &Order, registry_id: ID): vector<u8> {
    let mut buf = DOMAIN_TAG;
    buf.push_back(DOMAIN_VERSION);
    buf.append(bcs::to_bytes(&registry_id));
    buf.append(bcs::to_bytes(order));
    hash::blake2b256(&buf)
}

// === Signature verification (§4.3) ===

/// Verify a maker signature over an order digest.
///
/// `signature` is scheme-prefixed: `[flag] ‖ raw 64-byte signature`.
/// `public_key` is 32 bytes (ed25519) or 33 compressed bytes (secp256k1).
///
/// Returns `(valid, derived_signer_address)`. The caller decides whether the
/// derived address is authorized (the maker itself, or a delegated signer on
/// the maker's BalanceManager).
///
/// Byte recipe (identical to Sui wallets' `signPersonalMessage`): every
/// scheme signs `signing_digest = blake2b256(intent ‖ bcs(digest))`. Ed25519
/// verifies over that digest directly; the secp256k1 native additionally
/// sha256-hashes its message internally, matching fastcrypto's ECDSA signer.
public fun verify_signature(
    digest: &vector<u8>,
    signature: &vector<u8>,
    public_key: &vector<u8>,
): (bool, address) {
    assert!(signature.length() == 65, EBadSignatureLength);
    let scheme = signature[0];
    let mut sig = vector[];
    let mut i = 1;
    while (i < 65) {
        sig.push_back(signature[i]);
        i = i + 1;
    };

    let signing_digest = personal_message_digest(digest);

    if (scheme == SCHEME_ED25519) {
        assert!(public_key.length() == 32, EBadPublicKeyLength);
        let ok = ed25519::ed25519_verify(&sig, public_key, &signing_digest);
        (ok, derive_address(scheme, public_key))
    } else if (scheme == SCHEME_SECP256K1) {
        assert!(public_key.length() == 33, EBadPublicKeyLength);
        // Canonical-form guard: ECDSA's malleable twin can't double-fill
        // (accounting keys on the digest) but low-s keeps dedup/audit sane.
        assert!(is_low_s(&sig), ENotLowS);
        let ok = ecdsa_k1::secp256k1_verify(&sig, public_key, &signing_digest, K1_HASH_SHA256);
        (ok, derive_address(scheme, public_key))
    } else {
        abort EUnsupportedScheme
    }
}

/// `blake2b256(intent ‖ bcs(message as vector<u8>))` — the 32-byte payload a
/// Sui wallet actually signs for a personal message.
fun personal_message_digest(message: &vector<u8>): vector<u8> {
    let mut buf = INTENT_PERSONAL_MESSAGE;
    buf.append(bcs::to_bytes(message));
    hash::blake2b256(&buf)
}

/// Sui address derivation: `blake2b256(flag ‖ pk)`.
fun derive_address(flag: u8, public_key: &vector<u8>): address {
    let mut pre = vector[flag];
    pre.append(*public_key);
    address::from_bytes(hash::blake2b256(&pre))
}

/// Big-endian lexicographic compare of the s half against n/2.
fun is_low_s(sig64: &vector<u8>): bool {
    let half = SECP256K1_HALF_ORDER;
    let mut i = 0;
    while (i < 32) {
        let s_byte = sig64[32 + i];
        let h_byte = half[i];
        if (s_byte < h_byte) return true;
        if (s_byte > h_byte) return false;
        i = i + 1;
    };
    true // equal
}

// === Canonical type strings ===

/// The canonical coin type string committed into signed orders:
/// `0x` + full 64-hex original address + `::module::Name`.
///
/// Original (pre-upgrade) IDs are used so package upgrades of a coin type
/// never change the string makers sign.
public fun canonical_type<T>(): String {
    let tn = type_name::with_original_ids<T>();
    let mut s = string::utf8(b"0x");
    s.append(string::from_ascii(tn.into_string()));
    s
}

// === Field accessors ===

public fun maker_token(o: &Order): &String { &o.maker_token }
public fun taker_token(o: &Order): &String { &o.taker_token }
public fun maker_amount(o: &Order): u64 { o.maker_amount }
public fun taker_amount(o: &Order): u64 { o.taker_amount }
public fun max_fee_bps(o: &Order): u64 { o.max_fee_bps }
public fun maker(o: &Order): address { o.maker }
public fun maker_manager_id(o: &Order): ID { o.maker_manager_id }
public fun taker(o: &Order): address { o.taker }
public fun sender(o: &Order): address { o.sender }
public fun expiry_ms(o: &Order): u64 { o.expiry_ms }
public fun salt(o: &Order): u64 { o.salt }

// === Test helpers ===

#[test_only]
public fun new_for_testing(
    maker_token: String,
    taker_token: String,
    maker_amount: u64,
    taker_amount: u64,
    max_fee_bps: u64,
    maker: address,
    maker_manager_id: ID,
    taker: address,
    sender: address,
    expiry_ms: u64,
    salt: u64,
): Order {
    Order {
        maker_token,
        taker_token,
        maker_amount,
        taker_amount,
        max_fee_bps,
        maker,
        maker_manager_id,
        taker,
        sender,
        expiry_ms,
        salt,
    }
}

#[test_only]
public fun to_bytes(o: &Order): vector<u8> { bcs::to_bytes(o) }
