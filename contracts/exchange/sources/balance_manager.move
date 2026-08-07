/// Per-user escrow (spec §4.4) — the Sui replacement for ERC-20 allowances.
///
/// Depositing grants spendable capacity to the settlement package, revocable
/// by withdrawal at any time. Self-custodial: only the owner can withdraw,
/// withdrawal is permissionless and instant (and deliberately independent of
/// any registry pause state), and settlement can debit only against a valid
/// maker signature (`debit`/`credit` are `public(package)`).
///
/// No order locking: escrow is a shared pot, over-committed makers fail late
/// and the orderbook service prunes them (§5.4, §5.7).
module exchange::balance_manager;

use std::string::String;
use std::type_name;
use sui::balance::Balance;
use sui::coin::{Self, Coin};
use sui::dynamic_field as df;
use sui::event;
use sui::vec_set::{Self, VecSet};
use exchange::order;

// === Errors ===

const ENotOwner: u64 = 1;
const EInsufficientEscrow: u64 = 2;
const ETooManySigners: u64 = 3;
const EAlreadyApproved: u64 = 4;
const ENotApproved: u64 = 5;

/// Bound on delegated signers (§4.4).
const MAX_APPROVED_SIGNERS: u64 = 16;

public struct BalanceManager has key {
    id: UID,
    owner: address,
    /// Delegated order signers (§4.3): hot trading keys whose signatures are
    /// accepted for this manager's orders. Membership is checked at fill
    /// time, so removing a signer instantly voids that key's outstanding
    /// orders. Funds live as dynamic fields: TypeName -> Balance<T>.
    approved_signers: VecSet<address>,
}

// === Events (§4.8) ===

public struct DepositEvent has copy, drop {
    manager: ID,
    owner: address,
    token: String,
    amount: u64,
}

public struct WithdrawEvent has copy, drop {
    manager: ID,
    owner: address,
    token: String,
    amount: u64,
}

public struct SignerAddedEvent has copy, drop {
    manager: ID,
    owner: address,
    signer: address,
}

public struct SignerRemovedEvent has copy, drop {
    manager: ID,
    owner: address,
    signer: address,
}

// === Lifecycle ===

/// Create and share a manager owned by the transaction sender.
public fun new(ctx: &mut TxContext): ID {
    let bm = BalanceManager {
        id: object::new(ctx),
        owner: ctx.sender(),
        approved_signers: vec_set::empty(),
    };
    let id = object::id(&bm);
    transfer::share_object(bm);
    id
}

// === Funds ===

/// Anyone may deposit into any manager.
public fun deposit<T>(bm: &mut BalanceManager, c: Coin<T>) {
    let amount = c.value();
    let key = type_name::with_original_ids<T>();
    if (df::exists(&bm.id, key)) {
        let bal: &mut Balance<T> = df::borrow_mut(&mut bm.id, key);
        bal.join(c.into_balance());
    } else {
        df::add(&mut bm.id, key, c.into_balance());
    };
    event::emit(DepositEvent {
        manager: object::id(bm),
        owner: bm.owner,
        token: order::canonical_type<T>(),
        amount,
    });
}

/// Owner-only, instant, no lockup — and independent of any pause state.
public fun withdraw<T>(bm: &mut BalanceManager, amount: u64, ctx: &mut TxContext): Coin<T> {
    assert!(ctx.sender() == bm.owner, ENotOwner);
    let out = debit<T>(bm, amount);
    event::emit(WithdrawEvent {
        manager: object::id(bm),
        owner: bm.owner,
        token: order::canonical_type<T>(),
        amount,
    });
    coin::from_balance(out, ctx)
}

/// Settlement-only escrow debit; aborts with `EInsufficientEscrow` when the
/// pot doesn't cover the amount (the relayer decodes this to prune the
/// over-committed maker's orders, §5.6).
public(package) fun debit<T>(bm: &mut BalanceManager, amount: u64): Balance<T> {
    let key = type_name::with_original_ids<T>();
    assert!(df::exists(&bm.id, key), EInsufficientEscrow);
    let bal: &mut Balance<T> = df::borrow_mut(&mut bm.id, key);
    assert!(bal.value() >= amount, EInsufficientEscrow);
    bal.split(amount)
}

/// Fill proceeds are credited back into the manager (not transferred out as
/// a Coin) so market makers trade continuously without re-depositing.
public(package) fun credit<T>(bm: &mut BalanceManager, b: Balance<T>) {
    let key = type_name::with_original_ids<T>();
    if (df::exists(&bm.id, key)) {
        let bal: &mut Balance<T> = df::borrow_mut(&mut bm.id, key);
        bal.join(b);
    } else {
        df::add(&mut bm.id, key, b);
    }
}

// === Delegated signers ===

public fun add_signer(bm: &mut BalanceManager, signer: address, ctx: &TxContext) {
    assert!(ctx.sender() == bm.owner, ENotOwner);
    assert!(!bm.approved_signers.contains(&signer), EAlreadyApproved);
    assert!(bm.approved_signers.length() < MAX_APPROVED_SIGNERS, ETooManySigners);
    bm.approved_signers.insert(signer);
    event::emit(SignerAddedEvent { manager: object::id(bm), owner: bm.owner, signer });
}

/// Removing a signer instantly voids all of that key's outstanding orders —
/// a free per-key bulk cancel for compromised-key response.
public fun remove_signer(bm: &mut BalanceManager, signer: address, ctx: &TxContext) {
    assert!(ctx.sender() == bm.owner, ENotOwner);
    assert!(bm.approved_signers.contains(&signer), ENotApproved);
    bm.approved_signers.remove(&signer);
    event::emit(SignerRemovedEvent { manager: object::id(bm), owner: bm.owner, signer });
}

// === Reads ===

public fun owner(bm: &BalanceManager): address { bm.owner }

public fun is_approved_signer(bm: &BalanceManager, signer: address): bool {
    bm.approved_signers.contains(&signer)
}

public fun balance_of<T>(bm: &BalanceManager): u64 {
    let key = type_name::with_original_ids<T>();
    if (df::exists(&bm.id, key)) {
        let bal: &Balance<T> = df::borrow(&bm.id, key);
        bal.value()
    } else {
        0
    }
}

// === Test helpers ===

#[test_only]
/// Create a shared manager with an explicit owner (lets tests act for a
/// fixture keypair's address without being able to sign transactions as it).
public fun new_with_owner_for_testing(owner: address, ctx: &mut TxContext): ID {
    let bm = BalanceManager { id: object::new(ctx), owner, approved_signers: vec_set::empty() };
    let id = object::id(&bm);
    transfer::share_object(bm);
    id
}
