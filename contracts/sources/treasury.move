module options_protocol::treasury;

use std::type_name;
use sui::balance::Balance;
use sui::coin::{Self, Coin};
use sui::dynamic_field as df;

use options_protocol::admin::AdminCap;
use options_protocol::errors;
use options_protocol::events;

public struct Treasury has key {
    id: UID,
}

public struct BalanceKey<phantom T> has copy, drop, store {}

public fun create_and_share(_: &AdminCap, ctx: &mut TxContext) {
    let treasury = Treasury { id: object::new(ctx) };
    transfer::share_object(treasury);
}

public(package) fun deposit_balance<T>(treasury: &mut Treasury, bal_in: Balance<T>) {
    let key = BalanceKey<T> {};
    if (df::exists(&treasury.id, key)) {
        let bal: &mut Balance<T> = df::borrow_mut(&mut treasury.id, key);
        bal.join(bal_in);
    } else {
        df::add(&mut treasury.id, key, bal_in);
    };
}

/// Returns the withdrawn coin to the calling PTB; routing is the PTB's job.
public fun withdraw<T>(
    _: &AdminCap,
    treasury: &mut Treasury,
    amount: u64,
    ctx: &mut TxContext,
): Coin<T> {
    let key = BalanceKey<T> {};
    assert!(df::exists(&treasury.id, key), errors::insufficient_treasury_balance());
    let bal: &mut Balance<T> = df::borrow_mut(&mut treasury.id, key);
    assert!(bal.value() >= amount, errors::insufficient_treasury_balance());
    let withdrawn = bal.split(amount);
    let out: Coin<T> = coin::from_balance(withdrawn, ctx);
    events::emit_treasury_withdrawn(type_name::with_defining_ids<T>(), amount);
    out
}

public fun balance_of<T>(treasury: &Treasury): u64 {
    let key = BalanceKey<T> {};
    if (!df::exists(&treasury.id, key)) {
        return 0
    };
    let bal: &Balance<T> = df::borrow(&treasury.id, key);
    bal.value()
}
