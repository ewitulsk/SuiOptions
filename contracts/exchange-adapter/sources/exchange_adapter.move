/// Trading-vault adapter for the hybrid exchange (SO-370): lets a
/// curator market-make on `exchange` with vault capital, with the
/// vault's depositor guarantees intact.
///
/// Custody shape — the inverse of the DeepBook adapter's wrapped BM. The
/// exchange `BalanceManager` must STAY a shared object (takers and the
/// relayer need `&mut` at fill time), so the vault custodies only the
/// AUTHORITY over it: an `ExchangeCustody` position holding the
/// `OwnerCap` plus the set of asset types appraisals must value. The BM
/// is created cap-owned with the vault's ID-as-address as its
/// order-attribution owner; no owner path is ever sender-reachable.
///
/// Trust surface: fills debit the BM only against orders signed by keys
/// the curator delegated (`approved_signers`) — economically equivalent
/// to curator discretion, which the vault model already grants. Value
/// enters NAV from chain state (`balance_of` on the shared BM), not from
/// an attested equity number, and third-party deposits into the BM are
/// rejected by the exchange (donation-lever closure; see
/// `balance_manager::deposit`).
///
/// Session discipline: fund/defund/signer management ride curator
/// sessions. Withdraw-all and signer removal are additionally exposed
/// through FORCE sessions (unlocked when the vault is Closing or the
/// queue head is starved) — pulling the book's working capital home is
/// disruptive, so it is deliberately NOT a crank-session entry.
module exchange_adapter::exchange_adapter;

use std::type_name::{Self, TypeName};
use sui::clock::Clock;
use sui::coin::{Self, Coin};
use sui::event;
use sui::vec_set::{Self, VecSet};

use exchange::balance_manager::{Self, BalanceManager, OwnerCap};
use exchange::registry::SettlementRegistry;
use exchange::settlement::{Self, FillObligation};
use exchange::whitelist::Whitelist;
use trading_vault::price::{Self, PriceAttestation};
use trading_vault::registry::{IntegrationRegistry, VaultProtocolConfig};
use trading_vault::vault::{Self, Appraisal, CuratorCap, Session, TradingVault};

const E_WRONG_CUSTODY: u64 = 1;
const E_WRONG_MANAGER: u64 = 2;
const E_APPRAISAL_INCOMPLETE: u64 = 3;
const E_MISSING_ATTESTATION: u64 = 4;
const E_PRICE_ASSET_MISMATCH: u64 = 5;
const E_VALUE_OVERFLOW: u64 = 6;
const E_ASSET_STILL_HELD: u64 = 7;
// Direct vault escrow (SO-372).
/// fund/defund on a direct custody — its manager is identity-only.
const E_DIRECT_CUSTODY: u64 = 8;
/// The base-selling vault's free balance cannot cover its leg. Side-
/// tagged so the relayer prunes exactly the starved maker's orders.
const E_INSUFFICIENT_ESCROW_A: u64 = 9;
/// The quote-selling vault's free balance cannot cover its leg.
const E_INSUFFICIENT_ESCROW_B: u64 = 10;
/// A vault matched against itself (via its own funded manager) — value-
/// neutral minus fees; rebalance with fund/defund instead.
const E_SELF_CROSS: u64 = 11;
/// Direct-escrow fill entries require a direct custody.
const E_NOT_DIRECT: u64 = 12;

/// Integration witness (allowlisted in `IntegrationRegistry`).
public struct ExchangeAdapter has drop {}

/// The vault's authority over its shared exchange BalanceManager.
public struct ExchangeCustody has key, store {
    id: UID,
    vault_id: ID,
    bm_id: ID,
    owner_cap: OwnerCap,
    /// Escrow mode, fixed at creation (SO-372). Funded (`false`): the
    /// manager warehouses working capital swept in via `fund`. Direct
    /// (`true`): the manager is identity-only — orders escrow against
    /// the VAULT's free balances through quote sessions, and
    /// `fund`/`defund` refuse. A curator wanting both styles runs both
    /// custodies.
    direct: bool,
    /// Asset types the custody appraisal must value in the manager.
    /// Funding tracks automatically; assets a market's fills bring in
    /// (the base of a quoted market) are tracked via `track_asset` —
    /// untracked types simply undercount, the conservative direction.
    /// Always empty on a direct custody (its appraisal is trivially
    /// zero — the capital already lives in appraised free balances).
    assets: VecSet<TypeName>,
}

public struct CustodyCreated has copy, drop {
    vault_id: ID,
    custody_id: ID,
    balance_manager_id: ID,
    direct: bool,
}

/// One vault side of a direct-escrow fill: emitted per vault per fill,
/// alongside the exchange's own FillEvent.
public struct VaultQuoteFilled has copy, drop {
    vault_id: ID,
    custody_id: ID,
    balance_manager_id: ID,
    sold_base: bool,
    base_amount: u64,
    quote_amount: u64,
}

// ═══════════════════════════ custody lifecycle ═══════════════════════════

/// Create the vault's cap-owned BalanceManager (owner = the vault's
/// ID-as-address, for order attribution) and custody its OwnerCap.
/// Funded mode: capital is swept in via `fund` and appraised in the
/// manager.
public fun init_custody(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    ctx: &mut TxContext,
): ID {
    init_custody_impl(vault, cap, reg, false, ctx)
}

/// Direct mode (SO-372): the manager is identity-only; the vault itself
/// is the escrow, settled per fill through quote sessions. Pair with the
/// curator's `vault::add_quote_adapter<ExchangeAdapter>` opt-in.
public fun init_direct_custody(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    ctx: &mut TxContext,
): ID {
    init_custody_impl(vault, cap, reg, true, ctx)
}

fun init_custody_impl(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    direct: bool,
    ctx: &mut TxContext,
): ID {
    let mut s = vault::begin_session(vault, cap, reg, ExchangeAdapter {});
    let (bm_id, owner_cap) =
        balance_manager::new_with_owner_cap(object::id(vault).to_address(), ctx);
    let custody = ExchangeCustody {
        id: object::new(ctx),
        vault_id: object::id(vault),
        bm_id,
        owner_cap,
        direct,
        assets: vec_set::empty(),
    };
    let custody_id = object::id(&custody);
    event::emit(CustodyCreated {
        vault_id: object::id(vault),
        custody_id,
        balance_manager_id: bm_id,
        direct,
    });
    vault::put_position(vault, &mut s, custody);
    vault::end_session(vault, s);
    custody_id
}

// ══════════════════════ funds in and out of the BM ══════════════════════

/// Move vault free balance into the exchange BalanceManager.
public fun fund<T>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    wl: &Whitelist,
    bm: &mut BalanceManager,
    custody_id: ID,
    amount: u64,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_session(vault, cap, reg, ExchangeAdapter {});
    let mut custody = take_custody(vault, &mut s, custody_id, bm);
    assert!(!custody.direct, E_DIRECT_CUSTODY);
    let funds = vault::take<T>(vault, &mut s, amount);
    balance_manager::deposit_with_cap<T>(
        bm,
        wl,
        &custody.owner_cap,
        coin::from_balance(funds, ctx),
        ctx,
    );
    track<T>(&mut custody);
    vault::put_position(vault, &mut s, custody);
    vault::end_session(vault, s);
}

/// Move BalanceManager funds back into the vault's free balances.
public fun defund<T>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    bm: &mut BalanceManager,
    custody_id: ID,
    amount: u64,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_session(vault, cap, reg, ExchangeAdapter {});
    let mut custody = take_custody(vault, &mut s, custody_id, bm);
    assert!(!custody.direct, E_DIRECT_CUSTODY);
    let out = balance_manager::withdraw_with_cap<T>(bm, &custody.owner_cap, amount, ctx);
    vault::put<T>(vault, &mut s, out.into_balance());
    prune_if_empty<T>(&mut custody, bm);
    vault::put_position(vault, &mut s, custody);
    vault::end_session(vault, s);
}

/// Track an asset type fills may credit (the base of a quoted market) so
/// the custody appraisal values it. Not required for funded assets.
public fun track_asset<T>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    bm: &BalanceManager,
    custody_id: ID,
) {
    let mut s = vault::begin_session(vault, cap, reg, ExchangeAdapter {});
    let mut custody = take_custody(vault, &mut s, custody_id, bm);
    assert!(!custody.direct, E_DIRECT_CUSTODY);
    track<T>(&mut custody);
    vault::put_position(vault, &mut s, custody);
    vault::end_session(vault, s);
}

// ═══════════════════════════ signer management ═══════════════════════════

/// Delegate an order-signing hot key (the curator's maker bot).
public fun add_signer(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    bm: &mut BalanceManager,
    custody_id: ID,
    signer: address,
) {
    let mut s = vault::begin_session(vault, cap, reg, ExchangeAdapter {});
    let custody = take_custody(vault, &mut s, custody_id, bm);
    balance_manager::add_signer_with_cap(bm, &custody.owner_cap, signer);
    vault::put_position(vault, &mut s, custody);
    vault::end_session(vault, s);
}

/// Remove a signer — instantly voids that key's outstanding orders.
public fun remove_signer(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    bm: &mut BalanceManager,
    custody_id: ID,
    signer: address,
) {
    let mut s = vault::begin_session(vault, cap, reg, ExchangeAdapter {});
    let custody = take_custody(vault, &mut s, custody_id, bm);
    balance_manager::remove_signer_with_cap(bm, &custody.owner_cap, signer);
    vault::put_position(vault, &mut s, custody);
    vault::end_session(vault, s);
}

/// Remove an EMPTY custody from vault accounting (no tracked assets) and
/// hand the shell to `recipient`. Required before `finalize_close` — the
/// custody is a position and closure demands zero positions. The shell's
/// cap only controls the drained manager, so it is inert value-wise.
public fun eject_empty_custody(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    custody_id: ID,
    recipient: address,
) {
    let mut s = vault::begin_session(vault, cap, reg, ExchangeAdapter {});
    let custody = vault::take_position<ExchangeCustody>(vault, &mut s, custody_id);
    assert!(custody.vault_id == vault::session_vault_id(&s), E_WRONG_CUSTODY);
    assert!(custody.assets.is_empty(), E_ASSET_STILL_HELD);
    transfer::public_transfer(custody, recipient);
    vault::end_session(vault, s);
}

// ═══════════════ force exits (dead-curator recovery) ═══════════════

/// Pull the manager's entire `T` balance back into vault free balances.
/// Force sessions unlock when the vault is Closing or the queue head has
/// aged past `unwind_grace_ms` — this is how exits get funded past an
/// absent curator. Effectively also a bulk cancel: drained escrow makes
/// the owner's resting orders fail at fill time and the orderbook prunes
/// them.
public fun force_defund_all<T>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    bm: &mut BalanceManager,
    custody_id: ID,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_force_session(vault, reg, ExchangeAdapter {}, clock);
    let mut custody = take_custody(vault, &mut s, custody_id, bm);
    assert!(!custody.direct, E_DIRECT_CUSTODY);
    let amount = balance_manager::balance_of<T>(bm);
    if (amount > 0) {
        let out = balance_manager::withdraw_with_cap<T>(bm, &custody.owner_cap, amount, ctx);
        vault::put<T>(vault, &mut s, out.into_balance());
    };
    prune_if_empty<T>(&mut custody, bm);
    vault::put_position(vault, &mut s, custody);
    vault::end_session(vault, s);
}

/// Force-session signer removal: void a dead curator's bot key.
public fun force_remove_signer(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    bm: &mut BalanceManager,
    custody_id: ID,
    signer: address,
    clock: &Clock,
) {
    let mut s = vault::begin_force_session(vault, reg, ExchangeAdapter {}, clock);
    let custody = take_custody(vault, &mut s, custody_id, bm);
    balance_manager::remove_signer_with_cap(bm, &custody.owner_cap, signer);
    vault::put_position(vault, &mut s, custody);
    vault::end_session(vault, s);
}

// ═══════════════════ direct-escrow fills (SO-372) ═══════════════════
//
// The vault leg of the exchange's dependency-inverted escrow protocol:
// settlement mints a `FillObligation` after full validation of orders
// signed by curator-delegated keys; this module provides the vault's
// owed leg from free balances through a QUOTE session and collects its
// due with the custodied OwnerCap (the control proof), routing it back
// into the vault in the same transaction — the standing quote-session
// audit obligation.

/// Path A: a taker fills the vault's maker order selling Base. Returns
/// (taker's base out, taker's quote change).
public fun fill_vault_order<Base, Quote>(
    vault: &mut TradingVault,
    vreg: &IntegrationRegistry,
    reg: &mut SettlementRegistry<Base, Quote>,
    wl: &Whitelist,
    bm: &BalanceManager,
    custody_id: ID,
    order_bytes: vector<u8>,
    signature: vector<u8>,
    public_key: vector<u8>,
    taker_coin: Coin<Quote>,
    taker_fill_amount: u64,
    min_maker_amount_out: u64,
    clock: &Clock,
    ctx: &mut TxContext,
): (Coin<Base>, Coin<Quote>) {
    assert_direct_custody(vault, custody_id, bm);
    let ob = settlement::begin_fill(
        reg, wl, bm, order_bytes, signature, public_key, taker_fill_amount,
        min_maker_amount_out, clock, ctx,
    );
    fill_a_flow(vault, vreg, reg, custody_id, ob, taker_coin, ctx)
}

/// Path A mirror: the vault's maker order sells Quote; the taker pays
/// Base. Returns (taker's quote out, taker's base change).
public fun fill_vault_order_reverse<Base, Quote>(
    vault: &mut TradingVault,
    vreg: &IntegrationRegistry,
    reg: &mut SettlementRegistry<Base, Quote>,
    wl: &Whitelist,
    bm: &BalanceManager,
    custody_id: ID,
    order_bytes: vector<u8>,
    signature: vector<u8>,
    public_key: vector<u8>,
    taker_coin: Coin<Base>,
    taker_fill_amount: u64,
    min_maker_amount_out: u64,
    clock: &Clock,
    ctx: &mut TxContext,
): (Coin<Quote>, Coin<Base>) {
    assert_direct_custody(vault, custody_id, bm);
    let ob = settlement::begin_fill_reverse(
        reg, wl, bm, order_bytes, signature, public_key, taker_fill_amount,
        min_maker_amount_out, clock, ctx,
    );
    fill_b_flow(vault, vreg, reg, custody_id, ob, taker_coin, ctx)
}

/// Path B: the vault (selling Base, `order_a`) crosses a funded-manager
/// maker (selling Quote, `order_b`).
public fun match_vault_vs_bm<Base, Quote>(
    vault: &mut TradingVault,
    vreg: &IntegrationRegistry,
    reg: &mut SettlementRegistry<Base, Quote>,
    wl: &Whitelist,
    bm_a: &BalanceManager,
    custody_id: ID,
    bm_b: &mut BalanceManager,
    order_a_bytes: vector<u8>,
    sig_a: vector<u8>,
    pk_a: vector<u8>,
    order_b_bytes: vector<u8>,
    sig_b: vector<u8>,
    pk_b: vector<u8>,
    fill_base_amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert_direct_custody(vault, custody_id, bm_a);
    assert_not_self_cross(vault, bm_b);
    let mut ob = settlement::begin_match(
        reg, wl, bm_a, bm_b, order_a_bytes, sig_a, pk_a, order_b_bytes, sig_b, pk_b,
        fill_base_amount, clock, ctx,
    );
    let mut s = vault::begin_quote_session(vault, vreg, ExchangeAdapter {});
    provide_base_from_vault(vault, &mut s, &mut ob);
    settlement::provide_quote_from_manager(&mut ob, bm_b);
    collect_quote_to_vault(vault, &mut s, &mut ob, custody_id);
    settlement::collect_base_to_manager(&mut ob, bm_b);
    emit_quote_filled(vault, custody_id, true, &ob);
    settlement::finish(reg, ob);
    vault::end_session(vault, s);
}

/// Path B mirror: a funded-manager maker (selling Base, `order_a`)
/// crosses the vault (selling Quote, `order_b`).
public fun match_bm_vs_vault<Base, Quote>(
    vault: &mut TradingVault,
    vreg: &IntegrationRegistry,
    reg: &mut SettlementRegistry<Base, Quote>,
    wl: &Whitelist,
    bm_a: &mut BalanceManager,
    bm_b: &BalanceManager,
    custody_id: ID,
    order_a_bytes: vector<u8>,
    sig_a: vector<u8>,
    pk_a: vector<u8>,
    order_b_bytes: vector<u8>,
    sig_b: vector<u8>,
    pk_b: vector<u8>,
    fill_base_amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert_direct_custody(vault, custody_id, bm_b);
    assert_not_self_cross(vault, bm_a);
    let mut ob = settlement::begin_match(
        reg, wl, bm_a, bm_b, order_a_bytes, sig_a, pk_a, order_b_bytes, sig_b, pk_b,
        fill_base_amount, clock, ctx,
    );
    let mut s = vault::begin_quote_session(vault, vreg, ExchangeAdapter {});
    settlement::provide_base_from_manager(&mut ob, bm_a);
    provide_quote_from_vault(vault, &mut s, &mut ob);
    settlement::collect_quote_to_manager(&mut ob, bm_a);
    collect_base_to_vault(vault, &mut s, &mut ob, custody_id);
    emit_quote_filled(vault, custody_id, false, &ob);
    settlement::finish(reg, ob);
    vault::end_session(vault, s);
}

/// Path B, both sides vaults: `vault_a` sells Base, `vault_b` sells
/// Quote. Two `&mut TradingVault` arguments are distinct objects by
/// construction (the runtime rejects passing one shared object twice
/// mutably); settlement's ESelfMatch guards the identity managers.
public fun match_vault_vs_vault<Base, Quote>(
    vault_a: &mut TradingVault,
    custody_a: ID,
    bm_a: &BalanceManager,
    vault_b: &mut TradingVault,
    custody_b: ID,
    bm_b: &BalanceManager,
    vreg: &IntegrationRegistry,
    reg: &mut SettlementRegistry<Base, Quote>,
    wl: &Whitelist,
    order_a_bytes: vector<u8>,
    sig_a: vector<u8>,
    pk_a: vector<u8>,
    order_b_bytes: vector<u8>,
    sig_b: vector<u8>,
    pk_b: vector<u8>,
    fill_base_amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert_direct_custody(vault_a, custody_a, bm_a);
    assert_direct_custody(vault_b, custody_b, bm_b);
    let ob = settlement::begin_match(
        reg, wl, bm_a, bm_b, order_a_bytes, sig_a, pk_a, order_b_bytes, sig_b, pk_b,
        fill_base_amount, clock, ctx,
    );
    match_vaults_flow(vault_a, custody_a, vault_b, custody_b, vreg, reg, ob)
}

fun fill_a_flow<Base, Quote>(
    vault: &mut TradingVault,
    vreg: &IntegrationRegistry,
    reg: &mut SettlementRegistry<Base, Quote>,
    custody_id: ID,
    mut ob: FillObligation<Base, Quote>,
    mut taker_coin: Coin<Quote>,
    ctx: &mut TxContext,
): (Coin<Base>, Coin<Quote>) {
    let mut s = vault::begin_quote_session(vault, vreg, ExchangeAdapter {});
    provide_base_from_vault(vault, &mut s, &mut ob);
    let quote_owes = settlement::quote_leg_owes(&ob);
    settlement::provide_quote(&mut ob, taker_coin.split(quote_owes, ctx).into_balance());
    collect_quote_to_vault(vault, &mut s, &mut ob, custody_id);
    let taker_out = coin::from_balance(settlement::collect_base_bearer(&mut ob), ctx);
    emit_quote_filled(vault, custody_id, true, &ob);
    settlement::finish(reg, ob);
    vault::end_session(vault, s);
    (taker_out, taker_coin)
}

fun fill_b_flow<Base, Quote>(
    vault: &mut TradingVault,
    vreg: &IntegrationRegistry,
    reg: &mut SettlementRegistry<Base, Quote>,
    custody_id: ID,
    mut ob: FillObligation<Base, Quote>,
    mut taker_coin: Coin<Base>,
    ctx: &mut TxContext,
): (Coin<Quote>, Coin<Base>) {
    let mut s = vault::begin_quote_session(vault, vreg, ExchangeAdapter {});
    provide_quote_from_vault(vault, &mut s, &mut ob);
    let base_owes = settlement::base_leg_owes(&ob);
    settlement::provide_base(&mut ob, taker_coin.split(base_owes, ctx).into_balance());
    collect_base_to_vault(vault, &mut s, &mut ob, custody_id);
    let taker_out = coin::from_balance(settlement::collect_quote_bearer(&mut ob), ctx);
    emit_quote_filled(vault, custody_id, false, &ob);
    settlement::finish(reg, ob);
    vault::end_session(vault, s);
    (taker_out, taker_coin)
}

fun match_vaults_flow<Base, Quote>(
    vault_a: &mut TradingVault,
    custody_a: ID,
    vault_b: &mut TradingVault,
    custody_b: ID,
    vreg: &IntegrationRegistry,
    reg: &mut SettlementRegistry<Base, Quote>,
    mut ob: FillObligation<Base, Quote>,
) {
    let mut sa = vault::begin_quote_session(vault_a, vreg, ExchangeAdapter {});
    let mut sb = vault::begin_quote_session(vault_b, vreg, ExchangeAdapter {});
    provide_base_from_vault(vault_a, &mut sa, &mut ob);
    provide_quote_from_vault(vault_b, &mut sb, &mut ob);
    collect_quote_to_vault(vault_a, &mut sa, &mut ob, custody_a);
    collect_base_to_vault(vault_b, &mut sb, &mut ob, custody_b);
    emit_quote_filled(vault_a, custody_a, true, &ob);
    emit_quote_filled(vault_b, custody_b, false, &ob);
    settlement::finish(reg, ob);
    vault::end_session(vault_a, sa);
    vault::end_session(vault_b, sb);
}

/// The vault's owed base leg, pre-checked so a starved escrow aborts
/// with the side-tagged code the relayer prunes on.
fun provide_base_from_vault<Base, Quote>(
    vault: &mut TradingVault,
    s: &mut Session,
    ob: &mut FillObligation<Base, Quote>,
) {
    let owes = settlement::base_leg_owes(ob);
    assert!(vault::free_balance_of<Base>(vault) >= owes, E_INSUFFICIENT_ESCROW_A);
    settlement::provide_base(ob, vault::take<Base>(vault, s, owes));
}

fun provide_quote_from_vault<Base, Quote>(
    vault: &mut TradingVault,
    s: &mut Session,
    ob: &mut FillObligation<Base, Quote>,
) {
    let owes = settlement::quote_leg_owes(ob);
    assert!(vault::free_balance_of<Quote>(vault) >= owes, E_INSUFFICIENT_ESCROW_B);
    settlement::provide_quote(ob, vault::take<Quote>(vault, s, owes));
}

/// Collect the vault's due with the custodied OwnerCap and route it
/// straight home.
fun collect_quote_to_vault<Base, Quote>(
    vault: &mut TradingVault,
    s: &mut Session,
    ob: &mut FillObligation<Base, Quote>,
    custody_id: ID,
) {
    let due = {
        let custody: &ExchangeCustody = vault::borrow_position(vault, custody_id);
        settlement::collect_quote_with_cap(ob, &custody.owner_cap)
    };
    vault::put<Quote>(vault, s, due);
}

fun collect_base_to_vault<Base, Quote>(
    vault: &mut TradingVault,
    s: &mut Session,
    ob: &mut FillObligation<Base, Quote>,
    custody_id: ID,
) {
    let due = {
        let custody: &ExchangeCustody = vault::borrow_position(vault, custody_id);
        settlement::collect_base_with_cap(ob, &custody.owner_cap)
    };
    vault::put<Base>(vault, s, due);
}

fun assert_not_self_cross(vault: &TradingVault, counterparty: &BalanceManager) {
    assert!(
        balance_manager::owner(counterparty) != object::id(vault).to_address(),
        E_SELF_CROSS,
    );
}

fun emit_quote_filled<Base, Quote>(
    vault: &TradingVault,
    custody_id: ID,
    sold_base: bool,
    ob: &FillObligation<Base, Quote>,
) {
    let custody: &ExchangeCustody = vault::borrow_position(vault, custody_id);
    event::emit(VaultQuoteFilled {
        vault_id: object::id(vault),
        custody_id,
        balance_manager_id: custody.bm_id,
        sold_base,
        base_amount: settlement::base_leg_owes(ob),
        quote_amount: settlement::quote_leg_owes(ob),
    });
}

#[test_only]
public fun fill_vault_order_for_testing<Base, Quote>(
    vault: &mut TradingVault,
    vreg: &IntegrationRegistry,
    reg: &mut SettlementRegistry<Base, Quote>,
    wl: &Whitelist,
    bm: &BalanceManager,
    custody_id: ID,
    order_bytes: vector<u8>,
    taker_coin: Coin<Quote>,
    taker_fill_amount: u64,
    min_maker_amount_out: u64,
    clock: &Clock,
    ctx: &mut TxContext,
): (Coin<Base>, Coin<Quote>) {
    assert_direct_custody(vault, custody_id, bm);
    let ob = settlement::begin_fill_for_testing(
        reg, wl, bm, order_bytes, taker_fill_amount, min_maker_amount_out, clock, ctx,
    );
    fill_a_flow(vault, vreg, reg, custody_id, ob, taker_coin, ctx)
}

#[test_only]
public fun fill_vault_order_reverse_for_testing<Base, Quote>(
    vault: &mut TradingVault,
    vreg: &IntegrationRegistry,
    reg: &mut SettlementRegistry<Base, Quote>,
    wl: &Whitelist,
    bm: &BalanceManager,
    custody_id: ID,
    order_bytes: vector<u8>,
    taker_coin: Coin<Base>,
    taker_fill_amount: u64,
    min_maker_amount_out: u64,
    clock: &Clock,
    ctx: &mut TxContext,
): (Coin<Quote>, Coin<Base>) {
    assert_direct_custody(vault, custody_id, bm);
    let ob = settlement::begin_fill_reverse_for_testing(
        reg, wl, bm, order_bytes, taker_fill_amount, min_maker_amount_out, clock, ctx,
    );
    fill_b_flow(vault, vreg, reg, custody_id, ob, taker_coin, ctx)
}

#[test_only]
public fun match_vault_vs_bm_for_testing<Base, Quote>(
    vault: &mut TradingVault,
    vreg: &IntegrationRegistry,
    reg: &mut SettlementRegistry<Base, Quote>,
    wl: &Whitelist,
    bm_a: &BalanceManager,
    custody_id: ID,
    bm_b: &mut BalanceManager,
    order_a_bytes: vector<u8>,
    order_b_bytes: vector<u8>,
    fill_base_amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert_direct_custody(vault, custody_id, bm_a);
    assert_not_self_cross(vault, bm_b);
    let mut ob = settlement::begin_match_for_testing(
        reg, wl, bm_a, bm_b, order_a_bytes, order_b_bytes, fill_base_amount, clock, ctx,
    );
    let mut s = vault::begin_quote_session(vault, vreg, ExchangeAdapter {});
    provide_base_from_vault(vault, &mut s, &mut ob);
    settlement::provide_quote_from_manager(&mut ob, bm_b);
    collect_quote_to_vault(vault, &mut s, &mut ob, custody_id);
    settlement::collect_base_to_manager(&mut ob, bm_b);
    emit_quote_filled(vault, custody_id, true, &ob);
    settlement::finish(reg, ob);
    vault::end_session(vault, s);
}

#[test_only]
public fun match_vault_vs_vault_for_testing<Base, Quote>(
    vault_a: &mut TradingVault,
    custody_a: ID,
    bm_a: &BalanceManager,
    vault_b: &mut TradingVault,
    custody_b: ID,
    bm_b: &BalanceManager,
    vreg: &IntegrationRegistry,
    reg: &mut SettlementRegistry<Base, Quote>,
    wl: &Whitelist,
    order_a_bytes: vector<u8>,
    order_b_bytes: vector<u8>,
    fill_base_amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert_direct_custody(vault_a, custody_a, bm_a);
    assert_direct_custody(vault_b, custody_b, bm_b);
    let ob = settlement::begin_match_for_testing(
        reg, wl, bm_a, bm_b, order_a_bytes, order_b_bytes, fill_base_amount, clock, ctx,
    );
    match_vaults_flow(vault_a, custody_a, vault_b, custody_b, vreg, reg, ob)
}

// ══════════════════════════════ appraisal ══════════════════════════════

/// Sub-potato accumulating the custody's value from the SHARED manager's
/// live balances — chain truth, no equity oracle. Every tracked asset
/// must be covered before the value records into the vault appraisal.
public struct CustodyAppraisal {
    custody_id: ID,
    remaining_assets: VecSet<TypeName>,
    value: u128,
}

public fun begin_custody_appraisal(vault: &TradingVault, custody_id: ID): CustodyAppraisal {
    let custody: &ExchangeCustody = vault::borrow_position(vault, custody_id);
    CustodyAppraisal {
        custody_id,
        remaining_assets: custody.assets,
        value: 0,
    }
}

/// Value one manager-held asset. The accounting asset self-values 1:1;
/// any other asset needs a fresh attestation into the accounting asset.
public fun value_asset<T>(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    ca: &mut CustodyAppraisal,
    bm: &BalanceManager,
    att: Option<PriceAttestation>,
    clock: &Clock,
) {
    let custody: &ExchangeCustody = vault::borrow_position(vault, ca.custody_id);
    assert!(custody.bm_id == object::id(bm), E_WRONG_MANAGER);
    let t = type_name::with_defining_ids<T>();
    assert!(ca.remaining_assets.contains(&t), E_APPRAISAL_INCOMPLETE);
    let amount = balance_manager::balance_of<T>(bm);
    ca.value = ca.value + value_in_accounting(vault, cfg, t, amount, att, clock);
    ca.remaining_assets.remove(&t);
}

/// Record the completed custody value into the vault appraisal.
public fun finalize_custody_appraisal(
    vault: &TradingVault,
    appraisal: &mut Appraisal,
    ca: CustodyAppraisal,
) {
    let CustodyAppraisal { custody_id, remaining_assets, value } = ca;
    assert!(remaining_assets.is_empty(), E_APPRAISAL_INCOMPLETE);
    assert!(value <= (std::u64::max_value!() as u128), E_VALUE_OVERFLOW);
    vault::record_position_value(vault, appraisal, ExchangeAdapter {}, custody_id, value as u64);
}

// ═══════════════════════════════ internals ═══════════════════════════════

fun take_custody(
    vault: &mut TradingVault,
    s: &mut Session,
    custody_id: ID,
    bm: &BalanceManager,
): ExchangeCustody {
    let custody = vault::take_position<ExchangeCustody>(vault, s, custody_id);
    assert!(custody.vault_id == vault::session_vault_id(s), E_WRONG_CUSTODY);
    assert!(custody.bm_id == object::id(bm), E_WRONG_MANAGER);
    custody
}

/// Read-only direct-custody binding check for the fill entries.
fun assert_direct_custody(vault: &TradingVault, custody_id: ID, bm: &BalanceManager) {
    let custody: &ExchangeCustody = vault::borrow_position(vault, custody_id);
    assert!(custody.vault_id == object::id(vault), E_WRONG_CUSTODY);
    assert!(custody.bm_id == object::id(bm), E_WRONG_MANAGER);
    assert!(custody.direct, E_NOT_DIRECT);
}

fun track<T>(custody: &mut ExchangeCustody) {
    let t = type_name::with_defining_ids<T>();
    if (!custody.assets.contains(&t)) { custody.assets.insert(t) };
}

fun prune_if_empty<T>(custody: &mut ExchangeCustody, bm: &BalanceManager) {
    if (balance_manager::balance_of<T>(bm) == 0) {
        let t = type_name::with_defining_ids<T>();
        if (custody.assets.contains(&t)) { custody.assets.remove(&t) };
    };
}

fun value_in_accounting(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    asset: TypeName,
    amount: u64,
    mut att: Option<PriceAttestation>,
    clock: &Clock,
): u128 {
    if (amount == 0) {
        return 0
    };
    if (asset == vault::accounting_asset(vault)) {
        return amount as u128
    };
    assert!(att.is_some(), E_MISSING_ATTESTATION);
    let a = att.extract();
    assert!(price::asset(&a) == asset, E_PRICE_ASSET_MISMATCH);
    vault::check_attestation(vault, cfg, &a, clock);
    (((amount as u256) * (price::price(&a) as u256) / (price::price_scale() as u256)) as u128)
}

// ══════════════════════════════ getters ══════════════════════════════

public fun custody_vault_id(c: &ExchangeCustody): ID { c.vault_id }

public fun custody_bm_id(c: &ExchangeCustody): ID { c.bm_id }

public fun custody_assets(c: &ExchangeCustody): VecSet<TypeName> { c.assets }
