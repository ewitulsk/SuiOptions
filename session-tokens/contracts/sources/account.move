/// Per-user shared treasury + control state (spec §1.2). Holds the user's
/// balances (one `Balance<T>` dynamic field per coin type), a monotonically
/// increasing `generation` (bumped to revoke every outstanding cap at once),
/// and a per-(cap, coin type) cumulative spend ledger.
///
/// The account is deliberately NOT generic over a coin type: target contracts
/// thread `&Account` / `&mut Account` purely for the generation check and the
/// spend ledger, and a user's single account must serve every asset their
/// session touches (an options trade alone moves an underlying, a settlement
/// asset, and a per-bucket call coin).
module siws_session::account;

use std::type_name;
use sui::balance::Balance;
use sui::coin::{Self, Coin};
use sui::dynamic_field as df;
use sui::table::{Self, Table};
use sui::transfer::Receiving;

/// One per user. Shared object.
public struct Account has key {
    id: UID,
    /// root identity bytes: 32-byte Solana ed25519 pubkey or 20-byte
    /// Ethereum address (scheme inferred by length)
    owner_pk: vector<u8>,
    /// bump to revoke ALL outstanding session caps at once
    generation: u64,
    /// cumulative spend ledger keyed by (cap ID, canonical coin type)
    spent: Table<SpendKey, u64>,
}

/// Ledger key: spend is tracked per cap AND per coin type, because a single
/// session may move several assets with independent limits.
public struct SpendKey has copy, drop, store {
    cap_id: ID,
    /// canonical `0x`-prefixed type bytes (see `canonical_type_bytes`)
    coin_type: vector<u8>,
}

/// Dynamic-field key for the per-coin-type balance.
public struct BalanceKey<phantom T> has copy, drop, store {}

public(package) fun new(owner_pk: vector<u8>, ctx: &mut TxContext): Account {
    Account {
        id: object::new(ctx),
        owner_pk,
        generation: 0,
        spent: table::new(ctx),
    }
}

/// Share the account. Kept here (rather than at the session call site) because
/// `Account` deliberately has only `key` — sharing a `key`-only object is only
/// permitted inside its defining module.
public(package) fun share(self: Account) {
    transfer::share_object(self);
}

/// Canonical type bytes for `T`: `0x` + the chain `TypeName` (defining ids,
/// 64-hex padded address). Matches the repo-wide canonical form
/// (`normalizeStructTag` on the TS side), so limits the SDK encodes compare
/// byte-equal with types computed on-chain.
public fun canonical_type_bytes<T>(): vector<u8> {
    let mut out = b"0x";
    out.append(type_name::with_defining_ids<T>().into_string().into_bytes());
    out
}

// --- reads ---

public fun generation(self: &Account): u64 { self.generation }

public fun owner_pk(self: &Account): &vector<u8> { &self.owner_pk }

public fun balance_of<T>(self: &Account): u64 {
    let key = BalanceKey<T> {};
    if (!df::exists_(&self.id, key)) { return 0 };
    let bal: &Balance<T> = df::borrow(&self.id, key);
    bal.value()
}

public fun spent_of<T>(self: &Account, cap_id: ID): u64 {
    self.spent_by_type(cap_id, canonical_type_bytes<T>())
}

public(package) fun spent_by_type(self: &Account, cap_id: ID, coin_type: vector<u8>): u64 {
    let key = SpendKey { cap_id, coin_type };
    if (self.spent.contains(key)) *self.spent.borrow(key) else 0
}

// --- writes ---

/// Anyone may top up an account (funds are only ever removed via a SessionCap).
public entry fun deposit<T>(self: &mut Account, c: Coin<T>) {
    let key = BalanceKey<T> {};
    if (df::exists_(&self.id, key)) {
        let bal: &mut Balance<T> = df::borrow_mut(&mut self.id, key);
        bal.join(c.into_balance());
    } else {
        df::add(&mut self.id, key, c.into_balance());
    }
}

/// Sweep a `Coin<T>` that was transferred directly to the Account's id (a
/// common mistake: wallets send to the object id as an `AddressOwner` coin,
/// which is otherwise unspendable). `transfer::receive` lets the object whose
/// `UID` matches that address reclaim it; we join it into the balance.
/// Permissionless — only coins actually owned by this Account's id can be
/// received.
public entry fun receive<T>(self: &mut Account, coin: Receiving<Coin<T>>) {
    let c = transfer::public_receive(&mut self.id, coin);
    deposit(self, c);
}

public(package) fun bump_generation(self: &mut Account) {
    self.generation = self.generation + 1;
}

public(package) fun record_spend(
    self: &mut Account,
    cap_id: ID,
    coin_type: vector<u8>,
    new_total: u64,
) {
    let key = SpendKey { cap_id, coin_type };
    if (self.spent.contains(key)) {
        *self.spent.borrow_mut(key) = new_total;
    } else {
        self.spent.add(key, new_total);
    }
}

public(package) fun take<T>(self: &mut Account, amount: u64, ctx: &mut TxContext): Coin<T> {
    let key = BalanceKey<T> {};
    let bal: &mut Balance<T> = df::borrow_mut(&mut self.id, key);
    coin::from_balance(bal.split(amount), ctx)
}

#[test_only]
public fun new_for_testing(owner_pk: vector<u8>, ctx: &mut TxContext): Account {
    new(owner_pk, ctx)
}
