module options_core::quote;

use std::bcs;
use std::string::String;
use sui::clock::Clock;
use sui::ecdsa_k1;
use sui::ecdsa_r1;
use sui::ed25519;

use options_core::admin::{Self, ProtocolConfig};
use options_core::errors;
use options_core::quote_signer::{Self, QuoteSigner};

const SCHEME_ED25519: u8 = 0;
const SCHEME_SECP256K1: u8 = 1;
const SCHEME_SECP256R1: u8 = 2;

/// Hash flag for `ecdsa_k1::secp256k1_verify` /
/// `ecdsa_r1::secp256r1_verify`. `1` = SHA-256. The MM bot must hash the
/// BCS-encoded quote with SHA-256 before signing for the k1/r1 paths.
const ECDSA_HASH_SHA256: u8 = 1;

/// The signed payload. The full collateral routing — the source object AND
/// the package/module whose `release` debits it — is INSIDE the signature,
/// so no intermediary can substitute an MM's routing. BCS field order is
/// normative for off-chain signers (spec §4.1).
public struct Quote has copy, drop, store {
    protocol_id: vector<u8>,
    /// The `QuoteSigner` whose key + nonce table authorize this quote.
    signer_id: ID,
    /// The collateral object `release()` debits.
    collateral_source: ID,
    /// Package + module containing the standardized `release` function.
    release_package: address,
    release_module: String,
    signer_token_recipient: address,
    bucket_id: ID,
    write_amount: u64,
    premium: u64,
    valid_until_ms: u64,
    nonce: u64,
}

public struct SignedQuote has copy, drop, store {
    quote: Quote,
    signature: vector<u8>,
}

public fun new_quote(
    protocol_id: vector<u8>,
    signer_id: ID,
    collateral_source: ID,
    release_package: address,
    release_module: String,
    signer_token_recipient: address,
    bucket_id: ID,
    write_amount: u64,
    premium: u64,
    valid_until_ms: u64,
    nonce: u64,
): Quote {
    Quote {
        protocol_id,
        signer_id,
        collateral_source,
        release_package,
        release_module,
        signer_token_recipient,
        bucket_id,
        write_amount,
        premium,
        valid_until_ms,
        nonce,
    }
}

public fun new_signed_quote(quote: Quote, signature: vector<u8>): SignedQuote {
    SignedQuote { quote, signature }
}

public fun quote(sq: &SignedQuote): &Quote { &sq.quote }
public fun signature(sq: &SignedQuote): &vector<u8> { &sq.signature }

public fun protocol_id(q: &Quote): &vector<u8> { &q.protocol_id }
public fun signer_id(q: &Quote): ID { q.signer_id }
public fun collateral_source(q: &Quote): ID { q.collateral_source }
public fun release_package(q: &Quote): address { q.release_package }
public fun release_module(q: &Quote): &String { &q.release_module }
public fun signer_token_recipient(q: &Quote): address { q.signer_token_recipient }
public fun bucket_id(q: &Quote): ID { q.bucket_id }
public fun write_amount(q: &Quote): u64 { q.write_amount }
public fun premium(q: &Quote): u64 { q.premium }
public fun valid_until_ms(q: &Quote): u64 { q.valid_until_ms }
public fun nonce(q: &Quote): u64 { q.nonce }

public(package) fun verify_and_consume_quote(
    signer: &mut QuoteSigner,
    config: &ProtocolConfig,
    signed_quote: &SignedQuote,
    clock: &Clock,
): Quote {
    let q = signed_quote.quote;
    check_non_signature_fields(&q, signer, config, clock);

    let msg = bcs::to_bytes(&q);
    let scheme = quote_signer::signing_scheme(signer);
    let pubkey = quote_signer::signing_pubkey(signer);
    let sig = &signed_quote.signature;
    let valid = if (scheme == SCHEME_ED25519) {
        ed25519::ed25519_verify(sig, pubkey, &msg)
    } else if (scheme == SCHEME_SECP256K1) {
        ecdsa_k1::secp256k1_verify(sig, pubkey, &msg, ECDSA_HASH_SHA256)
    } else if (scheme == SCHEME_SECP256R1) {
        ecdsa_r1::secp256r1_verify(sig, pubkey, &msg, ECDSA_HASH_SHA256)
    } else {
        abort errors::invalid_signing_scheme()
    };
    assert!(valid, errors::quote_signature_invalid());

    quote_signer::consume_nonce(signer, q.nonce, q.valid_until_ms);

    q
}

fun check_non_signature_fields(
    q: &Quote,
    signer: &QuoteSigner,
    config: &ProtocolConfig,
    clock: &Clock,
) {
    assert!(&q.protocol_id == admin::protocol_id(config), errors::quote_protocol_mismatch());
    assert!(q.signer_id == quote_signer::signer_id(signer), errors::quote_account_mismatch());
    assert!(clock.timestamp_ms() < q.valid_until_ms, errors::quote_expired());
    assert!(!quote_signer::has_nonce(signer, q.nonce), errors::quote_nonce_used());
}

#[test_only]
public(package) fun verify_skip_sig(
    signer: &mut QuoteSigner,
    config: &ProtocolConfig,
    signed_quote: &SignedQuote,
    clock: &Clock,
): Quote {
    let q = signed_quote.quote;
    check_non_signature_fields(&q, signer, config, clock);
    quote_signer::consume_nonce(signer, q.nonce, q.valid_until_ms);
    q
}
