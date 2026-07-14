module options_core::account;

use std::type_name;
use sui::balance::Balance;
use sui::clock::Clock;
use sui::coin::{Self, Coin};
use sui::dynamic_field as df;

use options_core::errors;
use options_core::events;

/// Signature scheme tag stored alongside the pubkey. Verified against the
/// allowed list in `assert_scheme_supported`. Tag values intentionally
/// match `quote::SCHEME_*` so we never store one and dispatch on another.
public struct Account has key {
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

public struct BalanceKey<phantom T> has copy, drop, store {}

public struct NonceKey has copy, drop, store {
    nonce: u64,
}

public fun create_account(
    signing_scheme: u8,
    signing_pubkey: vector<u8>,
    ctx: &mut TxContext,
): Account {
    assert_scheme_pubkey(signing_scheme, &signing_pubkey);
    let owner = ctx.sender();
    let account = Account {
        id: object::new(ctx),
        owner,
        signing_scheme,
        signing_pubkey,
    };
    events::emit_account_created(
        object::id(&account),
        owner,
        signing_scheme,
        signing_pubkey,
    );
    account
}

public fun create_and_share_account(
    signing_scheme: u8,
    signing_pubkey: vector<u8>,
    ctx: &mut TxContext,
) {
    let account = create_account(signing_scheme, signing_pubkey, ctx);
    transfer::share_object(account);
}

public fun deposit<T>(account: &mut Account, coin: Coin<T>) {
    let amount = coin.value();
    let key = BalanceKey<T> {};
    let bal_in = coin.into_balance();
    if (df::exists(&account.id, key)) {
        let bal: &mut Balance<T> = df::borrow_mut(&mut account.id, key);
        bal.join(bal_in);
    } else {
        df::add(&mut account.id, key, bal_in);
    };
    events::emit_account_deposit(object::id(account), type_name::with_defining_ids<T>(), amount);
}

public fun withdraw<T>(account: &mut Account, amount: u64, ctx: &mut TxContext): Coin<T> {
    assert!(ctx.sender() == account.owner, errors::not_owner());
    let coin = withdraw_internal<T>(account, amount, ctx);
    events::emit_account_withdraw(object::id(account), type_name::with_defining_ids<T>(), amount);
    coin
}

public(package) fun withdraw_internal<T>(
    account: &mut Account,
    amount: u64,
    ctx: &mut TxContext,
): Coin<T> {
    let key = BalanceKey<T> {};
    assert!(df::exists(&account.id, key), errors::insufficient_account_balance());
    let bal: &mut Balance<T> = df::borrow_mut(&mut account.id, key);
    assert!(bal.value() >= amount, errors::insufficient_account_balance());
    coin::from_balance(bal.split(amount), ctx)
}

public(package) fun deposit_balance<T>(account: &mut Account, bal_in: Balance<T>) {
    let amount = bal_in.value();
    let key = BalanceKey<T> {};
    if (df::exists(&account.id, key)) {
        let bal: &mut Balance<T> = df::borrow_mut(&mut account.id, key);
        bal.join(bal_in);
    } else {
        df::add(&mut account.id, key, bal_in);
    };
    events::emit_account_deposit(object::id(account), type_name::with_defining_ids<T>(), amount);
}

public fun set_quote_signing_key(
    account: &mut Account,
    new_scheme: u8,
    new_pubkey: vector<u8>,
    ctx: &mut TxContext,
) {
    assert!(ctx.sender() == account.owner, errors::not_owner());
    assert_scheme_pubkey(new_scheme, &new_pubkey);
    account.signing_scheme = new_scheme;
    account.signing_pubkey = new_pubkey;
    events::emit_signing_key_rotated(object::id(account), new_scheme, new_pubkey);
}

public(package) fun has_nonce(account: &Account, nonce: u64): bool {
    df::exists(&account.id, NonceKey { nonce })
}

public(package) fun consume_nonce(account: &mut Account, nonce: u64, valid_until_ms: u64) {
    df::add(&mut account.id, NonceKey { nonce }, valid_until_ms);
}

public fun prune_nonce(account: &mut Account, nonce: u64, clock: &Clock) {
    let key = NonceKey { nonce };
    if (!df::exists(&account.id, key)) {
        return
    };
    let valid_until_ms: u64 = df::remove(&mut account.id, key);
    assert!(clock.timestamp_ms() > valid_until_ms, errors::nonce_still_valid());
}

public fun owner(account: &Account): address { account.owner }

public fun signing_pubkey(account: &Account): &vector<u8> { &account.signing_pubkey }

public fun signing_scheme(account: &Account): u8 { account.signing_scheme }

public fun balance_of<T>(account: &Account): u64 {
    let key = BalanceKey<T> {};
    if (!df::exists(&account.id, key)) {
        return 0
    };
    let bal: &Balance<T> = df::borrow(&account.id, key);
    bal.value()
}

public fun account_id(account: &Account): ID {
    object::id(account)
}
