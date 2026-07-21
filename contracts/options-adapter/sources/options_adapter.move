/// Options-protocol adapter for the curated trading vault: the vault as
/// covered-call (and cash-secured-put) WRITER via the on-chain RFQ
/// (docs/trading-vault/01-contract-design.md §7 mode 1, decisions in
/// docs/vault-curator-product.md).
///
/// Mirrors `options_vault`'s inlined pattern rather than the
/// `options_rfq` package: the curator escrows vault funds straight into
/// a generic `auction::create_coupled` with THIS module's witness as
/// settle authority and the VAULT as origin; market makers bid premium
/// on the shared auction; the permissionless settle crank writes into
/// the bucket and absorbs the `Position` + net premium back into the
/// vault. (`options_rfq` routes outputs to plain addresses, which is the
/// wrong shape for a shared-object vault.)
///
/// While an auction is live the escrow sits outside the vault, so each
/// open RFQ is represented by an `RfqTicket` position appraised at
/// escrow cost. Option `Position`s are appraised at their conservative
/// exercise-now mark: exercised range at strike proceeds, unexercised
/// range at min(spot, strike) — premium upside is never marked.
///
/// The vault also acts as auction BIDDER (the mm-bot desk buying option
/// coins, SO-299): `bid_on_auction` escrows a bid from vault funds into
/// any premium/swap auction with all outputs routed to a `BidTicket`
/// position's own object address — see the `BidTicket` docs for why the
/// transfer-to-object payout is the burn proof.
module options_adapter::options_adapter;

use std::type_name::{Self, TypeName};
use sui::clock::Clock;
use sui::coin::{Self, Coin};
use sui::event;
use sui::transfer::Receiving;

use auction::auction::{Self as auctions, Auction};
use options_core::admin::ProtocolConfig;
use options_core::bucket::{Self, Bucket};
use options_core::position::{Self, Position};
use options_core::put_bucket::{Self, PutBucket};
use options_core::treasury::Treasury;

use trading_vault::price::{Self, PriceAttestation};
use trading_vault::registry::{IntegrationRegistry, VaultProtocolConfig};
use trading_vault::vault::{Self, Appraisal, CuratorCap, TradingVault};

const E_WRONG_TICKET: u64 = 1;
const E_BUCKET_MISMATCH: u64 = 2;
const E_BUCKET_LIVE: u64 = 3;
const E_PRICE_ASSET_MISMATCH: u64 = 4;
const E_MISSING_ATTESTATION: u64 = 5;
const E_VALUE_OVERFLOW: u64 = 6;
const E_ESCROW_TYPE_MISMATCH: u64 = 7;
const E_AMOUNT_OVERFLOW: u64 = 8;
const E_VAULT_NOT_OPEN: u64 = 9;
const E_STILL_BEST_BIDDER: u64 = 10;
const E_REFUND_MISMATCH: u64 = 11;
const E_WIN_TYPE_MISMATCH: u64 = 12;
const E_WIN_MISMATCH: u64 = 13;
const E_ZERO_AMOUNT: u64 = 14;
const E_SPREAD_POSITION: u64 = 15;

/// Adapter witness: allowlist in `IntegrationRegistry`; also the
/// auctions' settle authority.
public struct OptionsAdapter has drop {}

/// Open-RFQ marker held as a vault position while the escrow lives in
/// the shared `Auction`. Appraised at escrow cost.
public struct RfqTicket has key, store {
    id: UID,
    vault_id: ID,
    auction_id: ID,
    bucket_id: ID,
    /// Option contracts being written.
    write_amount: u64,
    /// Escrow paid in: underlying (calls) or settlement collateral (puts).
    escrow_amount: u64,
    escrow_type: TypeName,
    is_put: bool,
}

/// Live vault-funded bid on someone ELSE's auction (the desk BUYING
/// option coins): the bid escrow left the vault into the shared
/// `Auction`, so the ticket holds its value in NAV at cost.
///
/// Output routing is the load-bearing design: the bid is placed with the
/// TICKET's own object address as both the bidder identity (refund
/// target) and the `token_recipient` (win target), so every auction
/// output — outbid refund, early-settle refund, won tokens — lands as a
/// transfer-to-object at the ticket. The burn cranks below RECEIVE that
/// coin into the vault in the same transaction that burns the ticket:
/// the coin is unforgeable-without-value proof the auction paid out, no
/// burn can drop NAV below the ticket's cost mark, and no path can pay
/// the curator. (Auction objects are deleted at settle, so no gate on
/// the auction itself can cover the won/refunded-after-settle cases.)
public struct BidTicket has key, store {
    id: UID,
    vault_id: ID,
    auction_id: ID,
    /// The bucket whose option coins the auction sells (informational —
    /// caller-supplied, for off-chain classification).
    bucket_id: ID,
    /// Bid escrowed into the auction, in `escrow_type` (the auction's
    /// Bid asset). A refund returns exactly this.
    escrow_amount: u64,
    escrow_type: TypeName,
    /// What a win delivers to the ticket address: `win_amount` of
    /// `win_type` (the bucket's option coin for coupled RFQ auctions,
    /// the auction's escrow asset for plain swap auctions).
    win_type: TypeName,
    win_amount: u64,
    is_put: bool,
    placed_at_ms: u64,
}

public struct BidPlaced has copy, drop {
    vault_id: ID,
    ticket_id: ID,
    auction_id: ID,
    bucket_id: ID,
    escrow_amount: u64,
    win_type: TypeName,
    win_amount: u64,
    is_put: bool,
}

public struct BidReclaimed has copy, drop {
    vault_id: ID,
    ticket_id: ID,
    auction_id: ID,
    refunded: u64,
}

public struct BidRedeemed has copy, drop {
    vault_id: ID,
    ticket_id: ID,
    auction_id: ID,
    win_type: TypeName,
    win_amount: u64,
}

public struct RfqOpened has copy, drop {
    vault_id: ID,
    ticket_id: ID,
    auction_id: ID,
    bucket_id: ID,
    write_amount: u64,
    escrow_amount: u64,
    reserve_premium: u64,
    is_put: bool,
}

public struct RfqSettled has copy, drop {
    vault_id: ID,
    ticket_id: ID,
    auction_id: ID,
    bucket_id: ID,
    filled: bool,
    net_premium: u64,
    fee: u64,
    position_id: Option<ID>,
    is_put: bool,
}

public struct PositionRedeemed has copy, drop {
    vault_id: ID,
    bucket_id: ID,
    position_id: ID,
    underlying_out: u64,
    settlement_out: u64,
    is_put: bool,
}

// ═══════════════════════════ call flows ═══════════════════════════

/// Curator escrows `escrow_amount` underlying into a premium auction for
/// `bucket`. One RFQ writes exactly the escrow (1:1 with contracts).
public fun open_call_rfq<U, S, C>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    bucket: &Bucket<U, S, C>,
    escrow_amount: u64,
    reserve_premium: u64,
    duration_ms: u64,
    snipe_window_ms: u64,
    snipe_extension_ms: u64,
    max_extension_ms: u64,
    min_increment_bps: u64,
    clock: &Clock,
    ctx: &mut TxContext,
): ID {
    let mut s = vault::begin_session(vault, cap, reg, OptionsAdapter {});
    let escrow = vault::take<U>(vault, &mut s, escrow_amount);
    let vault_id = vault::session_vault_id(&s);
    let auction_id = auctions::create_coupled<U, S, OptionsAdapter>(
        OptionsAdapter {},
        escrow,
        reserve_premium,
        duration_ms,
        snipe_window_ms,
        snipe_extension_ms,
        max_extension_ms,
        min_increment_bps,
        vault_id,
        clock,
        ctx,
    );
    let ticket = RfqTicket {
        id: object::new(ctx),
        vault_id,
        auction_id,
        bucket_id: object::id(bucket),
        write_amount: escrow_amount,
        escrow_amount,
        escrow_type: type_name::with_defining_ids<U>(),
        is_put: false,
    };
    let ticket_id = object::id(&ticket);
    event::emit(RfqOpened {
        vault_id,
        ticket_id,
        auction_id,
        bucket_id: object::id(bucket),
        write_amount: escrow_amount,
        escrow_amount,
        reserve_premium,
        is_put: false,
    });
    vault::put_position(vault, &mut s, ticket);
    vault::end_session(vault, s);
    ticket_id
}

/// Permissionless settle after the auction deadline: winner → write into
/// the bucket, absorb Position + net premium; no winner → escrow back.
public fun settle_call_rfq<U, S, C>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    ticket_id: ID,
    auction: Auction<U, S>,
    bucket: &mut Bucket<U, S, C>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    clock: &Clock,
    ctx: &mut TxContext,
): Option<ID> {
    let mut s = vault::begin_crank_session(vault, reg, OptionsAdapter {});
    let ticket = vault::take_position<RfqTicket>(vault, &mut s, ticket_id);
    let (auction_id, bucket_id) = check_and_burn_ticket(&s, ticket, &auction, false);
    assert!(bucket_id == object::id(bucket), E_BUCKET_MISMATCH);

    let (mut winner, escrow, _receipt) =
        auctions::finalize<U, S, OptionsAdapter>(OptionsAdapter {}, auction, clock);
    let vault_id = vault::session_vault_id(&s);
    if (winner.is_some()) {
        let (_bidder, call_recipient, premium) = auctions::unpack_bid(winner.extract());
        winner.destroy_none();
        let (net, fee) = bucket::skim_fee(config, treasury, premium);
        let net_premium = net.value();
        let (pos, call) = bucket::write_collateralized_balance(bucket, escrow, clock, ctx);
        let position_id = object::id(&pos);
        transfer::public_transfer(call, call_recipient);
        vault::put_position(vault, &mut s, pos);
        vault::put<S>(vault, &mut s, net);
        event::emit(RfqSettled {
            vault_id,
            ticket_id,
            auction_id,
            bucket_id,
            filled: true,
            net_premium,
            fee,
            position_id: option::some(position_id),
            is_put: false,
        });
        vault::end_session(vault, s);
        return option::some(position_id)
    } else {
        winner.destroy_none();
        vault::put<U>(vault, &mut s, escrow);
        event::emit(RfqSettled {
            vault_id,
            ticket_id,
            auction_id,
            bucket_id,
            filled: false,
            net_premium: 0,
            fee: 0,
            position_id: option::none(),
            is_put: false,
        });
    };
    vault::end_session(vault, s);
    option::none()
}

/// Recovery settle when the bucket died under the auction (expired or
/// invalidated): refund the best bid, reclaim the escrow.
public fun settle_call_rfq_expired<U, S, C>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    ticket_id: ID,
    auction: Auction<U, S>,
    bucket: &Bucket<U, S, C>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(
        clock.timestamp_ms() >= bucket::expiry_ms(bucket) || bucket::invalidated(bucket),
        E_BUCKET_LIVE,
    );
    let mut s = vault::begin_crank_session(vault, reg, OptionsAdapter {});
    let ticket = vault::take_position<RfqTicket>(vault, &mut s, ticket_id);
    let (auction_id, bucket_id) = check_and_burn_ticket(&s, ticket, &auction, false);
    assert!(bucket_id == object::id(bucket), E_BUCKET_MISMATCH);

    let (mut winner, escrow, _receipt) =
        auctions::finalize_early<U, S, OptionsAdapter>(OptionsAdapter {}, auction);
    let vault_id = vault::session_vault_id(&s);
    if (winner.is_some()) {
        let (bidder, _recipient, premium) = auctions::unpack_bid(winner.extract());
        transfer::public_transfer(coin::from_balance(premium, ctx), bidder);
    };
    winner.destroy_none();
    vault::put<U>(vault, &mut s, escrow);
    event::emit(RfqSettled {
        vault_id,
        ticket_id,
        auction_id,
        bucket_id,
        filled: false,
        net_premium: 0,
        fee: 0,
        position_id: option::none(),
        is_put: false,
    });
    vault::end_session(vault, s);
}

/// Permissionless post-expiry redemption of a custodied call Position.
public fun redeem_call_position<U, S, C>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    bucket: &mut Bucket<U, S, C>,
    position_id: ID,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_crank_session(vault, reg, OptionsAdapter {});
    let pos = vault::take_position<Position>(vault, &mut s, position_id);
    let (u_out, s_out) = bucket::redeem_position(bucket, pos, clock, ctx);
    event::emit(PositionRedeemed {
        vault_id: vault::session_vault_id(&s),
        bucket_id: object::id(bucket),
        position_id,
        underlying_out: u_out.value(),
        settlement_out: s_out.value(),
        is_put: false,
    });
    vault::put<U>(vault, &mut s, u_out.into_balance());
    vault::put<S>(vault, &mut s, s_out.into_balance());
    vault::end_session(vault, s);
}

// ═══════════════════════════ put flows ═══════════════════════════

/// Curator escrows exact cash collateral for `write_amount` puts.
public fun open_put_rfq<U, S, P>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    bucket: &PutBucket<U, S, P>,
    write_amount: u64,
    reserve_premium: u64,
    duration_ms: u64,
    snipe_window_ms: u64,
    snipe_extension_ms: u64,
    max_extension_ms: u64,
    min_increment_bps: u64,
    clock: &Clock,
    ctx: &mut TxContext,
): ID {
    let escrow_amount = put_bucket::required_collateral(bucket, write_amount);
    let mut s = vault::begin_session(vault, cap, reg, OptionsAdapter {});
    let escrow = vault::take<S>(vault, &mut s, escrow_amount);
    let vault_id = vault::session_vault_id(&s);
    let auction_id = auctions::create_coupled<S, S, OptionsAdapter>(
        OptionsAdapter {},
        escrow,
        reserve_premium,
        duration_ms,
        snipe_window_ms,
        snipe_extension_ms,
        max_extension_ms,
        min_increment_bps,
        vault_id,
        clock,
        ctx,
    );
    let ticket = RfqTicket {
        id: object::new(ctx),
        vault_id,
        auction_id,
        bucket_id: object::id(bucket),
        write_amount,
        escrow_amount,
        escrow_type: type_name::with_defining_ids<S>(),
        is_put: true,
    };
    let ticket_id = object::id(&ticket);
    event::emit(RfqOpened {
        vault_id,
        ticket_id,
        auction_id,
        bucket_id: object::id(bucket),
        write_amount,
        escrow_amount,
        reserve_premium,
        is_put: true,
    });
    vault::put_position(vault, &mut s, ticket);
    vault::end_session(vault, s);
    ticket_id
}

public fun settle_put_rfq<U, S, P>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    ticket_id: ID,
    auction: Auction<S, S>,
    bucket: &mut PutBucket<U, S, P>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    clock: &Clock,
    ctx: &mut TxContext,
): Option<ID> {
    let mut s = vault::begin_crank_session(vault, reg, OptionsAdapter {});
    let ticket = vault::take_position<RfqTicket>(vault, &mut s, ticket_id);
    let write_amount = ticket.write_amount;
    let (auction_id, bucket_id) = check_and_burn_ticket(&s, ticket, &auction, true);
    assert!(bucket_id == object::id(bucket), E_BUCKET_MISMATCH);

    let (mut winner, escrow, _receipt) =
        auctions::finalize<S, S, OptionsAdapter>(OptionsAdapter {}, auction, clock);
    let vault_id = vault::session_vault_id(&s);
    if (winner.is_some()) {
        let (_bidder, put_recipient, premium) = auctions::unpack_bid(winner.extract());
        winner.destroy_none();
        let (net, fee) = bucket::skim_fee(config, treasury, premium);
        let net_premium = net.value();
        let (pos, put) =
            put_bucket::write_collateralized_balance(bucket, escrow, write_amount, clock, ctx);
        let position_id = object::id(&pos);
        transfer::public_transfer(put, put_recipient);
        vault::put_position(vault, &mut s, pos);
        vault::put<S>(vault, &mut s, net);
        event::emit(RfqSettled {
            vault_id,
            ticket_id,
            auction_id,
            bucket_id,
            filled: true,
            net_premium,
            fee,
            position_id: option::some(position_id),
            is_put: true,
        });
        vault::end_session(vault, s);
        return option::some(position_id)
    } else {
        winner.destroy_none();
        vault::put<S>(vault, &mut s, escrow);
        event::emit(RfqSettled {
            vault_id,
            ticket_id,
            auction_id,
            bucket_id,
            filled: false,
            net_premium: 0,
            fee: 0,
            position_id: option::none(),
            is_put: true,
        });
    };
    vault::end_session(vault, s);
    option::none()
}

public fun settle_put_rfq_expired<U, S, P>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    ticket_id: ID,
    auction: Auction<S, S>,
    bucket: &PutBucket<U, S, P>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(
        clock.timestamp_ms() >= put_bucket::expiry_ms(bucket) || put_bucket::invalidated(bucket),
        E_BUCKET_LIVE,
    );
    let mut s = vault::begin_crank_session(vault, reg, OptionsAdapter {});
    let ticket = vault::take_position<RfqTicket>(vault, &mut s, ticket_id);
    let (auction_id, bucket_id) = check_and_burn_ticket(&s, ticket, &auction, true);
    assert!(bucket_id == object::id(bucket), E_BUCKET_MISMATCH);

    let (mut winner, escrow, _receipt) =
        auctions::finalize_early<S, S, OptionsAdapter>(OptionsAdapter {}, auction);
    let vault_id = vault::session_vault_id(&s);
    if (winner.is_some()) {
        let (bidder, _recipient, premium) = auctions::unpack_bid(winner.extract());
        transfer::public_transfer(coin::from_balance(premium, ctx), bidder);
    };
    winner.destroy_none();
    vault::put<S>(vault, &mut s, escrow);
    event::emit(RfqSettled {
        vault_id,
        ticket_id,
        auction_id,
        bucket_id,
        filled: false,
        net_premium: 0,
        fee: 0,
        position_id: option::none(),
        is_put: true,
    });
    vault::end_session(vault, s);
}

public fun redeem_put_position<U, S, P>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    bucket: &mut PutBucket<U, S, P>,
    position_id: ID,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_crank_session(vault, reg, OptionsAdapter {});
    let pos = vault::take_position<Position>(vault, &mut s, position_id);
    let (u_out, s_out) = put_bucket::redeem_position(bucket, pos, clock, ctx);
    event::emit(PositionRedeemed {
        vault_id: vault::session_vault_id(&s),
        bucket_id: object::id(bucket),
        position_id,
        underlying_out: u_out.value(),
        settlement_out: s_out.value(),
        is_put: true,
    });
    vault::put<U>(vault, &mut s, u_out.into_balance());
    vault::put<S>(vault, &mut s, s_out.into_balance());
    vault::end_session(vault, s);
}

// ═══════════════════ vault-funded auction bids ═══════════════════

/// Curator bids `bid_amount` of the vault's `B` on a live auction
/// selling `E` (the desk BUYING option coins from retail). Open vaults
/// only — a Closing vault unwinds, it does not take new risk.
///
/// `W`/`win_amount` pin what a win must deliver (the bucket's option
/// coin and the write amount for coupled RFQ auctions; `E` and the
/// auction amount for plain swap auctions). Both auction outputs are
/// routed to the minted `BidTicket`'s own address (see the struct docs);
/// a wrong `W`/`win_amount` can only strand the ticket's redemption —
/// it can never route value to the curator.
public fun bid_on_auction<E, B, W>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    auction: &mut Auction<E, B>,
    bid_amount: u64,
    win_amount: u64,
    bucket_id: ID,
    is_put: bool,
    clock: &Clock,
    ctx: &mut TxContext,
): ID {
    assert!(vault::is_open(vault), E_VAULT_NOT_OPEN);
    assert!(bid_amount > 0 && win_amount > 0, E_ZERO_AMOUNT);
    let mut s = vault::begin_session(vault, cap, reg, OptionsAdapter {});
    let vault_id = vault::session_vault_id(&s);
    let ticket = BidTicket {
        id: object::new(ctx),
        vault_id,
        auction_id: object::id(auction),
        bucket_id,
        escrow_amount: bid_amount,
        escrow_type: type_name::with_defining_ids<B>(),
        win_type: type_name::with_defining_ids<W>(),
        win_amount,
        is_put,
        placed_at_ms: clock.timestamp_ms(),
    };
    let ticket_id = object::id(&ticket);
    let ticket_addr = ticket_id.to_address();
    let escrow = vault::take<B>(vault, &mut s, bid_amount);
    auctions::bid_with_recipient(
        auction,
        coin::from_balance(escrow, ctx),
        ticket_addr,
        ticket_addr,
        clock,
        ctx,
    );
    event::emit(BidPlaced {
        vault_id,
        ticket_id,
        auction_id: object::id(auction),
        bucket_id,
        escrow_amount: bid_amount,
        win_type: type_name::with_defining_ids<W>(),
        win_amount,
        is_put,
    });
    vault::put_position(vault, &mut s, ticket);
    vault::end_session(vault, s);
    ticket_id
}

/// Permissionless: reclaim an OUTBID ticket while its auction is still
/// live — the auction already push-transferred the full refund to the
/// ticket address when the outbid landed. Receives the refund into the
/// vault and burns the ticket. Aborts while the ticket is still the best
/// bidder (its escrow is live in the auction; a donated look-alike coin
/// cannot force an early burn through this path).
public fun reclaim_outbid_ticket<E, B>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    ticket_id: ID,
    auction: &Auction<E, B>,
    refund: Receiving<Coin<B>>,
) {
    let mut s = vault::begin_crank_session(vault, reg, OptionsAdapter {});
    let mut ticket = vault::take_position<BidTicket>(vault, &mut s, ticket_id);
    assert!(ticket.auction_id == object::id(auction), E_WRONG_TICKET);
    let best = auctions::best_bidder(auction);
    let ticket_addr = ticket_id.to_address();
    assert!(
        best.is_none() || *best.borrow() != ticket_addr,
        E_STILL_BEST_BIDDER,
    );
    let coin = transfer::public_receive(&mut ticket.id, refund);
    reclaim_impl(vault, &mut s, ticket, coin);
    vault::end_session(vault, s);
}

/// Permissionless: reclaim a REFUNDED ticket after its auction object is
/// gone (a third party settled a won-by-someone-else auction, or the
/// seller's dead-bucket recovery refunded the standing bid). The
/// received coin must equal the escrow exactly, so the burn always
/// returns the ticket's full cost mark to the vault; the narrow residual
/// (an adversary donating an exact-amount coin to trigger the burn while
/// the real escrow is still live) costs the adversary at least what it
/// strands and leaves NAV whole at cost.
public fun reclaim_refunded_ticket<B>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    ticket_id: ID,
    refund: Receiving<Coin<B>>,
) {
    let mut s = vault::begin_crank_session(vault, reg, OptionsAdapter {});
    let mut ticket = vault::take_position<BidTicket>(vault, &mut s, ticket_id);
    let coin = transfer::public_receive(&mut ticket.id, refund);
    reclaim_impl(vault, &mut s, ticket, coin);
    vault::end_session(vault, s);
}

/// Permissionless: redeem a WON ticket after the auction settled — the
/// settle routed `win_amount` of `W` to the ticket address (the
/// `token_recipient`). Receives the winnings into the vault's free
/// balances (option coins appraise via the options oracle) and burns the
/// ticket. The pinned type + amount mean a burn can only ever be
/// triggered by delivering at least the win itself.
public fun redeem_won_ticket<W>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    ticket_id: ID,
    winnings: Receiving<Coin<W>>,
) {
    let mut s = vault::begin_crank_session(vault, reg, OptionsAdapter {});
    let mut ticket = vault::take_position<BidTicket>(vault, &mut s, ticket_id);
    assert!(type_name::with_defining_ids<W>() == ticket.win_type, E_WIN_TYPE_MISMATCH);
    let coin = transfer::public_receive(&mut ticket.id, winnings);
    assert!(coin.value() >= ticket.win_amount, E_WIN_MISMATCH);
    let BidTicket { id, vault_id, auction_id, win_type, .. } = ticket;
    id.delete();
    assert!(vault_id == vault::session_vault_id(&s), E_WRONG_TICKET);
    event::emit(BidRedeemed {
        vault_id,
        ticket_id,
        auction_id,
        win_type,
        win_amount: coin.value(),
    });
    vault::put<W>(vault, &mut s, coin.into_balance());
    vault::end_session(vault, s);
}

/// Shared refund-side burn: the received coin must be the full escrow,
/// so NAV holds exactly at the ticket's cost mark across the burn.
fun reclaim_impl<B>(
    vault: &mut TradingVault,
    s: &mut trading_vault::vault::Session,
    ticket: BidTicket,
    coin: Coin<B>,
) {
    let BidTicket { id, vault_id, auction_id, escrow_amount, escrow_type, .. } = ticket;
    let ticket_id = id.to_inner();
    id.delete();
    assert!(vault_id == vault::session_vault_id(s), E_WRONG_TICKET);
    assert!(type_name::with_defining_ids<B>() == escrow_type, E_ESCROW_TYPE_MISMATCH);
    assert!(coin.value() == escrow_amount, E_REFUND_MISMATCH);
    event::emit(BidReclaimed { vault_id, ticket_id, auction_id, refunded: escrow_amount });
    vault::put<B>(vault, s, coin.into_balance());
}

// ══════════════════════════════ appraisal ══════════════════════════════

/// An open RFQ marks at escrow cost (premium upside never marked).
public fun appraise_rfq_ticket<E>(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    appraisal: &mut Appraisal,
    ticket_id: ID,
    att: Option<PriceAttestation>,
    clock: &Clock,
) {
    let ticket: &RfqTicket = vault::borrow_position(vault, ticket_id);
    assert!(type_name::with_defining_ids<E>() == ticket.escrow_type, E_ESCROW_TYPE_MISMATCH);
    let value = value_in_deposit(vault, cfg, ticket.escrow_type, ticket.escrow_amount, att, clock);
    assert!(value <= (std::u64::max_value!() as u128), E_VALUE_OVERFLOW);
    vault::record_position_value(vault, appraisal, OptionsAdapter {}, ticket_id, value as u64);
}

/// A live vault-funded bid marks at escrow cost, mirroring
/// `appraise_rfq_ticket` (win upside never marked).
public fun appraise_bid_ticket<B>(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    appraisal: &mut Appraisal,
    ticket_id: ID,
    att: Option<PriceAttestation>,
    clock: &Clock,
) {
    let ticket: &BidTicket = vault::borrow_position(vault, ticket_id);
    assert!(type_name::with_defining_ids<B>() == ticket.escrow_type, E_ESCROW_TYPE_MISMATCH);
    let value = value_in_deposit(vault, cfg, ticket.escrow_type, ticket.escrow_amount, att, clock);
    assert!(value <= (std::u64::max_value!() as u128), E_VALUE_OVERFLOW);
    vault::record_position_value(vault, appraisal, OptionsAdapter {}, ticket_id, value as u64);
}

/// Conservative exercise-now mark for a custodied call `Position`:
/// exercised range → strike proceeds (settlement); unexercised range →
/// min(spot, strike). Uses the bucket's own `required_settlement` math
/// so strike arithmetic is exact.
public fun appraise_call_position<U, S, C>(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    appraisal: &mut Appraisal,
    bucket: &Bucket<U, S, C>,
    position_id: ID,
    underlying_att: Option<PriceAttestation>,
    settlement_att: Option<PriceAttestation>,
    clock: &Clock,
) {
    let pos: &Position = vault::borrow_position(vault, position_id);
    assert!(position::bucket_id(pos) == object::id(bucket), E_BUCKET_MISMATCH);
    // A compressed range is escrow-backed, not pool-backed — it marks
    // via `vault_mm::appraise_call_spread_position` on the VaultMm
    // custody path (no adapter flow can custody a spread position).
    assert!(
        !bucket::range_overlaps_spread(
            bucket,
            position::range_start(pos),
            position::range_end(pos),
        ),
        E_SPREAD_POSITION,
    );
    let (exercised, unexercised) = split_ranges(
        position::range_start(pos),
        position::range_end(pos),
        bucket::exercise_cursor(bucket),
    );
    let u_type = type_name::with_defining_ids<U>();
    let s_type = type_name::with_defining_ids<S>();

    // Exercised range: strike proceeds already banked in the bucket.
    let exercised_s = bucket::required_settlement(bucket, exercised);
    let mut value = value_in_deposit(vault, cfg, s_type, exercised_s, settlement_att, clock);

    // Unexercised range: worst case min(spot, strike).
    if (unexercised > 0) {
        let spot_value =
            value_in_deposit(vault, cfg, u_type, unexercised, underlying_att, clock);
        let strike_s = bucket::required_settlement(bucket, unexercised);
        let strike_value = value_in_deposit(vault, cfg, s_type, strike_s, settlement_att, clock);
        value = value + spot_value.min(strike_value);
    };
    assert!(value <= (std::u64::max_value!() as u128), E_VALUE_OVERFLOW);
    vault::record_position_value(vault, appraisal, OptionsAdapter {}, position_id, value as u64);
}

/// Put twin: exercised range holds underlying (marked at spot);
/// unexercised range worst case min(spot, strike-collateral).
public fun appraise_put_position<U, S, P>(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    appraisal: &mut Appraisal,
    bucket: &PutBucket<U, S, P>,
    position_id: ID,
    underlying_att: Option<PriceAttestation>,
    settlement_att: Option<PriceAttestation>,
    clock: &Clock,
) {
    let pos: &Position = vault::borrow_position(vault, position_id);
    assert!(position::bucket_id(pos) == object::id(bucket), E_BUCKET_MISMATCH);
    // A compressed range is escrow-backed, not pool-backed — it marks
    // via `vault_mm::appraise_put_spread_position` on the VaultMm
    // custody path (no adapter flow can custody a spread position).
    assert!(
        !put_bucket::range_overlaps_spread(
            bucket,
            position::range_start(pos),
            position::range_end(pos),
        ),
        E_SPREAD_POSITION,
    );
    let (exercised, unexercised) = split_ranges(
        position::range_start(pos),
        position::range_end(pos),
        put_bucket::exercise_cursor(bucket),
    );
    let u_type = type_name::with_defining_ids<U>();
    let s_type = type_name::with_defining_ids<S>();

    let mut value = value_in_deposit(vault, cfg, u_type, exercised, underlying_att, clock);
    if (unexercised > 0) {
        let collateral_s = put_bucket::required_collateral(bucket, unexercised);
        let spot_value =
            value_in_deposit(vault, cfg, u_type, unexercised, underlying_att, clock);
        let collateral_value =
            value_in_deposit(vault, cfg, s_type, collateral_s, settlement_att, clock);
        value = value + spot_value.min(collateral_value);
    };
    assert!(value <= (std::u64::max_value!() as u128), E_VALUE_OVERFLOW);
    vault::record_position_value(vault, appraisal, OptionsAdapter {}, position_id, value as u64);
}

// ═══════════════════════════════ internals ═══════════════════════════════

/// Validates a ticket against its auction + session vault and burns it.
fun check_and_burn_ticket<E, B>(
    s: &trading_vault::vault::Session,
    ticket: RfqTicket,
    auction: &Auction<E, B>,
    expect_put: bool,
): (ID, ID) {
    let RfqTicket {
        id,
        vault_id,
        auction_id,
        bucket_id,
        write_amount: _,
        escrow_amount: _,
        escrow_type: _,
        is_put,
    } = ticket;
    id.delete();
    assert!(vault_id == vault::session_vault_id(s), E_WRONG_TICKET);
    assert!(auction_id == object::id(auction), E_WRONG_TICKET);
    assert!(auctions::origin(auction) == vault_id, E_WRONG_TICKET);
    assert!(is_put == expect_put, E_WRONG_TICKET);
    (auction_id, bucket_id)
}

fun split_ranges(range_start: u128, range_end: u128, cursor: u128): (u64, u64) {
    let amount = range_end - range_start;
    let exercised = if (cursor <= range_start) {
        0
    } else if (cursor >= range_end) {
        amount
    } else {
        cursor - range_start
    };
    let unexercised = amount - exercised;
    assert!(
        exercised <= (std::u64::max_value!() as u128)
            && unexercised <= (std::u64::max_value!() as u128),
        E_AMOUNT_OVERFLOW,
    );
    ((exercised as u64), (unexercised as u64))
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
    if (asset == vault::deposit_asset(vault)) {
        return amount as u128
    };
    assert!(att.is_some(), E_MISSING_ATTESTATION);
    let a = att.extract();
    assert!(price::asset(&a) == asset, E_PRICE_ASSET_MISMATCH);
    vault::check_attestation(vault, cfg, &a, clock);
    (((amount as u256) * (price::price(&a) as u256) / (price::price_scale() as u256)) as u128)
}

// ══════════════════════════════ getters ══════════════════════════════

public fun ticket_auction_id(t: &RfqTicket): ID { t.auction_id }

public fun ticket_bucket_id(t: &RfqTicket): ID { t.bucket_id }

public fun ticket_write_amount(t: &RfqTicket): u64 { t.write_amount }

public fun ticket_escrow_amount(t: &RfqTicket): u64 { t.escrow_amount }

public fun ticket_is_put(t: &RfqTicket): bool { t.is_put }

public fun bid_ticket_auction_id(t: &BidTicket): ID { t.auction_id }

public fun bid_ticket_bucket_id(t: &BidTicket): ID { t.bucket_id }

public fun bid_ticket_escrow_amount(t: &BidTicket): u64 { t.escrow_amount }

public fun bid_ticket_win_amount(t: &BidTicket): u64 { t.win_amount }

public fun bid_ticket_is_put(t: &BidTicket): bool { t.is_put }

public fun bid_ticket_placed_at_ms(t: &BidTicket): u64 { t.placed_at_ms }
