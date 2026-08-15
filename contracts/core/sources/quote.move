module options_core::quote;

use std::bcs;
use std::string::String;
use std::type_name;
use sui::clock::Clock;
use sui::ecdsa_k1;
use sui::ecdsa_r1;
use sui::ed25519;

use options_core::admin::{Self, ProtocolConfig};
use options_core::bucket_registry::{Self, BucketKey};
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
///
/// # Why the spec and not a bucket id
///
/// A quote names the bucket's ECONOMICS, not an object. Buckets are created
/// just-in-time — the transaction that fills a quote may be the one that
/// brings its bucket into existence — so binding an object id would make a
/// fresh strike unquotable until someone had paid to create it.
///
/// Binding the spec is safe because `bucket_registry` admits exactly one
/// bucket per `BucketKey` (`derived_object::claim` aborts on a second claim,
/// and the AdminCap creators that could have minted a duplicate are gone).
/// One spec, one object: the signer cannot be redirected to a bucket with
/// different economics, or to a second bucket with the same ones and a
/// different exercise queue.
///
/// Note there is no `Call`/`Put` coin type in the spec, and none is needed:
/// `create_*_any_strike` pins the coin type to `(U, S, expiry, sig, exp)`
/// through `option_coin::register_*`'s encoding assert, so a matching spec
/// determines the coin type.
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
    /// The bucket's full economic identity — the SAME key the registry
    /// derives the bucket's address from, so the two can never drift.
    spec: BucketKey,
    /// Assignment-risk bound: refuse the fill if this much size is already
    /// written ahead of it. `max_u128` opts out. The exercise cursor walks
    /// `total_written` in write order, so a signer's assignment probability
    /// is a function of how much sits ahead — this lets the quote say so
    /// instead of implying it through an object id.
    max_total_written: u128,
    write_amount: u64,
    premium: u64,
    valid_until_ms: u64,
    nonce: u64,
}

public struct SignedQuote has copy, drop, store {
    quote: Quote,
    signature: vector<u8>,
}

/// Build a quote for the `(U, S)` pair.
///
/// Generic in the pair because `TypeName` has no constructor from a string —
/// `type_name::with_defining_ids` is a native generic, and a struct is not a
/// valid PTB pure argument — so the asset and settlement types can only enter
/// the payload by being named as type arguments. That is also what makes them
/// unforgeable: a caller who passes different type arguments than the signer
/// used produces different BCS bytes, and signature verification fails.
///
/// `strike_sig`/`strike_exp` are the NORMALIZED strike (trailing zeros
/// stripped, real ratio `sig / 10^exp`) — the same form
/// `option_coin::normalize_strike` produces and the bucket stores, so
/// equivalent raw strike encodings collapse to one quote.
public fun new_quote<U, S>(
    protocol_id: vector<u8>,
    signer_id: ID,
    collateral_source: ID,
    release_package: address,
    release_module: String,
    signer_token_recipient: address,
    expiry_ms: u64,
    strike_sig: u64,
    strike_exp: u8,
    is_put: bool,
    max_total_written: u128,
    write_amount: u64,
    premium: u64,
    valid_until_ms: u64,
    nonce: u64,
): Quote {
    let spec = bucket_registry::key(
        type_name::with_defining_ids<U>(),
        type_name::with_defining_ids<S>(),
        expiry_ms,
        strike_sig,
        strike_exp,
        is_put,
    );
    Quote {
        protocol_id,
        signer_id,
        collateral_source,
        release_package,
        release_module,
        signer_token_recipient,
        spec,
        max_total_written,
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
public fun spec(q: &Quote): &BucketKey { &q.spec }
public fun max_total_written(q: &Quote): u128 { q.max_total_written }
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
