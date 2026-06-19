/// Session-gated DeepBook trading (siws_session integration).
///
/// The session's ephemeral key signs the PTB, so it simply OWNS the
/// `BalanceManager` and trades via DeepBook's owner-path — no `TradeCap`
/// delegation and no DeepBook app-authorization. These thin twins are the
/// bridge between the user's options `Account` custody and DeepBook:
/// collateral is pulled from custody, the order fills, and EVERY settled
/// balance sweeps straight back into custody, so nothing rests in the
/// `BalanceManager` between trades and no value reaches an arbitrary recipient
/// (cap-free `authorize` gating, consistent with `session_bucket` /
/// `session_vault`).
///
/// Market orders only: a market order deposits → fills → sweeps back in one
/// PTB, so the per-session `BalanceManager` is always left empty. A resting
/// (limit) order would strand funds behind the ephemeral key if the session
/// lapsed before it filled, so limit orders are intentionally not offered to
/// session users.
module options_protocol::session_deepbook;

use sui::clock::Clock;
use sui::coin::Coin;

use deepbook::balance_manager::{Self, BalanceManager};
use deepbook::constants;
use deepbook::pool::Pool;
use deepbook::registry::Registry;

use siws_session::account::Account as SessionAccount;
use siws_session::session::{Self, SessionCap};

use options_protocol::account::{Self, Account};
use options_protocol::session_account;

const SEL_ENABLE_TRADING: vector<u8> =
    b"options_protocol::session_deepbook::enable_trading_with_session";
const SEL_PLACE_MARKET: vector<u8> =
    b"options_protocol::session_deepbook::place_market_order_with_session";

public fun enable_trading_selector(): vector<u8> { SEL_ENABLE_TRADING }
public fun place_market_order_selector(): vector<u8> { SEL_PLACE_MARKET }

/// One-time per session: create a `BalanceManager` owned by the ephemeral
/// session key, register it (so the frontend can rediscover it from the
/// `BalanceManagerEvent`), and share it. Cap-gated; touches no custody — the
/// trade entrypoints move the funds.
public fun enable_trading_with_session(
    registry: &mut Registry,
    cap: &SessionCap,
    session_account: &SessionAccount,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    // Binds the cap to the sender (the ephemeral key, which becomes the BM
    // owner) and to `session_account`.
    session::authorize(cap, session_account, clock, SEL_ENABLE_TRADING, ctx.sender());
    let bm = balance_manager::new(ctx);
    bm.register_balance_manager(registry, ctx);
    transfer::public_share_object(bm);
}

/// Session twin of a DeepBook market order. A buy (`is_bid`) pulls up to
/// `max_quote_in` settlement from custody; a sell delivers `quantity` base
/// from custody. The order fills against the book, then both legs settle back
/// into custody (bought base + unspent quote for a buy; proceeds + unfilled
/// base for a sell). No value leaves custody to an arbitrary recipient, so
/// this is `authorize`-gated (cap-free).
public fun place_market_order_with_session<Base, Quote>(
    pool: &mut Pool<Base, Quote>,
    bm: &mut BalanceManager,
    user_account: &mut Account,
    cap: &SessionCap,
    session_account: &SessionAccount,
    client_order_id: u64,
    is_bid: bool,
    quantity: u64,
    max_quote_in: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    session_account::assert_session_linked(user_account, cap);
    session::authorize(cap, session_account, clock, SEL_PLACE_MARKET, ctx.sender());

    // Fund the BM from custody: a buy pays quote, a sell delivers base. The BM
    // is owned by the ephemeral signer, so the owner-path `deposit` applies.
    if (is_bid) {
        let quote_in = account::withdraw_internal<Quote>(user_account, max_quote_in, ctx);
        bm.deposit(quote_in, ctx);
    } else {
        let base_in = account::withdraw_internal<Base>(user_account, quantity, ctx);
        bm.deposit(base_in, ctx);
    };

    let proof = bm.generate_proof_as_owner(ctx);
    // pay_with_deep = false: taker fees ride in the input token, no DEEP held.
    let _order = pool.place_market_order(
        bm,
        &proof,
        client_order_id,
        constants::cancel_taker(),
        quantity,
        is_bid,
        false,
        clock,
        ctx,
    );

    // Sweep every settled balance back into custody, leaving the BM empty.
    pool.withdraw_settled_amounts(bm, &proof);
    sweep_to_custody<Base>(bm, user_account, ctx);
    sweep_to_custody<Quote>(bm, user_account, ctx);
}

/// Drain the BM's entire `T` balance back into the user's custody (or drop a
/// zero coin without emitting a spurious deposit event).
fun sweep_to_custody<T>(bm: &mut BalanceManager, user_account: &mut Account, ctx: &mut TxContext) {
    let coin: Coin<T> = bm.withdraw_all<T>(ctx);
    if (coin.value() > 0) {
        account::deposit_balance(user_account, coin.into_balance());
    } else {
        coin.destroy_zero();
    };
}
