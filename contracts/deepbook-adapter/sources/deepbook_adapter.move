/// DeepBook v3 spot-trading adapter for the curated trading vault
/// (docs/trading-vault/01-contract-design.md §6, decisions in
/// docs/vault-curator-product.md).
///
/// Custody: one `DeepBookCustody` per vault, stored INSIDE the vault as
/// an adapter-tagged position. It wraps a NON-shared `BalanceManager`
/// plus all three caps, minted at creation while the creating curator is
/// momentarily the BM owner. Once wrapped, `&mut BalanceManager` is only
/// ever reachable through this module's session-gated entry points, so
/// the owner-gated direct `withdraw`/`mint_*_cap` paths are permanently
/// unreachable — the curator can trade but can never route funds
/// anywhere except back into the vault.
///
/// No price guardrails anywhere (design decision 6): order prices and
/// sizes are the curator's alone. The only structural brake is the
/// admin-set pool allowlist. Valuation for NAV happens in the custody
/// appraisal, which must cover every asset the manager holds and every
/// pool with locked balance (both sets tracked on the custody object).
module deepbook_adapter::deepbook_adapter;

use std::type_name::{Self, TypeName};
use sui::clock::Clock;
use sui::coin::Coin;
use sui::event;
use sui::vec_set::{Self, VecSet};

use deepbook::balance_manager::{Self, BalanceManager, DepositCap, TradeCap, WithdrawCap};
use deepbook::pool::{Self, Pool};
use token::deep::DEEP;

use options_core::admin::AdminCap;

use trading_vault::price::{Self, PriceAttestation};
use trading_vault::registry::{IntegrationRegistry, VaultProtocolConfig};
use trading_vault::vault::{Self, Appraisal, CuratorCap, Session, TradingVault};

const E_POOL_NOT_ALLOWED: u64 = 1;
const E_WRONG_CUSTODY: u64 = 2;
const E_POOL_STILL_LOCKED: u64 = 3;
const E_ASSET_STILL_HELD: u64 = 4;
const E_APPRAISAL_INCOMPLETE: u64 = 5;
const E_PRICE_ASSET_MISMATCH: u64 = 6;
const E_VALUE_OVERFLOW: u64 = 7;
const E_MISSING_ATTESTATION: u64 = 8;
const E_MIN_OUT_NOT_MET: u64 = 9;

/// Adapter witness: allowlist this in `IntegrationRegistry` to enable.
public struct DeepBookAdapter has drop {}

/// Admin-vetted DeepBook pools curators may trade.
public struct PoolAllowlist has key {
    id: UID,
    allowed: VecSet<ID>,
}

/// The vault's DeepBook custody, held as a vault position. Everything
/// needed to trade — and to APPRAISE: `assets` is every coin type the
/// manager may hold a balance of, `active_pools` every pool that may
/// hold locked balance; the custody appraisal must cover both sets.
public struct DeepBookCustody has key, store {
    id: UID,
    vault_id: ID,
    bm: BalanceManager,
    trade_cap: TradeCap,
    deposit_cap: DepositCap,
    withdraw_cap: WithdrawCap,
    assets: VecSet<TypeName>,
    active_pools: VecSet<ID>,
}

public struct CustodyCreated has copy, drop {
    vault_id: ID,
    custody_id: ID,
    balance_manager_id: ID,
}

public struct PoolAllowed has copy, drop { pool_id: ID }

/// A curator taker swap of vault free balances against an allowlisted
/// pool. `unswapped` is the input returned unfilled (lot rounding or a
/// thin book).
public struct TakerSwapExecuted has copy, drop {
    vault_id: ID,
    pool_id: ID,
    base_for_quote: bool,
    amount_in: u64,
    amount_out: u64,
    unswapped: u64,
}

public struct PoolDisallowed has copy, drop { pool_id: ID }

fun init(ctx: &mut TxContext) {
    transfer::share_object(PoolAllowlist { id: object::new(ctx), allowed: vec_set::empty() });
}

// ═══════════════════════════════ admin ═══════════════════════════════

public fun allow_pool(_: &AdminCap, list: &mut PoolAllowlist, pool_id: ID) {
    list.allowed.insert(pool_id);
    event::emit(PoolAllowed { pool_id });
}

public fun disallow_pool(_: &AdminCap, list: &mut PoolAllowlist, pool_id: ID) {
    list.allowed.remove(&pool_id);
    event::emit(PoolDisallowed { pool_id });
}

// ═══════════════════════════ custody lifecycle ═══════════════════════════

/// Create the vault's BalanceManager and wrap it (plus all three caps)
/// into vault custody. The transaction sender — the curator — is the BM
/// owner for exactly this transaction, which is what lets the caps be
/// minted; wrapping then seals the owner paths forever.
public fun init_custody(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    ctx: &mut TxContext,
): ID {
    let mut s = vault::begin_session(vault, cap, reg, DeepBookAdapter {});
    let mut bm = balance_manager::new(ctx);
    let bm_id = object::id(&bm);
    let trade_cap = balance_manager::mint_trade_cap(&mut bm, ctx);
    let deposit_cap = balance_manager::mint_deposit_cap(&mut bm, ctx);
    let withdraw_cap = balance_manager::mint_withdraw_cap(&mut bm, ctx);
    let custody = DeepBookCustody {
        id: object::new(ctx),
        vault_id: object::id(vault),
        bm,
        trade_cap,
        deposit_cap,
        withdraw_cap,
        assets: vec_set::empty(),
        active_pools: vec_set::empty(),
    };
    let custody_id = object::id(&custody);
    event::emit(CustodyCreated {
        vault_id: object::id(vault),
        custody_id,
        balance_manager_id: bm_id,
    });
    vault::put_position(vault, &mut s, custody);
    vault::end_session(vault, s);
    custody_id
}

// ══════════════════════ funds in and out of the BM ══════════════════════

/// Move vault free balance into the BalanceManager.
public fun deposit<T>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    custody_id: ID,
    amount: u64,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_session(vault, cap, reg, DeepBookAdapter {});
    let mut custody = take_custody(vault, &mut s, custody_id);
    let funds = vault::take<T>(vault, &mut s, amount);
    deposit_to_bm<T>(&mut custody, sui::coin::from_balance(funds, ctx), ctx);
    let t = type_name::with_defining_ids<T>();
    if (!custody.assets.contains(&t)) { custody.assets.insert(t) };
    vault::put_position(vault, &mut s, custody);
    vault::end_session(vault, s);
}

/// Move BalanceManager funds back into the vault's free balances.
public fun withdraw<T>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    custody_id: ID,
    amount: u64,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_session(vault, cap, reg, DeepBookAdapter {});
    let mut custody = take_custody(vault, &mut s, custody_id);
    let coin: Coin<T> = withdraw_from_bm(&mut custody, amount, ctx);
    prune_asset_if_empty<T>(&mut custody);
    vault::put<T>(vault, &mut s, coin.into_balance());
    vault::put_position(vault, &mut s, custody);
    vault::end_session(vault, s);
}

// ═══════════════════════════════ trading ═══════════════════════════════

/// Thin pass-throughs: no price or size validation by design. Every
/// order pins the pool to the admin allowlist and records it in
/// `active_pools` so appraisals must account for its locked balance.
public fun place_limit_order<B, Q>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    list: &PoolAllowlist,
    custody_id: ID,
    pool: &mut Pool<B, Q>,
    client_order_id: u64,
    order_type: u8,
    self_matching_option: u8,
    price: u64,
    quantity: u64,
    is_bid: bool,
    pay_with_deep: bool,
    expire_timestamp: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(list.allowed.contains(&object::id(pool)), E_POOL_NOT_ALLOWED);
    let mut s = vault::begin_session(vault, cap, reg, DeepBookAdapter {});
    let mut custody = take_custody(vault, &mut s, custody_id);
    let proof = trader_proof(&mut custody, ctx);
    let _info = pool::place_limit_order(
        pool,
        &mut custody.bm,
        &proof,
        client_order_id,
        order_type,
        self_matching_option,
        price,
        quantity,
        is_bid,
        pay_with_deep,
        expire_timestamp,
        clock,
        ctx,
    );
    track_pool(&mut custody, object::id(pool));
    track_pool_assets<B, Q>(&mut custody);
    vault::put_position(vault, &mut s, custody);
    vault::end_session(vault, s);
}

public fun place_market_order<B, Q>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    list: &PoolAllowlist,
    custody_id: ID,
    pool: &mut Pool<B, Q>,
    client_order_id: u64,
    self_matching_option: u8,
    quantity: u64,
    is_bid: bool,
    pay_with_deep: bool,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(list.allowed.contains(&object::id(pool)), E_POOL_NOT_ALLOWED);
    let mut s = vault::begin_session(vault, cap, reg, DeepBookAdapter {});
    let mut custody = take_custody(vault, &mut s, custody_id);
    let proof = trader_proof(&mut custody, ctx);
    let _info = pool::place_market_order(
        pool,
        &mut custody.bm,
        &proof,
        client_order_id,
        self_matching_option,
        quantity,
        is_bid,
        pay_with_deep,
        clock,
        ctx,
    );
    track_pool(&mut custody, object::id(pool));
    vault::put_position(vault, &mut s, custody);
    vault::end_session(vault, s);
}

/// Modify a resting order's quantity (SO-294).
public fun modify_order<B, Q>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    custody_id: ID,
    pool: &mut Pool<B, Q>,
    order_id: u128,
    new_quantity: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_session(vault, cap, reg, DeepBookAdapter {});
    let mut custody = take_custody(vault, &mut s, custody_id);
    let proof = trader_proof(&mut custody, ctx);
    pool::modify_order(pool, &mut custody.bm, &proof, order_id, new_quantity, clock, ctx);
    vault::put_position(vault, &mut s, custody);
    vault::end_session(vault, s);
}

public fun cancel_order<B, Q>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    custody_id: ID,
    pool: &mut Pool<B, Q>,
    order_id: u128,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_session(vault, cap, reg, DeepBookAdapter {});
    let mut custody = take_custody(vault, &mut s, custody_id);
    let proof = trader_proof(&mut custody, ctx);
    pool::cancel_order(pool, &mut custody.bm, &proof, order_id, clock, ctx);
    vault::put_position(vault, &mut s, custody);
    vault::end_session(vault, s);
}

public fun cancel_all_orders<B, Q>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    custody_id: ID,
    pool: &mut Pool<B, Q>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_session(vault, cap, reg, DeepBookAdapter {});
    let mut custody = take_custody(vault, &mut s, custody_id);
    let proof = trader_proof(&mut custody, ctx);
    pool::cancel_all_orders(pool, &mut custody.bm, &proof, clock, ctx);
    vault::put_position(vault, &mut s, custody);
    vault::end_session(vault, s);
}

/// Curator-side settled sweep (also exists as a permissionless crank).
public fun withdraw_settled<B, Q>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    custody_id: ID,
    pool: &mut Pool<B, Q>,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_session(vault, cap, reg, DeepBookAdapter {});
    let mut custody = take_custody(vault, &mut s, custody_id);
    let proof = trader_proof(&mut custody, ctx);
    pool::withdraw_settled_amounts(pool, &mut custody.bm, &proof);
    track_pool_assets<B, Q>(&mut custody);
    vault::put_position(vault, &mut s, custody);
    vault::end_session(vault, s);
}

/// Retire a pool from the appraisal set once nothing is locked in it.
public fun retire_pool<B, Q>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    custody_id: ID,
    pool: &Pool<B, Q>,
) {
    let mut s = vault::begin_session(vault, cap, reg, DeepBookAdapter {});
    let mut custody = take_custody(vault, &mut s, custody_id);
    let (b, q, d) = pool::locked_balance(pool, &custody.bm);
    assert!(b == 0 && q == 0 && d == 0, E_POOL_STILL_LOCKED);
    custody.active_pools.remove(&object::id(pool));
    vault::put_position(vault, &mut s, custody);
    vault::end_session(vault, s);
}

/// Remove an EMPTY custody from vault accounting (its appraisal sets
/// must both be empty) and hand the shell to `recipient`. Required
/// before `finalize_close` — the custody is a position, and closure
/// demands zero positions. The shell's caps only control the empty
/// wrapped manager, so it is inert value-wise.
public fun eject_empty_custody(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    custody_id: ID,
    recipient: address,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_session(vault, cap, reg, DeepBookAdapter {});
    let custody = take_custody(vault, &mut s, custody_id);
    assert!(
        custody.assets.is_empty() && custody.active_pools.is_empty(),
        E_ASSET_STILL_HELD,
    );
    transfer::public_transfer(custody, recipient);
    vault::end_session(vault, s);
    let _ = ctx;
}

// ═══════════════════════════ taker swaps ═══════════════════════════
//
// Direct taker exits for vault FREE balances (SO-299): take from the
// vault, swap against an allowlisted pool, put proceeds (plus any
// unswapped remainder and DEEP change) straight back — all inside one
// curator session. Deliberately NOT routed through the wrapped-BM
// custody: the custody exists for RESTING orders (working capital
// warehoused against the book, tracked by the custody appraisal); a
// taker exit is one-shot and should not require custody setup or leave
// an appraisal-tracked asset entry behind. This is the resale leg for
// option coins freed by `vault_mm::release_coin_to_balances` —
// release<CALL> → taker_swap_base_for_quote<CALL, USDC> in one PTB.
//
// Fees are paid in the input asset (a zero DEEP coin is passed), so no
// DEEP balance is required; `vault::put` drops zero balances, keeping
// `asset_types` clean when there is no remainder or DEEP change.

/// Sell `amount` of Base from vault free balances for Quote. `min_out`
/// binds the actual Quote received: the pool checks it too, EXCEPT when
/// the input rounds below the pool's min size and comes back unswapped —
/// the local assert also catches that — so a curator typo can never
/// donate value to a book.
public fun taker_swap_base_for_quote<B, Q>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    list: &PoolAllowlist,
    pool: &mut Pool<B, Q>,
    amount: u64,
    min_out: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(list.allowed.contains(&object::id(pool)), E_POOL_NOT_ALLOWED);
    let mut s = vault::begin_session(vault, cap, reg, DeepBookAdapter {});
    let base_in = sui::coin::from_balance(vault::take<B>(vault, &mut s, amount), ctx);
    let (base_rem, quote_out, deep_rem) = pool::swap_exact_base_for_quote(
        pool,
        base_in,
        sui::coin::zero<DEEP>(ctx),
        min_out,
        clock,
        ctx,
    );
    assert!(quote_out.value() >= min_out, E_MIN_OUT_NOT_MET);
    event::emit(TakerSwapExecuted {
        vault_id: object::id(vault),
        pool_id: object::id(pool),
        base_for_quote: true,
        amount_in: amount,
        amount_out: quote_out.value(),
        unswapped: base_rem.value(),
    });
    vault::put<B>(vault, &mut s, base_rem.into_balance());
    vault::put<Q>(vault, &mut s, quote_out.into_balance());
    vault::put<DEEP>(vault, &mut s, deep_rem.into_balance());
    vault::end_session(vault, s);
}

/// Buy Base with `amount` of Quote from vault free balances. Same
/// `min_out` semantics as `taker_swap_base_for_quote`, on the Base
/// received.
public fun taker_swap_quote_for_base<B, Q>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    list: &PoolAllowlist,
    pool: &mut Pool<B, Q>,
    amount: u64,
    min_out: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(list.allowed.contains(&object::id(pool)), E_POOL_NOT_ALLOWED);
    let mut s = vault::begin_session(vault, cap, reg, DeepBookAdapter {});
    let quote_in = sui::coin::from_balance(vault::take<Q>(vault, &mut s, amount), ctx);
    let (base_out, quote_rem, deep_rem) = pool::swap_exact_quote_for_base(
        pool,
        quote_in,
        sui::coin::zero<DEEP>(ctx),
        min_out,
        clock,
        ctx,
    );
    assert!(base_out.value() >= min_out, E_MIN_OUT_NOT_MET);
    event::emit(TakerSwapExecuted {
        vault_id: object::id(vault),
        pool_id: object::id(pool),
        base_for_quote: false,
        amount_in: amount,
        amount_out: base_out.value(),
        unswapped: quote_rem.value(),
    });
    vault::put<B>(vault, &mut s, base_out.into_balance());
    vault::put<Q>(vault, &mut s, quote_rem.into_balance());
    vault::put<DEEP>(vault, &mut s, deep_rem.into_balance());
    vault::end_session(vault, s);
}

// ══════════════════════ permissionless cranks / unwind ══════════════════════

/// Sweep settled amounts into the BalanceManager — benign, so it runs
/// under an always-available crank session.
public fun crank_withdraw_settled<B, Q>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    custody_id: ID,
    pool: &mut Pool<B, Q>,
) {
    let mut s = vault::begin_crank_session(vault, reg, DeepBookAdapter {});
    let mut custody = take_custody(vault, &mut s, custody_id);
    pool::withdraw_settled_amounts_permissionless(pool, &mut custody.bm);
    track_pool_assets<B, Q>(&mut custody);
    vault::put_position(vault, &mut s, custody);
    vault::end_session(vault, s);
}

/// Force-unwind (queue starved past grace, or vault Closing): cancel the
/// book so locked balance settles back to the manager.
public fun force_cancel_all<B, Q>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    custody_id: ID,
    pool: &mut Pool<B, Q>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_force_session(vault, reg, DeepBookAdapter {}, clock);
    let mut custody = take_custody(vault, &mut s, custody_id);
    let proof = trader_proof(&mut custody, ctx);
    pool::cancel_all_orders(pool, &mut custody.bm, &proof, clock, ctx);
    vault::put_position(vault, &mut s, custody);
    vault::end_session(vault, s);
}

/// Force-unwind: sweep one asset's whole manager balance back to vault
/// free balances (from where the withdrawal crank can pay the queue).
public fun force_sweep<T>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    custody_id: ID,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_force_session(vault, reg, DeepBookAdapter {}, clock);
    let mut custody = take_custody(vault, &mut s, custody_id);
    let amount = balance_manager::balance<T>(&custody.bm);
    if (amount > 0) {
        let coin: Coin<T> = withdraw_from_bm(&mut custody, amount, ctx);
        vault::put<T>(vault, &mut s, coin.into_balance());
    };
    prune_asset_if_empty<T>(&mut custody);
    vault::put_position(vault, &mut s, custody);
    vault::end_session(vault, s);
}

// ══════════════════════════════ appraisal ══════════════════════════════

/// Sub-potato accumulating the custody's value: every tracked asset and
/// every active pool must be covered before it can be recorded into the
/// vault appraisal.
public struct CustodyAppraisal {
    custody_id: ID,
    remaining_assets: VecSet<TypeName>,
    remaining_pools: VecSet<ID>,
    value: u128,
}

public fun begin_custody_appraisal(vault: &TradingVault, custody_id: ID): CustodyAppraisal {
    let custody: &DeepBookCustody = vault::borrow_position(vault, custody_id);
    CustodyAppraisal {
        custody_id,
        remaining_assets: custody.assets,
        remaining_pools: custody.active_pools,
        value: 0,
    }
}

/// Value one manager-held asset. The deposit asset self-values 1:1; any
/// other asset needs a fresh attestation into the deposit asset.
public fun value_asset<T>(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    ca: &mut CustodyAppraisal,
    att: Option<PriceAttestation>,
    clock: &Clock,
) {
    let custody: &DeepBookCustody = vault::borrow_position(vault, ca.custody_id);
    let t = type_name::with_defining_ids<T>();
    assert!(ca.remaining_assets.contains(&t), E_APPRAISAL_INCOMPLETE);
    let amount = balance_manager::balance<T>(&custody.bm);
    ca.value = ca.value + value_in_deposit(vault, cfg, t, amount, att, clock);
    ca.remaining_assets.remove(&t);
}

/// Value one active pool's locked balances (base, quote, DEEP).
/// Attestations are per-asset options; assets equal to the deposit asset
/// value 1:1, and a zero locked component needs no attestation.
public fun value_pool_locked<B, Q>(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    ca: &mut CustodyAppraisal,
    pool: &Pool<B, Q>,
    base_att: Option<PriceAttestation>,
    quote_att: Option<PriceAttestation>,
    deep_att: Option<PriceAttestation>,
    clock: &Clock,
) {
    let custody: &DeepBookCustody = vault::borrow_position(vault, ca.custody_id);
    let pool_id = object::id(pool);
    assert!(ca.remaining_pools.contains(&pool_id), E_APPRAISAL_INCOMPLETE);
    let (b, q, d) = pool::locked_balance(pool, &custody.bm);
    let mut v = ca.value;
    if (b > 0) {
        v = v + value_in_deposit(vault, cfg, type_name::with_defining_ids<B>(), b, base_att, clock);
    };
    if (q > 0) {
        v = v + value_in_deposit(vault, cfg, type_name::with_defining_ids<Q>(), q, quote_att, clock);
    };
    if (d > 0) {
        v = v
            + value_in_deposit(vault, cfg, type_name::with_defining_ids<DEEP>(), d, deep_att, clock);
    };
    ca.value = v;
    ca.remaining_pools.remove(&pool_id);
}

/// Record the completed custody value into the vault appraisal.
public fun finalize_custody_appraisal(
    vault: &TradingVault,
    appraisal: &mut Appraisal,
    ca: CustodyAppraisal,
) {
    let CustodyAppraisal { custody_id, remaining_assets, remaining_pools, value } = ca;
    assert!(remaining_assets.is_empty() && remaining_pools.is_empty(), E_APPRAISAL_INCOMPLETE);
    assert!(value <= (std::u64::max_value!() as u128), E_VALUE_OVERFLOW);
    vault::record_position_value(vault, appraisal, DeepBookAdapter {}, custody_id, value as u64);
}

// ═══════════════════════════════ internals ═══════════════════════════════

fun take_custody(vault: &mut TradingVault, s: &mut Session, custody_id: ID): DeepBookCustody {
    let custody = vault::take_position<DeepBookCustody>(vault, s, custody_id);
    assert!(custody.vault_id == vault::session_vault_id(s), E_WRONG_CUSTODY);
    custody
}

/// Move rejects `&mut custody.bm` alongside `&custody.trade_cap` inside
/// one call expression; reference-pattern destructuring splits the
/// borrows into disjoint field references, which is legal.
fun trader_proof(
    custody: &mut DeepBookCustody,
    ctx: &TxContext,
): deepbook::balance_manager::TradeProof {
    let DeepBookCustody { bm, trade_cap, .. } = custody;
    balance_manager::generate_proof_as_trader(bm, trade_cap, ctx)
}

fun deposit_to_bm<T>(custody: &mut DeepBookCustody, coin: Coin<T>, ctx: &TxContext) {
    let DeepBookCustody { bm, deposit_cap, .. } = custody;
    balance_manager::deposit_with_cap<T>(bm, deposit_cap, coin, ctx);
}

fun withdraw_from_bm<T>(custody: &mut DeepBookCustody, amount: u64, ctx: &mut TxContext): Coin<T> {
    let DeepBookCustody { bm, withdraw_cap, .. } = custody;
    balance_manager::withdraw_with_cap(bm, withdraw_cap, amount, ctx)
}

fun track_pool_assets<B, Q>(custody: &mut DeepBookCustody) {
    let b = type_name::with_defining_ids<B>();
    let q = type_name::with_defining_ids<Q>();
    if (!custody.assets.contains(&b)) { custody.assets.insert(b) };
    if (!custody.assets.contains(&q)) { custody.assets.insert(q) };
}

fun track_pool(custody: &mut DeepBookCustody, pool_id: ID) {
    if (!custody.active_pools.contains(&pool_id)) { custody.active_pools.insert(pool_id) };
}

fun prune_asset_if_empty<T>(custody: &mut DeepBookCustody) {
    let t = type_name::with_defining_ids<T>();
    if (balance_manager::balance<T>(&custody.bm) == 0 && custody.assets.contains(&t)) {
        custody.assets.remove(&t);
    };
}

fun value_in_deposit(
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

public fun custody_vault_id(c: &DeepBookCustody): ID { c.vault_id }

public fun custody_balance<T>(c: &DeepBookCustody): u64 { balance_manager::balance<T>(&c.bm) }

public fun custody_assets(c: &DeepBookCustody): VecSet<TypeName> { c.assets }

public fun custody_active_pools(c: &DeepBookCustody): VecSet<ID> { c.active_pools }

public fun is_pool_allowed(list: &PoolAllowlist, pool_id: ID): bool {
    list.allowed.contains(&pool_id)
}

#[test_only]
public fun init_for_testing(ctx: &mut TxContext) {
    init(ctx)
}
