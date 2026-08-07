/// Settlement entry points (spec §4.6–4.7): open-orderbook fills, matched
/// (relayer-submitted) settlement, cancellation, and route composition
/// helpers. Either an entire fill happens at the signed terms or the
/// transaction aborts — there is no partial-custody state at any point.
module exchange::settlement;

use sui::clock::Clock;
use sui::coin::{Self, Coin};
use sui::event;
use exchange::balance_manager::{Self, BalanceManager};
use exchange::order::{Self, Order};
use exchange::registry::{Self, SettlementRegistry};

// === Errors (one per check, in check order — the relayer decodes these) ===

const EPaused: u64 = 1;
const ETokenMismatch: u64 = 2;
const EExpired: u64 = 3;
const ETakerRestricted: u64 = 4;
const ESenderRestricted: u64 = 5;
const EBadManager: u64 = 6;
const ESaltVoided: u64 = 7;
const ECancelled: u64 = 8;
const EAlreadyFilled: u64 = 9;
const EBadSignature: u64 = 10;
const EZeroFill: u64 = 11;
const ESlippage: u64 = 12;
const ENotMaker: u64 = 13;
const EWatermarkRegression: u64 = 14;
const ENotCrossing: u64 = 15;
const ELimitViolated: u64 = 16;
const ECoinBelowMin: u64 = 17;

const BPS_DENOM: u64 = 10_000;

// === Events (§4.8) ===
// One event per order digest per fill; in matched mode `taker` is the
// counterparty maker and each side's event carries its own cumulative total.

public struct FillEvent has copy, drop {
    registry: ID,
    digest: vector<u8>,
    maker: address,
    taker: address,
    /// Gross amounts (before fees).
    base_amount: u64,
    quote_amount: u64,
    maker_fee_bps: u64,
    taker_fee_bps: u64,
    /// Fee withheld from this order's maker's proceeds.
    maker_fee: u64,
    /// Fee withheld from the counterparty's proceeds on this fill.
    taker_fee: u64,
    maker_sold_base: bool,
    /// Cumulative taker-token filled for this digest after this fill.
    taker_token_filled_total: u64,
    timestamp_ms: u64,
}

public struct CancelEvent has copy, drop {
    registry: ID,
    digest: vector<u8>,
    maker: address,
}

public struct SaltWatermarkEvent has copy, drop {
    registry: ID,
    maker: address,
    min_valid_salt: u64,
}

// === Path A — open orderbook fill (taker-submitted, §4.6) ===

/// Fill a maker order that sells Base for Quote. The taker passes their own
/// `Coin<Quote>` and receives `(maker tokens bought, taker coin change)`.
public fun fill_limit_order<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    maker_bm: &mut BalanceManager,
    order_bytes: vector<u8>,
    signature: vector<u8>,
    public_key: vector<u8>,
    taker_coin: Coin<Quote>,
    taker_fill_amount: u64,
    min_maker_amount_out: u64,
    clock: &Clock,
    ctx: &mut TxContext,
): (Coin<Base>, Coin<Quote>) {
    fill_impl(
        reg, maker_bm, order_bytes, &signature, &public_key, taker_coin,
        taker_fill_amount, min_maker_amount_out, true, clock, ctx,
    )
}

/// Mirror fill for a maker order that sells Quote for Base.
public fun fill_limit_order_reverse<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    maker_bm: &mut BalanceManager,
    order_bytes: vector<u8>,
    signature: vector<u8>,
    public_key: vector<u8>,
    taker_coin: Coin<Base>,
    taker_fill_amount: u64,
    min_maker_amount_out: u64,
    clock: &Clock,
    ctx: &mut TxContext,
): (Coin<Quote>, Coin<Base>) {
    fill_impl_reverse(
        reg, maker_bm, order_bytes, &signature, &public_key, taker_coin,
        taker_fill_amount, min_maker_amount_out, true, clock, ctx,
    )
}

// === Path B — matched settlement (relayer-submitted, §4.6) ===

/// Cross two resting signed orders. `order_a` sells Base (its maker escrow is
/// `bm_a`); `order_b` sells Quote. Execution price is the resting
/// (earlier-salt) order's price — surplus is price improvement to the newer
/// order — and both signed limits are enforced regardless.
public fun match_orders<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    bm_a: &mut BalanceManager,
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
    match_impl(
        reg, bm_a, bm_b, order_a_bytes, &sig_a, &pk_a, order_b_bytes, &sig_b, &pk_b,
        fill_base_amount, true, clock, ctx,
    )
}

// === Cancellation (§4.7) ===

/// Hard cancel, per order. Only the maker may cancel; the digest is marked
/// unfillable forever (0x semantics: cancel = mark hash unfillable).
public fun cancel<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    order_bytes: vector<u8>,
    ctx: &TxContext,
) {
    let ord = order::from_bytes(order_bytes);
    assert!(ctx.sender() == ord.maker(), ENotMaker);
    let digest = order::digest(&ord, object::id(reg));
    registry::mark_cancelled(reg, digest, ord.expiry_ms());
    event::emit(CancelEvent { registry: object::id(reg), digest, maker: ord.maker() });
}

/// Salt-watermark bulk cancel — the maker dead-man switch: one cheap
/// transaction voids ALL the sender's orders in this market with
/// `salt <= min_valid_salt`.
public fun cancel_up_to<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    min_valid_salt: u64,
    ctx: &TxContext,
) {
    let maker = ctx.sender();
    assert!(min_valid_salt >= registry::watermark(reg, maker), EWatermarkRegression);
    registry::raise_watermark(reg, maker, min_valid_salt);
    event::emit(SaltWatermarkEvent { registry: object::id(reg), maker, min_valid_salt });
}

// === Route composition (§4.6 multi-hop) ===

/// The single strict min-out guard at the end of a multi-branch route: every
/// intra-route hop sets `min_maker_amount_out = 0`, the joined output gets
/// exactly one of these, and the whole PTB reverts atomically if violated.
public fun assert_coin_min<T>(c: &Coin<T>, min: u64) {
    assert!(c.value() >= min, ECoinBelowMin);
}

// === Internals ===

/// Checks 1–8 of §4.6, in order, each with its dedicated abort code.
/// `expected_taker` is the counterparty who takes the order: the tx sender
/// in open-orderbook mode, the opposite maker in matched mode. Signature
/// verification runs last (it is the expensive step) and only when
/// `verify_sig` — the `#[test_only]` wrappers disable it because tests
/// cannot mint wallet signatures for scenario-generated object IDs; the
/// full signed path is covered by the cross-language conformance suite.
fun validate<Base, Quote>(
    reg: &SettlementRegistry<Base, Quote>,
    maker_bm: &BalanceManager,
    order_bytes: vector<u8>,
    signature: &vector<u8>,
    public_key: &vector<u8>,
    maker_sells_base: bool,
    expected_taker: address,
    tx_sender: address,
    verify_sig: bool,
    clock: &Clock,
): (Order, vector<u8>) {
    // 1. pause
    assert!(!registry::is_paused(reg), EPaused);
    // 2. decode + token orientation
    let ord = order::from_bytes(order_bytes);
    let base_str = order::canonical_type<Base>();
    let quote_str = order::canonical_type<Quote>();
    if (maker_sells_base) {
        assert!(
            *ord.maker_token() == base_str && *ord.taker_token() == quote_str,
            ETokenMismatch,
        );
    } else {
        assert!(
            *ord.maker_token() == quote_str && *ord.taker_token() == base_str,
            ETokenMismatch,
        );
    };
    // 3. expiry
    assert!(clock.timestamp_ms() < ord.expiry_ms(), EExpired);
    // 4. taker / sender restrictions
    assert!(ord.taker() == @0x0 || ord.taker() == expected_taker, ETakerRestricted);
    assert!(ord.sender() == @0x0 || ord.sender() == tx_sender, ESenderRestricted);
    // 5. escrow binding
    assert!(
        object::id(maker_bm) == ord.maker_manager_id()
            && maker_bm.owner() == ord.maker(),
        EBadManager,
    );
    // 6. salt watermark
    assert!(ord.salt() > registry::watermark(reg, ord.maker()), ESaltVoided);
    // 7. fill state
    let digest = order::digest(&ord, object::id(reg));
    assert!(!registry::is_cancelled(reg, &digest), ECancelled);
    assert!(registry::filled(reg, &digest) < ord.taker_amount(), EAlreadyFilled);
    // 8. signature — maker itself or a delegated signer on the manager
    if (verify_sig) {
        let (ok, signer) = order::verify_signature(&digest, signature, public_key);
        assert!(
            ok && (signer == ord.maker() || maker_bm.is_approved_signer(signer)),
            EBadSignature,
        );
    };
    (ord, digest)
}

fun fill_impl<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    maker_bm: &mut BalanceManager,
    order_bytes: vector<u8>,
    signature: &vector<u8>,
    public_key: &vector<u8>,
    mut taker_coin: Coin<Quote>,
    taker_fill_amount: u64,
    min_maker_amount_out: u64,
    verify_sig: bool,
    clock: &Clock,
    ctx: &mut TxContext,
): (Coin<Base>, Coin<Quote>) {
    let sender = ctx.sender();
    let (ord, digest) = validate(
        reg, maker_bm, order_bytes, signature, public_key,
        /* maker_sells_base */ true, sender, sender, verify_sig, clock,
    );

    // 9. fill amounts: capped to remaining; maker-favoring floor division so
    // cumulative partial fills can never over-pay the maker's token out.
    let remaining = ord.taker_amount() - registry::filled(reg, &digest);
    let fill_t = taker_fill_amount.min(remaining);
    assert!(fill_t > 0, EZeroFill);
    let fill_m = muldiv_floor(fill_t, ord.maker_amount(), ord.taker_amount());

    // 10. fees, each side capped by the order's signed ceiling
    let maker_fee_bps = ord.max_fee_bps().min(registry::fee_bps_for(reg, ord.maker()));
    let taker_fee_bps = ord.max_fee_bps().min(registry::fee_bps_for(reg, sender));
    let maker_fee = muldiv_floor(fill_t, maker_fee_bps, BPS_DENOM);
    let taker_fee = muldiv_floor(fill_m, taker_fee_bps, BPS_DENOM);

    // 11. taker slippage guard (net of fee) vs. concurrent partial fills
    assert!(fill_m - taker_fee >= min_maker_amount_out, ESlippage);

    // 12. move funds
    let mut quote_in = taker_coin.split(fill_t, ctx).into_balance();
    registry::deposit_fee_quote(reg, quote_in.split(maker_fee));
    balance_manager::credit(maker_bm, quote_in);
    let mut base_out = balance_manager::debit<Base>(maker_bm, fill_m);
    registry::deposit_fee_base(reg, base_out.split(taker_fee));

    // 13. accounting + event
    let total = registry::record_fill(reg, digest, fill_t, ord.taker_amount(), ord.expiry_ms());
    event::emit(FillEvent {
        registry: object::id(reg),
        digest,
        maker: ord.maker(),
        taker: sender,
        base_amount: fill_m,
        quote_amount: fill_t,
        maker_fee_bps,
        taker_fee_bps,
        maker_fee,
        taker_fee,
        maker_sold_base: true,
        taker_token_filled_total: total,
        timestamp_ms: clock.timestamp_ms(),
    });
    (coin::from_balance(base_out, ctx), taker_coin)
}

fun fill_impl_reverse<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    maker_bm: &mut BalanceManager,
    order_bytes: vector<u8>,
    signature: &vector<u8>,
    public_key: &vector<u8>,
    mut taker_coin: Coin<Base>,
    taker_fill_amount: u64,
    min_maker_amount_out: u64,
    verify_sig: bool,
    clock: &Clock,
    ctx: &mut TxContext,
): (Coin<Quote>, Coin<Base>) {
    let sender = ctx.sender();
    let (ord, digest) = validate(
        reg, maker_bm, order_bytes, signature, public_key,
        /* maker_sells_base */ false, sender, sender, verify_sig, clock,
    );

    let remaining = ord.taker_amount() - registry::filled(reg, &digest);
    let fill_t = taker_fill_amount.min(remaining); // base units (taker token)
    assert!(fill_t > 0, EZeroFill);
    let fill_m = muldiv_floor(fill_t, ord.maker_amount(), ord.taker_amount()); // quote units

    let maker_fee_bps = ord.max_fee_bps().min(registry::fee_bps_for(reg, ord.maker()));
    let taker_fee_bps = ord.max_fee_bps().min(registry::fee_bps_for(reg, sender));
    let maker_fee = muldiv_floor(fill_t, maker_fee_bps, BPS_DENOM); // in base
    let taker_fee = muldiv_floor(fill_m, taker_fee_bps, BPS_DENOM); // in quote

    assert!(fill_m - taker_fee >= min_maker_amount_out, ESlippage);

    let mut base_in = taker_coin.split(fill_t, ctx).into_balance();
    registry::deposit_fee_base(reg, base_in.split(maker_fee));
    balance_manager::credit(maker_bm, base_in);
    let mut quote_out = balance_manager::debit<Quote>(maker_bm, fill_m);
    registry::deposit_fee_quote(reg, quote_out.split(taker_fee));

    let total = registry::record_fill(reg, digest, fill_t, ord.taker_amount(), ord.expiry_ms());
    event::emit(FillEvent {
        registry: object::id(reg),
        digest,
        maker: ord.maker(),
        taker: sender,
        base_amount: fill_t,
        quote_amount: fill_m,
        maker_fee_bps,
        taker_fee_bps,
        maker_fee,
        taker_fee,
        maker_sold_base: false,
        taker_token_filled_total: total,
        timestamp_ms: clock.timestamp_ms(),
    });
    (coin::from_balance(quote_out, ctx), taker_coin)
}

fun match_impl<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    bm_a: &mut BalanceManager,
    bm_b: &mut BalanceManager,
    order_a_bytes: vector<u8>,
    sig_a: &vector<u8>,
    pk_a: &vector<u8>,
    order_b_bytes: vector<u8>,
    sig_b: &vector<u8>,
    pk_b: &vector<u8>,
    fill_base_amount: u64,
    verify_sig: bool,
    clock: &Clock,
    ctx: &TxContext,
) {
    let relayer = ctx.sender();
    // Peek makers first so each order's `taker` restriction can be checked
    // against the opposite maker (the actual counterparty in matched mode).
    let maker_a = order::from_bytes(order_a_bytes).maker();
    let maker_b = order::from_bytes(order_b_bytes).maker();

    let (ord_a, digest_a) = validate(
        reg, bm_a, order_a_bytes, sig_a, pk_a,
        /* maker_sells_base */ true, maker_b, relayer, verify_sig, clock,
    );
    let (ord_b, digest_b) = validate(
        reg, bm_b, order_b_bytes, sig_b, pk_b,
        /* maker_sells_base */ false, maker_a, relayer, verify_sig, clock,
    );

    // Prices must cross (u128 integer form, §4.6 Path B).
    assert!(
        (ord_a.maker_amount() as u128) * (ord_b.maker_amount() as u128)
            >= (ord_a.taker_amount() as u128) * (ord_b.taker_amount() as u128),
        ENotCrossing,
    );

    assert!(fill_base_amount > 0, EZeroFill);

    // Execution price = the resting (earlier-salt) order's price, rounded in
    // the resting maker's favor; both signed limits asserted below anyway.
    let a_resting = ord_a.salt() <= ord_b.salt();
    let quote_amount = if (a_resting) {
        // A's price, ceil: A must receive at least its limit.
        muldiv_ceil(fill_base_amount, ord_a.taker_amount(), ord_a.maker_amount())
    } else {
        // B's price, floor: B must pay at most its limit.
        muldiv_floor(fill_base_amount, ord_b.maker_amount(), ord_b.taker_amount())
    };
    assert!(quote_amount > 0, EZeroFill);
    // A's limit: quote_in / base_out >= a.taker/a.maker
    assert!(
        (quote_amount as u128) * (ord_a.maker_amount() as u128)
            >= (fill_base_amount as u128) * (ord_a.taker_amount() as u128),
        ELimitViolated,
    );
    // B's limit: quote_out / base_in <= b.maker/b.taker
    assert!(
        (quote_amount as u128) * (ord_b.taker_amount() as u128)
            <= (fill_base_amount as u128) * (ord_b.maker_amount() as u128),
        ELimitViolated,
    );

    // Fees: each side's rate capped by its own order's signed ceiling.
    let fee_a_bps = ord_a.max_fee_bps().min(registry::fee_bps_for(reg, ord_a.maker()));
    let fee_b_bps = ord_b.max_fee_bps().min(registry::fee_bps_for(reg, ord_b.maker()));
    let fee_a = muldiv_floor(quote_amount, fee_a_bps, BPS_DENOM); // quote, from A's proceeds
    let fee_b = muldiv_floor(fill_base_amount, fee_b_bps, BPS_DENOM); // base, from B's proceeds

    // Accounting first (checks-effects): A's fills are tracked in quote (its
    // taker token), B's in base. An overfill aborts here, before any escrow
    // debit, so the relayer sees EOverfill (stale order — drop and re-match)
    // rather than a misleading escrow error.
    let total_a =
        registry::record_fill(reg, digest_a, quote_amount, ord_a.taker_amount(), ord_a.expiry_ms());
    let total_b =
        registry::record_fill(reg, digest_b, fill_base_amount, ord_b.taker_amount(), ord_b.expiry_ms());

    // Move funds: base A -> B, quote B -> A, fees into the registry vaults.
    let mut base_flow = balance_manager::debit<Base>(bm_a, fill_base_amount);
    registry::deposit_fee_base(reg, base_flow.split(fee_b));
    balance_manager::credit(bm_b, base_flow);
    let mut quote_flow = balance_manager::debit<Quote>(bm_b, quote_amount);
    registry::deposit_fee_quote(reg, quote_flow.split(fee_a));
    balance_manager::credit(bm_a, quote_flow);

    let now = clock.timestamp_ms();
    let registry_id = object::id(reg);
    event::emit(FillEvent {
        registry: registry_id,
        digest: digest_a,
        maker: ord_a.maker(),
        taker: ord_b.maker(),
        base_amount: fill_base_amount,
        quote_amount,
        maker_fee_bps: fee_a_bps,
        taker_fee_bps: fee_b_bps,
        maker_fee: fee_a,
        taker_fee: fee_b,
        maker_sold_base: true,
        taker_token_filled_total: total_a,
        timestamp_ms: now,
    });
    event::emit(FillEvent {
        registry: registry_id,
        digest: digest_b,
        maker: ord_b.maker(),
        taker: ord_a.maker(),
        base_amount: fill_base_amount,
        quote_amount,
        maker_fee_bps: fee_b_bps,
        taker_fee_bps: fee_a_bps,
        maker_fee: fee_b,
        taker_fee: fee_a,
        maker_sold_base: false,
        taker_token_filled_total: total_b,
        timestamp_ms: now,
    });
}

fun muldiv_floor(a: u64, b: u64, c: u64): u64 {
    (((a as u128) * (b as u128)) / (c as u128)) as u64
}

fun muldiv_ceil(a: u64, b: u64, c: u64): u64 {
    let num = (a as u128) * (b as u128);
    let c = c as u128;
    ((num + c - 1) / c) as u64
}

// === Test-only wrappers ===
// Identical code path with signature verification disabled: Move tests
// cannot produce wallet signatures over scenario-generated object IDs. The
// signed path itself is covered end-to-end by the cross-language conformance
// fixtures (tests/conformance_tests.move).

#[test_only]
public fun fill_limit_order_for_testing<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    maker_bm: &mut BalanceManager,
    order_bytes: vector<u8>,
    taker_coin: Coin<Quote>,
    taker_fill_amount: u64,
    min_maker_amount_out: u64,
    clock: &Clock,
    ctx: &mut TxContext,
): (Coin<Base>, Coin<Quote>) {
    fill_impl(
        reg, maker_bm, order_bytes, &vector[], &vector[], taker_coin,
        taker_fill_amount, min_maker_amount_out, false, clock, ctx,
    )
}

#[test_only]
public fun fill_limit_order_reverse_for_testing<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    maker_bm: &mut BalanceManager,
    order_bytes: vector<u8>,
    taker_coin: Coin<Base>,
    taker_fill_amount: u64,
    min_maker_amount_out: u64,
    clock: &Clock,
    ctx: &mut TxContext,
): (Coin<Quote>, Coin<Base>) {
    fill_impl_reverse(
        reg, maker_bm, order_bytes, &vector[], &vector[], taker_coin,
        taker_fill_amount, min_maker_amount_out, false, clock, ctx,
    )
}

#[test_only]
public fun match_orders_for_testing<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    bm_a: &mut BalanceManager,
    bm_b: &mut BalanceManager,
    order_a_bytes: vector<u8>,
    order_b_bytes: vector<u8>,
    fill_base_amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    match_impl(
        reg, bm_a, bm_b, order_a_bytes, &vector[], &vector[], order_b_bytes,
        &vector[], &vector[], fill_base_amount, false, clock, ctx,
    )
}
