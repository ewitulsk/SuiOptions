/// Settlement entry points (spec §4.6–4.7): open-orderbook fills, matched
/// (relayer-submitted) settlement, cancellation, and route composition
/// helpers. Either an entire fill happens at the signed terms or the
/// transaction aborts — there is no partial-custody state at any point.
module exchange::settlement;

use sui::balance::{Self, Balance};
use sui::clock::Clock;
use sui::coin::{Self, Coin};
use sui::event;
use exchange::balance_manager::{Self, BalanceManager, OwnerCap};
use exchange::order::{Self, Order};
use exchange::registry::{Self, SettlementRegistry};
use whitelist::whitelist::{Self, Whitelist};

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
// Obligation settlement (SO-372).
const EWrongEscrow: u64 = 18;
const ELegAmountMismatch: u64 = 19;
const EAlreadyProvided: u64 = 20;
const EAlreadyCollected: u64 = 21;
const ENotProvided: u64 = 22;
const EObligationIncomplete: u64 = 23;
const ERegistryMismatch: u64 = 24;
const ESelfMatch: u64 = 25;

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
    wl: &Whitelist,
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
        reg, wl, maker_bm, order_bytes, &signature, &public_key, taker_coin,
        taker_fill_amount, min_maker_amount_out, true, clock, ctx,
    )
}

/// Mirror fill for a maker order that sells Quote for Base.
public fun fill_limit_order_reverse<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    wl: &Whitelist,
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
        reg, wl, maker_bm, order_bytes, &signature, &public_key, taker_coin,
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
    wl: &Whitelist,
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
        reg, wl, bm_a, bm_b, order_a_bytes, &sig_a, &pk_a, order_b_bytes, &sig_b, &pk_b,
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

/// Signer-authorized watermark cancel for managers whose owner can never
/// be a transaction sender (cap-owned managers, e.g. a trading vault's):
/// any approved signer on the manager may void the OWNER's orders. Only
/// ever a cancel — a hostile signer can deny its own quotes, nothing
/// more.
public fun cancel_up_to_for_manager<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    bm: &BalanceManager,
    min_valid_salt: u64,
    ctx: &TxContext,
) {
    assert!(bm.is_approved_signer(ctx.sender()), ENotMaker);
    let maker = bm.owner();
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

// === The dependency-inverted escrow protocol (SO-372) ===
//
// Modeled on `options_core::collateral` (see that module's header): the
// settlement package does ALL protocol verification and mints an
// ability-less `FillObligation` naming, per leg, the escrow the SIGNED
// order committed to. Escrow implementations are permissionless and
// binding-checked — there is no registry:
//
//   • Providing a leg needs no authorization: the obligation only exists
//     because the maker's signature over an order naming that escrow was
//     verified, and a third party providing someone else's leg is a
//     donation to that maker, never a theft.
//   • Collecting a leg's due requires proof of control of the named
//     escrow: the funded path credits the `BalanceManager` whose id
//     matches (a destination fixed by the signed order); the external
//     path presents the manager's `OwnerCap` — held, for a trading
//     vault, inside its `ExchangeCustody` — and receives a `Balance` to
//     route home. Bearer legs (a Path A taker's coin) are collected by
//     the transaction that owns the potato.
//
// The potato has no abilities, so either every leg is provided and
// collected and `finish` banks the fees and emits the fill events, or
// the entire transaction — including the recorded fill — reverts. A
// malicious escrow implementation can only abort, never steal.
//
// External implementations MUST only collect a leg whose escrow id
// their capability matches, and MUST route the collected due back into
// the escrow's beneficial owner in the same transaction (for the
// trading-vault implementation, enforced by its quote session).

/// Which escrow a leg settles against.
public enum EscrowBinding has copy, drop, store {
    /// A BalanceManager id from the signed order. Funded managers settle
    /// internally; identity-only managers settle via their OwnerCap.
    Manager(ID),
    /// A Path A taker: provided from and collected into the transaction
    /// itself.
    Bearer,
}

public struct LegState has copy, drop, store {
    escrow: EscrowBinding,
    /// What this side must pay in, in its owed token's units.
    owes: u64,
    /// What this side receives (net of its fee), in the other token.
    due: u64,
    provided: bool,
    collected: bool,
}

/// A queued FillEvent, emitted at `finish`.
public struct PendingFill has copy, drop, store {
    digest: vector<u8>,
    maker: address,
    taker: address,
    base_amount: u64,
    quote_amount: u64,
    maker_fee_bps: u64,
    taker_fee_bps: u64,
    maker_fee: u64,
    taker_fee: u64,
    maker_sold_base: bool,
    taker_token_filled_total: u64,
    timestamp_ms: u64,
}

/// A validated, in-flight fill: two legs and the token pools they settle
/// through. No abilities — must be finished this transaction.
public struct FillObligation<phantom Base, phantom Quote> {
    registry_id: ID,
    /// The side selling Base: owes base, due quote (net).
    base_leg: LegState,
    /// The side selling Quote: owes quote, due base (net).
    quote_leg: LegState,
    /// Exact residues left in the pools after both collects.
    fee_base: u64,
    fee_quote: u64,
    pool_base: Balance<Base>,
    pool_quote: Balance<Quote>,
    fills: vector<PendingFill>,
}

/// Open a Path A fill against a maker selling Base. The sender is the
/// taker (bearer quote leg). Validation, amounts, fees, and the fill
/// record are identical to `fill_limit_order` — the classic path and
/// this one share `validate` + `fill_terms`.
public fun begin_fill<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    wl: &Whitelist,
    maker_bm: &BalanceManager,
    order_bytes: vector<u8>,
    signature: vector<u8>,
    public_key: vector<u8>,
    taker_fill_amount: u64,
    min_maker_amount_out: u64,
    clock: &Clock,
    ctx: &TxContext,
): FillObligation<Base, Quote> {
    begin_fill_impl(
        reg, wl, maker_bm, order_bytes, &signature, &public_key, taker_fill_amount,
        min_maker_amount_out, true, true, clock, ctx,
    )
}

/// Mirror for a maker selling Quote (bearer base leg).
public fun begin_fill_reverse<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    wl: &Whitelist,
    maker_bm: &BalanceManager,
    order_bytes: vector<u8>,
    signature: vector<u8>,
    public_key: vector<u8>,
    taker_fill_amount: u64,
    min_maker_amount_out: u64,
    clock: &Clock,
    ctx: &TxContext,
): FillObligation<Base, Quote> {
    begin_fill_impl(
        reg, wl, maker_bm, order_bytes, &signature, &public_key, taker_fill_amount,
        min_maker_amount_out, false, true, clock, ctx,
    )
}

fun begin_fill_impl<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    wl: &Whitelist,
    maker_bm: &BalanceManager,
    order_bytes: vector<u8>,
    signature: &vector<u8>,
    public_key: &vector<u8>,
    taker_fill_amount: u64,
    min_maker_amount_out: u64,
    maker_sells_base: bool,
    verify_sig: bool,
    clock: &Clock,
    ctx: &TxContext,
): FillObligation<Base, Quote> {
    let sender = ctx.sender();
    let (ord, digest) = validate(
        reg, wl, maker_bm, order_bytes, signature, public_key,
        maker_sells_base, sender, sender, verify_sig, clock,
    );
    let (fill_t, fill_m, maker_fee_bps, taker_fee_bps, maker_fee, taker_fee) =
        fill_terms(reg, &ord, &digest, taker_fill_amount, sender);
    assert!(fill_m - taker_fee >= min_maker_amount_out, ESlippage);
    let total = registry::record_fill(reg, digest, fill_t, ord.taker_amount(), ord.expiry_ms());

    let maker_escrow = EscrowBinding::Manager(object::id(maker_bm));
    // Orientation: when the maker sells base, fill_t is quote units and
    // fill_m base units (and vice versa) — see fill_impl/_reverse.
    let (base_leg, quote_leg, fee_base, fee_quote, base_amount, quote_amount) =
        if (maker_sells_base) {
            (
                LegState { escrow: maker_escrow, owes: fill_m, due: fill_t - maker_fee, provided: false, collected: false },
                LegState { escrow: EscrowBinding::Bearer, owes: fill_t, due: fill_m - taker_fee, provided: false, collected: false },
                taker_fee, // skimmed from the base flow
                maker_fee, // skimmed from the quote flow
                fill_m,
                fill_t,
            )
        } else {
            (
                LegState { escrow: EscrowBinding::Bearer, owes: fill_t, due: fill_m - taker_fee, provided: false, collected: false },
                LegState { escrow: maker_escrow, owes: fill_m, due: fill_t - maker_fee, provided: false, collected: false },
                maker_fee, // maker's fee is in base when it sells quote
                taker_fee,
                fill_t,
                fill_m,
            )
        };

    FillObligation {
        registry_id: object::id(reg),
        base_leg,
        quote_leg,
        fee_base,
        fee_quote,
        pool_base: balance::zero(),
        pool_quote: balance::zero(),
        fills: vector[PendingFill {
            digest,
            maker: ord.maker(),
            taker: sender,
            base_amount,
            quote_amount,
            maker_fee_bps,
            taker_fee_bps,
            maker_fee,
            taker_fee,
            maker_sold_base: maker_sells_base,
            taker_token_filled_total: total,
            timestamp_ms: clock.timestamp_ms(),
        }],
    }
}

/// Open a Path B match: `order_a` sells Base (escrow `bm_a`), `order_b`
/// sells Quote (escrow `bm_b`). Identity managers are read-only here —
/// funds move at leg settlement. Same validation/price/fee/record logic
/// as `match_orders` via `validate` + `match_terms`.
public fun begin_match<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    wl: &Whitelist,
    bm_a: &BalanceManager,
    bm_b: &BalanceManager,
    order_a_bytes: vector<u8>,
    sig_a: vector<u8>,
    pk_a: vector<u8>,
    order_b_bytes: vector<u8>,
    sig_b: vector<u8>,
    pk_b: vector<u8>,
    fill_base_amount: u64,
    clock: &Clock,
    ctx: &TxContext,
): FillObligation<Base, Quote> {
    begin_match_impl(
        reg, wl, bm_a, bm_b, order_a_bytes, &sig_a, &pk_a, order_b_bytes, &sig_b, &pk_b,
        fill_base_amount, true, clock, ctx,
    )
}

fun begin_match_impl<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    wl: &Whitelist,
    bm_a: &BalanceManager,
    bm_b: &BalanceManager,
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
): FillObligation<Base, Quote> {
    // The classic path takes both managers `&mut`, which structurally
    // forbids self-matching; identity refs here are immutable, so the
    // guard is explicit.
    assert!(object::id(bm_a) != object::id(bm_b), ESelfMatch);
    let relayer = ctx.sender();
    let maker_a = order::from_bytes(order_a_bytes).maker();
    let maker_b = order::from_bytes(order_b_bytes).maker();

    let (ord_a, digest_a) = validate(
        reg, wl, bm_a, order_a_bytes, sig_a, pk_a,
        /* maker_sells_base */ true, maker_b, relayer, verify_sig, clock,
    );
    let (ord_b, digest_b) = validate(
        reg, wl, bm_b, order_b_bytes, sig_b, pk_b,
        /* maker_sells_base */ false, maker_a, relayer, verify_sig, clock,
    );

    let fee_a_bps = ord_a.max_fee_bps().min(registry::fee_bps_for(reg, ord_a.maker()));
    let fee_b_bps = ord_b.max_fee_bps().min(registry::fee_bps_for(reg, ord_b.maker()));
    let (quote_amount, fee_a, fee_b) =
        match_terms(&ord_a, &ord_b, fill_base_amount, fee_a_bps, fee_b_bps);

    let total_a =
        registry::record_fill(reg, digest_a, quote_amount, ord_a.taker_amount(), ord_a.expiry_ms());
    let total_b =
        registry::record_fill(reg, digest_b, fill_base_amount, ord_b.taker_amount(), ord_b.expiry_ms());

    let now = clock.timestamp_ms();
    FillObligation {
        registry_id: object::id(reg),
        base_leg: LegState {
            escrow: EscrowBinding::Manager(object::id(bm_a)),
            owes: fill_base_amount,
            due: quote_amount - fee_a,
            provided: false,
            collected: false,
        },
        quote_leg: LegState {
            escrow: EscrowBinding::Manager(object::id(bm_b)),
            owes: quote_amount,
            due: fill_base_amount - fee_b,
            provided: false,
            collected: false,
        },
        fee_base: fee_b,
        fee_quote: fee_a,
        pool_base: balance::zero(),
        pool_quote: balance::zero(),
        fills: vector[
            PendingFill {
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
            },
            PendingFill {
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
            },
        ],
    }
}

// --- providing (no authorization needed; see the section header) ---

public fun provide_base<Base, Quote>(ob: &mut FillObligation<Base, Quote>, funds: Balance<Base>) {
    assert!(!ob.base_leg.provided, EAlreadyProvided);
    assert!(funds.value() == ob.base_leg.owes, ELegAmountMismatch);
    ob.pool_base.join(funds);
    ob.base_leg.provided = true;
}

public fun provide_quote<Base, Quote>(ob: &mut FillObligation<Base, Quote>, funds: Balance<Quote>) {
    assert!(!ob.quote_leg.provided, EAlreadyProvided);
    assert!(funds.value() == ob.quote_leg.owes, ELegAmountMismatch);
    ob.pool_quote.join(funds);
    ob.quote_leg.provided = true;
}

/// Funded-manager convenience: debit the leg's owed amount straight from
/// its own manager.
public fun provide_base_from_manager<Base, Quote>(
    ob: &mut FillObligation<Base, Quote>,
    bm: &mut BalanceManager,
) {
    assert!(ob.base_leg.escrow == EscrowBinding::Manager(object::id(bm)), EWrongEscrow);
    let funds = balance_manager::debit<Base>(bm, ob.base_leg.owes);
    provide_base(ob, funds);
}

public fun provide_quote_from_manager<Base, Quote>(
    ob: &mut FillObligation<Base, Quote>,
    bm: &mut BalanceManager,
) {
    assert!(ob.quote_leg.escrow == EscrowBinding::Manager(object::id(bm)), EWrongEscrow);
    let funds = balance_manager::debit<Quote>(bm, ob.quote_leg.owes);
    provide_quote(ob, funds);
}

// --- collecting (binding-checked; the counterparty pool must be funded) ---

/// The base seller's due (quote), credited into its funded manager.
public fun collect_quote_to_manager<Base, Quote>(
    ob: &mut FillObligation<Base, Quote>,
    bm: &mut BalanceManager,
) {
    assert!(ob.base_leg.escrow == EscrowBinding::Manager(object::id(bm)), EWrongEscrow);
    balance_manager::credit(bm, take_quote_due(ob));
}

/// The base seller's due (quote), released to the holder of its identity
/// manager's OwnerCap — the external-escrow control proof.
public fun collect_quote_with_cap<Base, Quote>(
    ob: &mut FillObligation<Base, Quote>,
    cap: &OwnerCap,
): Balance<Quote> {
    assert!(
        ob.base_leg.escrow == EscrowBinding::Manager(balance_manager::cap_bm_id(cap)),
        EWrongEscrow,
    );
    take_quote_due(ob)
}

/// A bearer base-seller's due (Path A reverse taker).
public fun collect_quote_bearer<Base, Quote>(
    ob: &mut FillObligation<Base, Quote>,
): Balance<Quote> {
    assert!(ob.base_leg.escrow == EscrowBinding::Bearer, EWrongEscrow);
    take_quote_due(ob)
}

/// The quote seller's due (base), credited into its funded manager.
public fun collect_base_to_manager<Base, Quote>(
    ob: &mut FillObligation<Base, Quote>,
    bm: &mut BalanceManager,
) {
    assert!(ob.quote_leg.escrow == EscrowBinding::Manager(object::id(bm)), EWrongEscrow);
    balance_manager::credit(bm, take_base_due(ob));
}

public fun collect_base_with_cap<Base, Quote>(
    ob: &mut FillObligation<Base, Quote>,
    cap: &OwnerCap,
): Balance<Base> {
    assert!(
        ob.quote_leg.escrow == EscrowBinding::Manager(balance_manager::cap_bm_id(cap)),
        EWrongEscrow,
    );
    take_base_due(ob)
}

/// A bearer quote-seller's due (Path A taker).
public fun collect_base_bearer<Base, Quote>(
    ob: &mut FillObligation<Base, Quote>,
): Balance<Base> {
    assert!(ob.quote_leg.escrow == EscrowBinding::Bearer, EWrongEscrow);
    take_base_due(ob)
}

fun take_quote_due<Base, Quote>(ob: &mut FillObligation<Base, Quote>): Balance<Quote> {
    assert!(ob.quote_leg.provided, ENotProvided);
    assert!(!ob.base_leg.collected, EAlreadyCollected);
    ob.base_leg.collected = true;
    ob.pool_quote.split(ob.base_leg.due)
}

fun take_base_due<Base, Quote>(ob: &mut FillObligation<Base, Quote>): Balance<Base> {
    assert!(ob.base_leg.provided, ENotProvided);
    assert!(!ob.quote_leg.collected, EAlreadyCollected);
    ob.quote_leg.collected = true;
    ob.pool_base.split(ob.quote_leg.due)
}

/// Close the obligation: every leg provided and collected, the exact fee
/// residues banked, the queued FillEvents emitted.
public fun finish<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    ob: FillObligation<Base, Quote>,
) {
    let FillObligation {
        registry_id,
        base_leg,
        quote_leg,
        fee_base,
        fee_quote,
        pool_base,
        pool_quote,
        mut fills,
    } = ob;
    assert!(registry_id == object::id(reg), ERegistryMismatch);
    assert!(
        base_leg.provided && base_leg.collected && quote_leg.provided && quote_leg.collected,
        EObligationIncomplete,
    );
    assert!(pool_base.value() == fee_base, ELegAmountMismatch);
    assert!(pool_quote.value() == fee_quote, ELegAmountMismatch);
    registry::deposit_fee_base(reg, pool_base);
    registry::deposit_fee_quote(reg, pool_quote);
    while (!fills.is_empty()) {
        let f = fills.remove(0);
        event::emit(FillEvent {
            registry: registry_id,
            digest: f.digest,
            maker: f.maker,
            taker: f.taker,
            base_amount: f.base_amount,
            quote_amount: f.quote_amount,
            maker_fee_bps: f.maker_fee_bps,
            taker_fee_bps: f.taker_fee_bps,
            maker_fee: f.maker_fee,
            taker_fee: f.taker_fee,
            maker_sold_base: f.maker_sold_base,
            taker_token_filled_total: f.taker_token_filled_total,
            timestamp_ms: f.timestamp_ms,
        });
    };
    fills.destroy_empty();
}

// --- obligation getters (for external escrow implementations) ---

public fun base_leg_owes<Base, Quote>(ob: &FillObligation<Base, Quote>): u64 { ob.base_leg.owes }

public fun base_leg_due<Base, Quote>(ob: &FillObligation<Base, Quote>): u64 { ob.base_leg.due }

public fun quote_leg_owes<Base, Quote>(ob: &FillObligation<Base, Quote>): u64 {
    ob.quote_leg.owes
}

public fun quote_leg_due<Base, Quote>(ob: &FillObligation<Base, Quote>): u64 { ob.quote_leg.due }

/// The manager id the base leg is bound to; aborts on bearer legs.
public fun base_leg_manager<Base, Quote>(ob: &FillObligation<Base, Quote>): ID {
    match (ob.base_leg.escrow) {
        EscrowBinding::Manager(id) => id,
        EscrowBinding::Bearer => abort EWrongEscrow,
    }
}

public fun quote_leg_manager<Base, Quote>(ob: &FillObligation<Base, Quote>): ID {
    match (ob.quote_leg.escrow) {
        EscrowBinding::Manager(id) => id,
        EscrowBinding::Bearer => abort EWrongEscrow,
    }
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
    wl: &Whitelist,
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
    // 1b. guarded-launch ingress gate on the tx sender. This is what
    // closes the taker-wallet-coin path (fills can move raw coins that
    // never touched a gated BalanceManager deposit). For match_orders the
    // sender is the relayer, so the relayer wallet must be a member;
    // makers' funds were already gated at BM deposit. FillObligation has
    // no abilities, so every provide_*/collect_* call is stuck inside a
    // tx whose sender passed this gate — the obligation legs need no
    // separate assert.
    whitelist::assert_ingress_allowed(wl, tx_sender);
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
    wl: &Whitelist,
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
        reg, wl, maker_bm, order_bytes, signature, public_key,
        /* maker_sells_base */ true, sender, sender, verify_sig, clock,
    );

    // 9–10. fill amounts and fees (shared with the obligation path).
    let (fill_t, fill_m, maker_fee_bps, taker_fee_bps, maker_fee, taker_fee) =
        fill_terms(reg, &ord, &digest, taker_fill_amount, sender);

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
    wl: &Whitelist,
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
        reg, wl, maker_bm, order_bytes, signature, public_key,
        /* maker_sells_base */ false, sender, sender, verify_sig, clock,
    );

    // fill_t: base units (taker token); fill_m: quote units;
    // maker_fee in base, taker_fee in quote.
    let (fill_t, fill_m, maker_fee_bps, taker_fee_bps, maker_fee, taker_fee) =
        fill_terms(reg, &ord, &digest, taker_fill_amount, sender);

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
    wl: &Whitelist,
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
        reg, wl, bm_a, order_a_bytes, sig_a, pk_a,
        /* maker_sells_base */ true, maker_b, relayer, verify_sig, clock,
    );
    let (ord_b, digest_b) = validate(
        reg, wl, bm_b, order_b_bytes, sig_b, pk_b,
        /* maker_sells_base */ false, maker_a, relayer, verify_sig, clock,
    );

    // Crossing + execution price + limits + fees (shared with the
    // obligation path).
    let fee_a_bps = ord_a.max_fee_bps().min(registry::fee_bps_for(reg, ord_a.maker()));
    let fee_b_bps = ord_b.max_fee_bps().min(registry::fee_bps_for(reg, ord_b.maker()));
    let (quote_amount, fee_a, fee_b) =
        match_terms(&ord_a, &ord_b, fill_base_amount, fee_a_bps, fee_b_bps);

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

/// Steps 9–10 of §4.6: fill amounts (capped to remaining, maker-favoring
/// floor division) and both fees (capped by the order's signed ceiling).
/// `fill_t` is in the order's taker-token units, `fill_m` in maker-token
/// units; the maker's fee is skimmed from the taker-token flow and the
/// taker's from the maker-token flow. Shared verbatim by the classic and
/// obligation paths so their numbers cannot diverge.
fun fill_terms<Base, Quote>(
    reg: &SettlementRegistry<Base, Quote>,
    ord: &Order,
    digest: &vector<u8>,
    taker_fill_amount: u64,
    taker: address,
): (u64, u64, u64, u64, u64, u64) {
    let remaining = ord.taker_amount() - registry::filled(reg, digest);
    let fill_t = taker_fill_amount.min(remaining);
    assert!(fill_t > 0, EZeroFill);
    let fill_m = muldiv_floor(fill_t, ord.maker_amount(), ord.taker_amount());
    let maker_fee_bps = ord.max_fee_bps().min(registry::fee_bps_for(reg, ord.maker()));
    let taker_fee_bps = ord.max_fee_bps().min(registry::fee_bps_for(reg, taker));
    let maker_fee = muldiv_floor(fill_t, maker_fee_bps, BPS_DENOM);
    let taker_fee = muldiv_floor(fill_m, taker_fee_bps, BPS_DENOM);
    (fill_t, fill_m, maker_fee_bps, taker_fee_bps, maker_fee, taker_fee)
}

/// Path B economics (§4.6): crossing check, execution price at the
/// resting (earlier-salt) order rounded in the resting maker's favor,
/// both signed limits, and per-side fees. Shared verbatim by the classic
/// and obligation paths.
fun match_terms(
    ord_a: &Order,
    ord_b: &Order,
    fill_base_amount: u64,
    fee_a_bps: u64,
    fee_b_bps: u64,
): (u64, u64, u64) {
    // Prices must cross (u128 integer form, §4.6 Path B).
    assert!(
        (ord_a.maker_amount() as u128) * (ord_b.maker_amount() as u128)
            >= (ord_a.taker_amount() as u128) * (ord_b.taker_amount() as u128),
        ENotCrossing,
    );

    assert!(fill_base_amount > 0, EZeroFill);

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

    let fee_a = muldiv_floor(quote_amount, fee_a_bps, BPS_DENOM); // quote, from A's proceeds
    let fee_b = muldiv_floor(fill_base_amount, fee_b_bps, BPS_DENOM); // base, from B's proceeds
    (quote_amount, fee_a, fee_b)
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
    wl: &Whitelist,
    maker_bm: &mut BalanceManager,
    order_bytes: vector<u8>,
    taker_coin: Coin<Quote>,
    taker_fill_amount: u64,
    min_maker_amount_out: u64,
    clock: &Clock,
    ctx: &mut TxContext,
): (Coin<Base>, Coin<Quote>) {
    fill_impl(
        reg, wl, maker_bm, order_bytes, &vector[], &vector[], taker_coin,
        taker_fill_amount, min_maker_amount_out, false, clock, ctx,
    )
}

#[test_only]
public fun fill_limit_order_reverse_for_testing<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    wl: &Whitelist,
    maker_bm: &mut BalanceManager,
    order_bytes: vector<u8>,
    taker_coin: Coin<Base>,
    taker_fill_amount: u64,
    min_maker_amount_out: u64,
    clock: &Clock,
    ctx: &mut TxContext,
): (Coin<Quote>, Coin<Base>) {
    fill_impl_reverse(
        reg, wl, maker_bm, order_bytes, &vector[], &vector[], taker_coin,
        taker_fill_amount, min_maker_amount_out, false, clock, ctx,
    )
}

#[test_only]
/// Economic fields of a FillEvent, for cross-path parity assertions:
/// (maker, taker, base_amount, quote_amount, maker_fee, taker_fee,
/// maker_sold_base, taker_token_filled_total).
public fun fill_event_fields(
    e: &FillEvent,
): (address, address, u64, u64, u64, u64, bool, u64) {
    (
        e.maker,
        e.taker,
        e.base_amount,
        e.quote_amount,
        e.maker_fee,
        e.taker_fee,
        e.maker_sold_base,
        e.taker_token_filled_total,
    )
}

#[test_only]
public fun begin_fill_for_testing<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    wl: &Whitelist,
    maker_bm: &BalanceManager,
    order_bytes: vector<u8>,
    taker_fill_amount: u64,
    min_maker_amount_out: u64,
    clock: &Clock,
    ctx: &TxContext,
): FillObligation<Base, Quote> {
    begin_fill_impl(
        reg, wl, maker_bm, order_bytes, &vector[], &vector[], taker_fill_amount,
        min_maker_amount_out, true, false, clock, ctx,
    )
}

#[test_only]
public fun begin_fill_reverse_for_testing<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    wl: &Whitelist,
    maker_bm: &BalanceManager,
    order_bytes: vector<u8>,
    taker_fill_amount: u64,
    min_maker_amount_out: u64,
    clock: &Clock,
    ctx: &TxContext,
): FillObligation<Base, Quote> {
    begin_fill_impl(
        reg, wl, maker_bm, order_bytes, &vector[], &vector[], taker_fill_amount,
        min_maker_amount_out, false, false, clock, ctx,
    )
}

#[test_only]
public fun begin_match_for_testing<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    wl: &Whitelist,
    bm_a: &BalanceManager,
    bm_b: &BalanceManager,
    order_a_bytes: vector<u8>,
    order_b_bytes: vector<u8>,
    fill_base_amount: u64,
    clock: &Clock,
    ctx: &TxContext,
): FillObligation<Base, Quote> {
    begin_match_impl(
        reg, wl, bm_a, bm_b, order_a_bytes, &vector[], &vector[], order_b_bytes,
        &vector[], &vector[], fill_base_amount, false, clock, ctx,
    )
}

#[test_only]
public fun match_orders_for_testing<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    wl: &Whitelist,
    bm_a: &mut BalanceManager,
    bm_b: &mut BalanceManager,
    order_a_bytes: vector<u8>,
    order_b_bytes: vector<u8>,
    fill_base_amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    match_impl(
        reg, wl, bm_a, bm_b, order_a_bytes, &vector[], &vector[], order_b_bytes,
        &vector[], &vector[], fill_base_amount, false, clock, ctx,
    )
}
