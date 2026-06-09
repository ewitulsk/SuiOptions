/// Per-user shared treasury + control state (spec §1.2). Holds the user's
/// `Coin<T>` balance, a monotonically increasing `generation` (bumped to revoke
/// every outstanding cap at once), and a per-cap cumulative spend ledger.
module siws_session::account;

use sui::balance::{Self, Balance};
use sui::coin::{Self, Coin};
use sui::table::{Self, Table};
use sui::transfer::Receiving;

/// One per user. Shared object.
public struct Account<phantom T> has key {
    id: UID,
    /// raw 32-byte Solana ed25519 pubkey that owns this account
    owner_pk: vector<u8>,
    funds: Balance<T>,
    /// bump to revoke ALL outstanding session caps at once
    generation: u64,
    /// per-cap cumulative spend ledger keyed by cap ID
    spent: Table<ID, u64>,
}

public(package) fun new<T>(owner_pk: vector<u8>, ctx: &mut TxContext): Account<T> {
    Account {
        id: object::new(ctx),
        owner_pk,
        funds: balance::zero<T>(),
        generation: 0,
        spent: table::new(ctx),
    }
}

/// Share the account. Kept here (rather than at the session call site) because
/// `Account` deliberately has only `key` — sharing a `key`-only object is only
/// permitted inside its defining module.
public(package) fun share<T>(self: Account<T>) {
    transfer::share_object(self);
}

// --- reads ---

public fun generation<T>(self: &Account<T>): u64 { self.generation }

public fun owner_pk<T>(self: &Account<T>): &vector<u8> { &self.owner_pk }

public fun balance_value<T>(self: &Account<T>): u64 { self.funds.value() }

public fun spent_of<T>(self: &Account<T>, cap_id: ID): u64 {
    if (self.spent.contains(cap_id)) *self.spent.borrow(cap_id) else 0
}

// --- writes ---

/// Anyone may top up an account (funds are only ever removed via a SessionCap).
public entry fun deposit<T>(self: &mut Account<T>, c: Coin<T>) {
    self.funds.join(c.into_balance());
}

/// Sweep a `Coin<T>` that was transferred directly to the Account's id (a
/// common mistake: wallets send to the object id as an `AddressOwner` coin,
/// which is otherwise unspendable). `transfer::receive` lets the object whose
/// `UID` matches that address reclaim it; we join it into `funds`. Permissionless
/// — only coins actually owned by this Account's id can be received.
public entry fun receive<T>(self: &mut Account<T>, coin: Receiving<Coin<T>>) {
    let c = transfer::public_receive(&mut self.id, coin);
    self.funds.join(c.into_balance());
}

public(package) fun bump_generation<T>(self: &mut Account<T>) {
    self.generation = self.generation + 1;
}

public(package) fun record_spend<T>(self: &mut Account<T>, cap_id: ID, new_total: u64) {
    if (self.spent.contains(cap_id)) {
        *self.spent.borrow_mut(cap_id) = new_total;
    } else {
        self.spent.add(cap_id, new_total);
    }
}

public(package) fun take<T>(self: &mut Account<T>, amount: u64, ctx: &mut TxContext): Coin<T> {
    coin::take(&mut self.funds, amount, ctx)
}

#[test_only]
public fun new_for_testing<T>(owner_pk: vector<u8>, ctx: &mut TxContext): Account<T> {
    new<T>(owner_pk, ctx)
}
