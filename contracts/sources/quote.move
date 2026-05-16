module options_protocol::quote;

use std::bcs;
use sui::clock::Clock;
use sui::ed25519;

use options_protocol::account::{Self, Account};
use options_protocol::admin::{Self, ProtocolConfig};
use options_protocol::errors;

public struct Quote has copy, drop, store {
    protocol_id: vector<u8>,
    signer_account_id: ID,
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
    signer_account_id: ID,
    signer_token_recipient: address,
    bucket_id: ID,
    write_amount: u64,
    premium: u64,
    valid_until_ms: u64,
    nonce: u64,
): Quote {
    Quote {
        protocol_id,
        signer_account_id,
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
public fun signer_account_id(q: &Quote): ID { q.signer_account_id }
public fun signer_token_recipient(q: &Quote): address { q.signer_token_recipient }
public fun bucket_id(q: &Quote): ID { q.bucket_id }
public fun write_amount(q: &Quote): u64 { q.write_amount }
public fun premium(q: &Quote): u64 { q.premium }
public fun valid_until_ms(q: &Quote): u64 { q.valid_until_ms }
public fun nonce(q: &Quote): u64 { q.nonce }

public(package) fun verify_and_consume_quote(
    account: &mut Account,
    config: &ProtocolConfig,
    signed_quote: &SignedQuote,
    clock: &Clock,
): Quote {
    let q = signed_quote.quote;
    check_non_signature_fields(&q, account, config, clock);

    let msg = bcs::to_bytes(&q);
    let valid = ed25519::ed25519_verify(&signed_quote.signature, account::signing_pubkey(account), &msg);
    assert!(valid, errors::quote_signature_invalid());

    account::consume_nonce(account, q.nonce, q.valid_until_ms);

    q
}

fun check_non_signature_fields(
    q: &Quote,
    account: &Account,
    config: &ProtocolConfig,
    clock: &Clock,
) {
    assert!(&q.protocol_id == admin::protocol_id(config), errors::quote_protocol_mismatch());
    assert!(q.signer_account_id == account::account_id(account), errors::quote_account_mismatch());
    assert!(clock.timestamp_ms() < q.valid_until_ms, errors::quote_expired());
    assert!(!account::has_nonce(account, q.nonce), errors::quote_nonce_used());
}

#[test_only]
public(package) fun verify_skip_sig(
    account: &mut Account,
    config: &ProtocolConfig,
    signed_quote: &SignedQuote,
    clock: &Clock,
): Quote {
    let q = signed_quote.quote;
    check_non_signature_fields(&q, account, config, clock);
    account::consume_nonce(account, q.nonce, q.valid_until_ms);
    q
}
