/// The quote-authorization half of what used to be `Account`: a registered
/// signing key + the consumed-nonce table, and nothing else. Collateral
/// custody lives OUTSIDE core — any package implementing the standardized
/// `release` interface against `collateral::CollateralRequest` (see
/// docs/audit-restructure/04-collateral-abstraction-plan.md). Core holds
/// no market-maker funds.
module options_core::quote_signer;

use sui::clock::Clock;
use sui::dynamic_field as df;

use options_core::errors;
use options_core::events;

/// Signature scheme tags intentionally match `quote::SCHEME_*` so we never
/// store one scheme and dispatch on another.
public struct QuoteSigner has key {
    id: UID,
    owner: address,
    signing_scheme: u8,
    signing_pubkey: vector<u8>,
}

const SCHEME_ED25519: u8 = 0;
const SCHEME_SECP256K1: u8 = 1;
const SCHEME_SECP256R1: u8 = 2;

const ED25519_PUBKEY_LEN: u64 = 32;
const SECP256_COMPRESSED_PUBKEY_LEN: u64 = 33;

fun assert_scheme_pubkey(scheme: u8, pubkey: &vector<u8>) {
    let expected_len = if (scheme == SCHEME_ED25519) {
        ED25519_PUBKEY_LEN
    } else if (scheme == SCHEME_SECP256K1 || scheme == SCHEME_SECP256R1) {
        SECP256_COMPRESSED_PUBKEY_LEN
    } else {
        abort errors::invalid_signing_scheme()
    };
    assert!(pubkey.length() == expected_len, errors::invalid_pubkey_length());
}

public struct NonceKey has copy, drop, store {
    nonce: u64,
}

public fun create_signer(
    signing_scheme: u8,
    signing_pubkey: vector<u8>,
    ctx: &mut TxContext,
): QuoteSigner {
    assert_scheme_pubkey(signing_scheme, &signing_pubkey);
    let owner = ctx.sender();
    let signer = QuoteSigner {
        id: object::new(ctx),
        owner,
        signing_scheme,
        signing_pubkey,
    };
    events::emit_signer_created(
        object::id(&signer),
        owner,
        signing_scheme,
        signing_pubkey,
    );
    signer
}

public fun create_and_share_signer(
    signing_scheme: u8,
    signing_pubkey: vector<u8>,
    ctx: &mut TxContext,
) {
    let signer = create_signer(signing_scheme, signing_pubkey, ctx);
    transfer::share_object(signer);
}

public fun set_quote_signing_key(
    signer: &mut QuoteSigner,
    new_scheme: u8,
    new_pubkey: vector<u8>,
    ctx: &mut TxContext,
) {
    assert!(ctx.sender() == signer.owner, errors::not_owner());
    assert_scheme_pubkey(new_scheme, &new_pubkey);
    signer.signing_scheme = new_scheme;
    signer.signing_pubkey = new_pubkey;
    events::emit_signing_key_rotated(object::id(signer), new_scheme, new_pubkey);
}

public(package) fun has_nonce(signer: &QuoteSigner, nonce: u64): bool {
    df::exists(&signer.id, NonceKey { nonce })
}

public(package) fun consume_nonce(signer: &mut QuoteSigner, nonce: u64, valid_until_ms: u64) {
    df::add(&mut signer.id, NonceKey { nonce }, valid_until_ms);
}

public fun prune_nonce(signer: &mut QuoteSigner, nonce: u64, clock: &Clock) {
    let key = NonceKey { nonce };
    if (!df::exists(&signer.id, key)) {
        return
    };
    let valid_until_ms: u64 = df::remove(&mut signer.id, key);
    assert!(clock.timestamp_ms() > valid_until_ms, errors::nonce_still_valid());
}

public fun owner(signer: &QuoteSigner): address { signer.owner }

public fun signing_pubkey(signer: &QuoteSigner): &vector<u8> { &signer.signing_pubkey }

public fun signing_scheme(signer: &QuoteSigner): u8 { signer.signing_scheme }

public fun signer_id(signer: &QuoteSigner): ID {
    object::id(signer)
}
