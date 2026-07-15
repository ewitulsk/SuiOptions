/// The "simple" market-maker collateral account: the first-party
/// implementation of the standardized collateral-release interface
/// (docs/audit-restructure/04-collateral-abstraction-plan.md §3-4), with
/// no abilities beyond what core's old custody Account did.
///
/// One MM per deployment: `init` creates and shares a single
/// `CollateralAccount` owned by the publisher. A new MM publishes their
/// own copy of this package; nothing in the protocol enumerates
/// implementations — the MM's signed quotes carry the routing
/// (`collateral_source` + `release_package`/`release_module`).
module mm_collateral::mm_collateral;

use std::type_name::{Self, TypeName};
use sui::balance::Balance;
use sui::coin::{Self, Coin};
use sui::dynamic_field as df;
use sui::event;

use options_core::collateral::{Self, CollateralRequest};

/// Sole custody object, created at publish. Balances live as dynamic
/// fields keyed by `BalanceKey<T>` — exactly the old core Account layout.
public struct CollateralAccount has key {
    id: UID,
    owner: address,
}

public struct BalanceKey<phantom T> has copy, drop, store {}

const E_NOT_OWNER: u64 = 1;
const E_WRONG_ACCOUNT: u64 = 2;
const E_INSUFFICIENT_BALANCE: u64 = 3;

// Events are for the OWNING MM's tooling: they carry this package's id,
// so the protocol indexer does not decode them.
public struct Deposited has copy, drop {
    account_id: ID,
    asset_type: TypeName,
    amount: u64,
}

public struct Withdrawn has copy, drop {
    account_id: ID,
    asset_type: TypeName,
    amount: u64,
}

public struct Released has copy, drop {
    account_id: ID,
    asset_type: TypeName,
    amount: u64,
    bucket_id: ID,
    quote_nonce: u64,
}

fun init(ctx: &mut TxContext) {
    transfer::share_object(CollateralAccount {
        id: object::new(ctx),
        owner: ctx.sender(),
    });
}

/// Permissionless top-up (only ever adds funds).
public fun deposit<T>(account: &mut CollateralAccount, coin: Coin<T>) {
    let amount = coin.value();
    let key = BalanceKey<T> {};
    let bal_in = coin.into_balance();
    if (df::exists(&account.id, key)) {
        let bal: &mut Balance<T> = df::borrow_mut(&mut account.id, key);
        bal.join(bal_in);
    } else {
        df::add(&mut account.id, key, bal_in);
    };
    event::emit(Deposited {
        account_id: object::id(account),
        asset_type: type_name::with_defining_ids<T>(),
        amount,
    });
}

/// Owner-only withdrawal.
public fun withdraw<T>(
    account: &mut CollateralAccount,
    amount: u64,
    ctx: &mut TxContext,
): Coin<T> {
    assert!(ctx.sender() == account.owner, E_NOT_OWNER);
    let coin = coin::from_balance(split_balance(account, amount), ctx);
    event::emit(Withdrawn {
        account_id: object::id(account),
        asset_type: type_name::with_defining_ids<T>(),
        amount,
    });
    coin
}

/// The standardized collateral-release interface (plan §3). The request
/// reference is the proof: only options_core mints one, only after
/// signature + expiry + nonce verification of a quote naming
/// `collateral_source`; asserting that source is THIS account is what
/// makes the release safe. The potato forces the caller to consume the
/// returned balance in an `execute_*_flow` this same transaction, or the
/// whole thing (including this debit) reverts.
public fun release<T>(
    account: &mut CollateralAccount,
    request: &CollateralRequest<T>,
    _ctx: &mut TxContext,
): Balance<T> {
    assert!(collateral::source(request) == object::id(account), E_WRONG_ACCOUNT);
    let amount = collateral::amount(request);
    let funds = split_balance<T>(account, amount);
    event::emit(Released {
        account_id: object::id(account),
        asset_type: type_name::with_defining_ids<T>(),
        amount,
        bucket_id: collateral::bucket_id(request),
        quote_nonce: collateral::quote_nonce(request),
    });
    funds
}

fun split_balance<T>(account: &mut CollateralAccount, amount: u64): Balance<T> {
    let key = BalanceKey<T> {};
    assert!(df::exists(&account.id, key), E_INSUFFICIENT_BALANCE);
    let bal: &mut Balance<T> = df::borrow_mut(&mut account.id, key);
    assert!(bal.value() >= amount, E_INSUFFICIENT_BALANCE);
    bal.split(amount)
}

public fun owner(account: &CollateralAccount): address { account.owner }

public fun balance_of<T>(account: &CollateralAccount): u64 {
    let key = BalanceKey<T> {};
    if (!df::exists(&account.id, key)) {
        return 0
    };
    let bal: &Balance<T> = df::borrow(&account.id, key);
    bal.value()
}

#[test_only]
public fun init_for_testing(ctx: &mut TxContext) {
    init(ctx)
}
