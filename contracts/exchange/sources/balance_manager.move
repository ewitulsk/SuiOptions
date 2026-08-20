/// Per-user escrow (spec §4.4) — the Sui replacement for ERC-20 allowances.
///
/// Depositing grants spendable capacity to the settlement package, revocable
/// by withdrawal at any time. Self-custodial: only the owner can withdraw,
/// withdrawal is instant (and deliberately independent of any registry
/// pause state), and settlement can debit only against a valid maker
/// signature (`debit`/`credit` are `public(package)`).
///
/// Deposits are restricted to the owner, approved signers, or the
/// `OwnerCap` holder (SO-370): a third-party deposit into someone else's
/// manager would be a donation lever into any vault whose NAV includes
/// the manager's balances — value must never enter appraised custody
/// from a non-shareholder. Fill credits are exempt (they move value at
/// the maker's own signed price).
///
/// Ownership comes in two flavors: address-owned (`new`, sender-checked
/// owner ops — wallets, bots) and cap-owned (`new_with_owner_cap` —
/// object owners like the trading vault, whose ID-as-address can never
/// be a transaction sender). Both keep `owner: address` as the order-
/// attribution identity settlement validates against.
///
/// No order locking: escrow is a shared pot, over-committed makers fail late
/// and the orderbook service prunes them (§5.4, §5.7).
module exchange::balance_manager;

use std::string::String;
use std::type_name;
use sui::balance::Balance;
use sui::coin::{Self, Coin};
use sui::dynamic_field as df;

use whitelist::whitelist::{Self, Whitelist};
use sui::event;
use sui::vec_set::{Self, VecSet};
use exchange::order;

// === Errors ===

const ENotOwner: u64 = 1;
const EInsufficientEscrow: u64 = 2;
const ETooManySigners: u64 = 3;
const EAlreadyApproved: u64 = 4;
const ENotApproved: u64 = 5;
const EDepositRestricted: u64 = 6;
const EWrongCap: u64 = 7;

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

/// Owner authority for a cap-owned manager: withdraw and signer
/// management authorize against this object instead of the sender, so a
/// shared object (the trading vault) can custody the authority while the
/// manager itself stays shared for fills.
public struct OwnerCap has key, store {
    id: UID,
    bm_id: ID,
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

/// Create and share a cap-owned manager. `owner` is the attribution
/// identity orders name as maker (for an object owner, its ID-as-
/// address); the returned `OwnerCap` is the withdraw/signer authority.
public fun new_with_owner_cap(owner: address, ctx: &mut TxContext): (ID, OwnerCap) {
    let bm = BalanceManager {
        id: object::new(ctx),
        owner,
        approved_signers: vec_set::empty(),
    };
    let id = object::id(&bm);
    let cap = OwnerCap { id: object::new(ctx), bm_id: id };
    transfer::share_object(bm);
    (id, cap)
}

// === Funds ===

/// Owner or approved signers only (see the module doc: third-party
/// deposits are a donation lever into vault NAV). Cap holders use
/// `deposit_with_cap`. Ingress-gated on the guarded-launch whitelist;
/// withdrawals are not.
public fun deposit<T>(bm: &mut BalanceManager, wl: &Whitelist, c: Coin<T>, ctx: &TxContext) {
    let sender = ctx.sender();
    whitelist::assert_ingress_allowed(wl, sender, whitelist::domain_exchange());
    assert!(
        sender == bm.owner || bm.approved_signers.contains(&sender),
        EDepositRestricted,
    );
    deposit_internal(bm, c);
}

public fun deposit_with_cap<T>(
    bm: &mut BalanceManager,
    wl: &Whitelist,
    cap: &OwnerCap,
    c: Coin<T>,
    ctx: &TxContext,
) {
    whitelist::assert_ingress_allowed(wl, ctx.sender(), whitelist::domain_exchange());
    assert!(cap.bm_id == object::id(bm), EWrongCap);
    deposit_internal(bm, c);
}

fun deposit_internal<T>(bm: &mut BalanceManager, c: Coin<T>) {
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
    withdraw_internal<T>(bm, amount, ctx)
}

/// Cap-authorized withdraw for cap-owned managers.
public fun withdraw_with_cap<T>(
    bm: &mut BalanceManager,
    cap: &OwnerCap,
    amount: u64,
    ctx: &mut TxContext,
): Coin<T> {
    assert!(cap.bm_id == object::id(bm), EWrongCap);
    withdraw_internal<T>(bm, amount, ctx)
}

fun withdraw_internal<T>(bm: &mut BalanceManager, amount: u64, ctx: &mut TxContext): Coin<T> {
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
    add_signer_internal(bm, signer);
}

public fun add_signer_with_cap(bm: &mut BalanceManager, cap: &OwnerCap, signer: address) {
    assert!(cap.bm_id == object::id(bm), EWrongCap);
    add_signer_internal(bm, signer);
}

fun add_signer_internal(bm: &mut BalanceManager, signer: address) {
    assert!(!bm.approved_signers.contains(&signer), EAlreadyApproved);
    assert!(bm.approved_signers.length() < MAX_APPROVED_SIGNERS, ETooManySigners);
    bm.approved_signers.insert(signer);
    event::emit(SignerAddedEvent { manager: object::id(bm), owner: bm.owner, signer });
}

/// Removing a signer instantly voids all of that key's outstanding orders —
/// a free per-key bulk cancel for compromised-key response.
public fun remove_signer(bm: &mut BalanceManager, signer: address, ctx: &TxContext) {
    assert!(ctx.sender() == bm.owner, ENotOwner);
    remove_signer_internal(bm, signer);
}

public fun remove_signer_with_cap(bm: &mut BalanceManager, cap: &OwnerCap, signer: address) {
    assert!(cap.bm_id == object::id(bm), EWrongCap);
    remove_signer_internal(bm, signer);
}

fun remove_signer_internal(bm: &mut BalanceManager, signer: address) {
    assert!(bm.approved_signers.contains(&signer), ENotApproved);
    bm.approved_signers.remove(&signer);
    event::emit(SignerRemovedEvent { manager: object::id(bm), owner: bm.owner, signer });
}

// === Reads ===

public fun owner(bm: &BalanceManager): address { bm.owner }

public fun is_approved_signer(bm: &BalanceManager, signer: address): bool {
    bm.approved_signers.contains(&signer)
}

public fun cap_bm_id(cap: &OwnerCap): ID { cap.bm_id }

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
