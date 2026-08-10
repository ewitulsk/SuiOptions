//! Build router `HopLadder`s from live book snapshots (spec §5.8 step 2).
//!
//! Rates are netted for fees at the market's current rate — a conservative
//! stand-in for `min(order.max_fee_bps, market fee)` (intake only accepts
//! orders whose signed ceiling is meaningful, and the route's end min-out
//! guard protects the taker regardless).

use exchange_book::Book;
use exchange_types::Market;
use exchange_router::{HopLadder, LiquiditySegment};

const BPS_DENOM: u64 = 10_000;

/// The two directed ladders (base->quote via bids, quote->base via asks) of
/// one market's current book.
pub fn ladders_for_market(market: &Market, book: &Book) -> Vec<HopLadder> {
    let fee_keep = BPS_DENOM - market.current_fee_bps.min(BPS_DENOM);
    let mut base_to_quote = Vec::new(); // consume bids, best (highest) first
    let mut quote_to_base = Vec::new(); // consume asks, best (lowest) first

    let mut orders: Vec<_> = book.iter_orders().filter(|o| o.available_base() > 0).collect();
    orders.sort_by_key(|o| o.price_ticks);

    // quote per base = price_ticks * tick_size / lot_size
    for o in &orders {
        match o.side {
            exchange_types::Side::Bid => {
                // sell base into this bid: out(quote) = in(base) * ticks*tick/lot, net fee
                base_to_quote.push(LiquiditySegment {
                    digest: o.digest,
                    max_in: o.available_base(),
                    num: o
                        .price_ticks
                        .saturating_mul(market.tick_size)
                        .saturating_mul(fee_keep),
                    den: market.lot_size.saturating_mul(BPS_DENOM),
                });
            }
            exchange_types::Side::Ask => {
                // buy base from this ask: out(base) = in(quote) * lot/(ticks*tick), net fee
                let quote_capacity = (o.available_base() as u128
                    * o.price_ticks as u128
                    * market.tick_size as u128
                    / market.lot_size as u128) as u64;
                quote_to_base.push(LiquiditySegment {
                    digest: o.digest,
                    max_in: quote_capacity,
                    num: market.lot_size.saturating_mul(fee_keep),
                    den: o
                        .price_ticks
                        .saturating_mul(market.tick_size)
                        .saturating_mul(BPS_DENOM),
                });
            }
        }
    }
    // bids: best = highest price first
    base_to_quote.reverse();

    let mut out = Vec::new();
    if !base_to_quote.is_empty() {
        out.push(HopLadder {
            market: market.registry_id,
            from: market.base.clone(),
            to: market.quote.clone(),
            segments: base_to_quote,
        });
    }
    if !quote_to_base.is_empty() {
        out.push(HopLadder {
            market: market.registry_id,
            from: market.quote.clone(),
            to: market.base.clone(),
            segments: quote_to_base,
        });
    }
    out
}
