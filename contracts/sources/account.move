module options_protocol::account;

use std::type_name;
use sui::balance::Balance;
use sui::clock::Clock;
use sui::coin::{Self, Coin};
use sui::dynamic_field as df;

use options_protocol::errors;
use options_protocol::events;

public struct Account has key {
    id: UID,
    owner: address,
    signing_pubkey: vector<u8>,
}

public struct BalanceKey<phantom T> has copy, drop, store {}

public struct NonceKey has copy, drop, store {
    nonce: u64,
}

public fun create_account(signing_pubkey: vector<u8>, ctx: &mut TxContext): Account {
    let account = Account {
        id: object::new(ctx),
        owner: ctx.sender(),
        signing_pubkey,
    };
    events::emit_account_created(object::id(&account), ctx.sender(), signing_pubkey);
    account
}

public fun create_and_share_account(signing_pubkey: vector<u8>, ctx: &mut TxContext) {
    let account = create_account(signing_pubkey, ctx);
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
    new_pubkey: vector<u8>,
    ctx: &mut TxContext,
) {
    assert!(ctx.sender() == account.owner, errors::not_owner());
    account.signing_pubkey = new_pubkey;
    events::emit_signing_key_rotated(object::id(account), new_pubkey);
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
