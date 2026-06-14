/// On-chain proceeds-swap auction: a Pyth-bounded reverse auction that
/// converts a vault's settlement proceeds back into underlying without a
/// DEX (docs/vault-implementation-guide/03-vault-contract.md §7.3).
///
/// The vault escrows settlement (the USDC it collected from premium +
/// exercise proceeds) and opens an auction; market makers escrow
/// *underlying* bids on-chain, competing to give the vault the most
/// underlying for that settlement. After the deadline the vault's
/// `settle_swap_rfq` crank resolves it and re-checks the winning rate
/// against a *fresh* Pyth cross — the swap only executes inside the
/// oracle band, so a permissionless settle can never give proceeds away.
///
/// This is the mirror of `rfq.move` with the two coins swapped: the
/// escrow / strict-increment / anti-snipe / push-refund / coupled-finalize
/// machinery is identical; only the bid direction and the price floor
/// differ. Proceeds swaps only ever originate from a vault, so this module
/// is venue-coupled only — there is no public seller/settle path, and no
/// bucket (the swap runs post-write, often post-expiry, so there is no
/// expiry coupling to recover from).
module options_protocol::swap_auction;

use sui::balance::{Self, Balance};
use sui::clock::Clock;
use sui::coin::{Self, Coin};

use options_protocol::errors;
use options_protocol::events;

/// Minimum auction duration, so MMs can react to `SwapRfqCreated`.
/// Mirrors `rfq::MIN_DURATION_MS`.
const MIN_DURATION_MS: u64 = 300_000; // 5 minutes

const BPS_DENOM: u128 = 10_000;

public struct SwapAuction<phantom Underlying, phantom Settlement> has key {
    id: UID,
    /// Settlement escrowed for this swap; handed to the winner at settle.
    settlement: Balance<Settlement>,
    /// == settlement.value(), cached for reads.
    amount_s: u64,
    /// Minimum underlying the vault will accept (underlying
    /// smallest-units). Derived from the open-time Pyth cross less the
    /// vault's slippage; bids below it are rejected. The *binding* band
    /// check is re-applied against a fresh cross at settle, so a price
    /// move after open can still void an in-reserve bid.
    reserve_underlying: u64,
    created_ms: u64,
    deadline_ms: u64,
    /// Anti-snipe: a best bid landing inside `snipe_window_ms` of the
    /// deadline pushes it out by `snipe_extension_ms`, capped at
    /// `max_deadline_ms`.
    snipe_window_ms: u64,
    snipe_extension_ms: u64,
    max_deadline_ms: u64,
    /// Minimum improvement over the current best, in bps of the best.
    min_increment_bps: u64,
    /// Current best bid; the underlying itself is escrowed in `bid_escrow`
    /// (its value IS the best bid — no duplicate field to drift).
    best_bidder: Option<address>,
    bid_escrow: Balance<Underlying>,
    /// Originating vault ID. Origin-gating on the settle path + indexing.
    origin: ID,
}

/// The winning side of a finalized swap, handed back to the vault to
/// absorb in its own transaction.
public struct FinalizedSwap<phantom Underlying> {
    bidder: address,
    underlying: Balance<Underlying>,
}

/// Identity/params of a finalized swap, for event emission after the
/// object is gone.
public struct SwapReceipt has copy, drop {
    swap_id: ID,
    origin: ID,
    amount_s: u64,
    reserve_underlying: u64,
}

/// Open a coupled swap auction: escrow `settlement` and start the clock.
/// Venue-only — proceeds swaps always originate from a vault, whose
/// `settle_swap_rfq` is the only resolver.
public(package) fun create_coupled<Underlying, Settlement>(
    settlement: Balance<Settlement>,
    reserve_underlying: u64,
    duration_ms: u64,
    snipe_window_ms: u64,
    snipe_extension_ms: u64,
    max_extension_ms: u64,
    min_increment_bps: u64,
    origin: ID,
    clock: &Clock,
    ctx: &mut TxContext,
): ID {
    let amount_s = settlement.value();
    assert!(amount_s > 0, errors::zero_amount());
    assert!(duration_ms >= MIN_DURATION_MS, errors::rfq_duration_too_short());
    let now = clock.timestamp_ms();
    let deadline_ms = now + duration_ms;
    let max_deadline_ms = deadline_ms + max_extension_ms;

    let swap = SwapAuction<Underlying, Settlement> {
        id: object::new(ctx),
        settlement,
        amount_s,
        reserve_underlying,
        created_ms: now,
        deadline_ms,
        snipe_window_ms,
        snipe_extension_ms,
        max_deadline_ms,
        min_increment_bps,
        best_bidder: option::none(),
        bid_escrow: balance::zero<Underlying>(),
        origin,
    };
    let swap_id = object::id(&swap);
    events::emit_swap_rfq_created(
        swap_id,
        origin,
        amount_s,
        reserve_underlying,
        deadline_ms,
        max_deadline_ms,
        min_increment_bps,
    );
    transfer::share_object(swap);
    swap_id
}

/// Escrow a bid of underlying. Must beat `max(reserve, best ×
/// (1 + increment))` and strictly beat the standing best. The previous
/// best is refunded by push transfer — always succeeds on Sui, no
/// re-entry, no blocking. Mirrors `rfq::bid` with the coins swapped.
public fun bid<Underlying, Settlement>(
    swap: &mut SwapAuction<Underlying, Settlement>,
    underlying_in: Coin<Underlying>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let now = clock.timestamp_ms();
    assert!(now < swap.deadline_ms, errors::rfq_closed());

    let value = underlying_in.value();
    let previous = swap.bid_escrow.value();
    let floor = if (swap.best_bidder.is_some()) {
        // Ceiling division so a non-zero increment always forces a real
        // improvement; the strict `>` handles min_increment_bps == 0.
        let with_increment = (
            ((previous as u128) * (BPS_DENOM + (swap.min_increment_bps as u128))
                + BPS_DENOM - 1) / BPS_DENOM
        ) as u64;
        assert!(value > previous, errors::rfq_bid_too_low());
        with_increment.max(swap.reserve_underlying)
    } else {
        swap.reserve_underlying
    };
    assert!(value >= floor, errors::rfq_bid_too_low());

    // Refund the outbid party, if any.
    if (swap.best_bidder.is_some()) {
        let refund = coin::from_balance(swap.bid_escrow.withdraw_all(), ctx);
        transfer::public_transfer(refund, *swap.best_bidder.borrow());
    };
    swap.bid_escrow.join(underlying_in.into_balance());
    swap.best_bidder = option::some(ctx.sender());

    // Anti-snipe: late best bids extend the deadline (capped).
    if (swap.deadline_ms - now < swap.snipe_window_ms) {
        let extended = now + swap.snipe_extension_ms;
        swap.deadline_ms = extended.min(swap.max_deadline_ms);
    };

    events::emit_swap_rfq_bid(
        object::id(swap),
        ctx.sender(),
        value,
        previous,
        swap.deadline_ms,
    );
}

/// Consume a closed auction and hand its parts back to the vault's
/// `settle_swap_rfq`, which applies the fresh-Pyth band check and routes
/// the balances. Returns `(winner, settlement, receipt)`; `winner ==
/// none` ⇒ no bids and `settlement` is the full escrow to return to the
/// vault. There is no `finalize_expired` twin: with no bucket coupling,
/// settle is always reachable once the deadline passes, so the escrow can
/// never strand.
public(package) fun finalize<Underlying, Settlement>(
    swap: SwapAuction<Underlying, Settlement>,
    clock: &Clock,
): (Option<FinalizedSwap<Underlying>>, Balance<Settlement>, SwapReceipt) {
    assert!(clock.timestamp_ms() >= swap.deadline_ms, errors::rfq_not_closed());
    let SwapAuction {
        id,
        settlement,
        amount_s,
        reserve_underlying,
        created_ms: _,
        deadline_ms: _,
        snipe_window_ms: _,
        snipe_extension_ms: _,
        max_deadline_ms: _,
        min_increment_bps: _,
        mut best_bidder,
        bid_escrow,
        origin,
    } = swap;
    let receipt = SwapReceipt {
        swap_id: id.to_inner(),
        origin,
        amount_s,
        reserve_underlying,
    };
    id.delete();

    let winner = if (best_bidder.is_some()) {
        option::some(FinalizedSwap {
            bidder: best_bidder.extract(),
            underlying: bid_escrow,
        })
    } else {
        bid_escrow.destroy_zero();
        option::none()
    };
    best_bidder.destroy_none();
    (winner, settlement, receipt)
}

// ---- FinalizedSwap / SwapReceipt accessors (vault settle path) ----

public(package) fun unpack_swap<U>(b: FinalizedSwap<U>): (address, Balance<U>) {
    let FinalizedSwap { bidder, underlying } = b;
    (bidder, underlying)
}

public(package) fun receipt_swap_id(r: &SwapReceipt): ID { r.swap_id }
public(package) fun receipt_origin(r: &SwapReceipt): ID { r.origin }
public(package) fun receipt_amount_s(r: &SwapReceipt): u64 { r.amount_s }
public(package) fun receipt_reserve_underlying(r: &SwapReceipt): u64 { r.reserve_underlying }

// ---- getters (off-chain reads: keeper + mm-bot) ----

public fun amount_s<U, S>(swap: &SwapAuction<U, S>): u64 { swap.amount_s }
public fun reserve_underlying<U, S>(swap: &SwapAuction<U, S>): u64 { swap.reserve_underlying }
public fun deadline_ms<U, S>(swap: &SwapAuction<U, S>): u64 { swap.deadline_ms }
public fun max_deadline_ms<U, S>(swap: &SwapAuction<U, S>): u64 { swap.max_deadline_ms }
public fun origin<U, S>(swap: &SwapAuction<U, S>): ID { swap.origin }
public fun best_bidder<U, S>(swap: &SwapAuction<U, S>): Option<address> { swap.best_bidder }

/// The standing best bid in underlying smallest-units (0 when no bids).
public fun best_bid<U, S>(swap: &SwapAuction<U, S>): u64 {
    swap.bid_escrow.value()
}
