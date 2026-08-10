//! Pure ladder math: oracle spot → tick-snapped bid/ask levels → exact
//! order amounts.
//!
//! Everything here is integer math on the market's `(tick_size, lot_size)`
//! grid. Amounts are constructed as `base = lots * lot_size` and
//! `quote = lots * price_ticks * tick_size`, which makes the book's
//! divisibility check (`quote * lot / (base * tick)` exact) hold by
//! construction — the invariant the round-trip test pins.

use exchange_types::{Market, Order, Side, SuiAddress};
use serde::Deserialize;

/// One ladder level: half-spread away from mid, size in lots (whole base
/// tokens — 1 lot = `lot_size` base units).
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct LevelSpec {
    pub bps: u64,
    pub lots: u64,
}

/// A priced, sized level ready to be turned into an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LadderOrder {
    pub side: Side,
    pub price_ticks: u64,
    pub lots: u64,
}

/// Convert an oracle spot (quote raw units per base raw unit, from
/// `compute_spot_from_prices`) to the market's tick grid, rounded to
/// nearest. `None` when the price collapses to zero ticks or overflows.
pub fn mid_ticks(spot_raw: f64, market: &Market) -> Option<u64> {
    if !spot_raw.is_finite() || spot_raw <= 0.0 {
        return None;
    }
    let ticks = spot_raw * market.lot_size as f64 / market.tick_size as f64;
    if !ticks.is_finite() || ticks < 0.5 || ticks > u64::MAX as f64 / 2.0 {
        return None;
    }
    Some(ticks.round() as u64)
}

/// Build the two-sided ladder around `mid`. Levels whose bid would reach
/// zero ticks are skipped; the ask side widens by at least one tick per
/// level so bid < mid < ask always holds.
pub fn build_ladder(mid: u64, levels: &[LevelSpec]) -> Vec<LadderOrder> {
    let mut out = Vec::with_capacity(levels.len() * 2);
    for l in levels {
        if l.lots == 0 {
            continue;
        }
        let delta = ((mid as u128 * l.bps as u128) / 10_000).max(1) as u64;
        if mid > delta {
            out.push(LadderOrder { side: Side::Bid, price_ticks: mid - delta, lots: l.lots });
        }
        if let Some(ask) = mid.checked_add(delta) {
            out.push(LadderOrder { side: Side::Ask, price_ticks: ask, lots: l.lots });
        }
    }
    out
}

/// Exact `(maker_amount, taker_amount)` for a level, or `None` on overflow /
/// below the market minimum. Amounts stay under `i64::MAX` (intake's
/// AMOUNT_RANGE bound).
pub fn amounts(level: &LadderOrder, market: &Market) -> Option<(u64, u64)> {
    let base = (level.lots as u128).checked_mul(market.lot_size as u128)?;
    let quote = (level.lots as u128)
        .checked_mul(level.price_ticks as u128)?
        .checked_mul(market.tick_size as u128)?;
    let cap = i64::MAX as u128;
    if base == 0 || quote == 0 || base > cap || quote > cap {
        return None;
    }
    if (base as u64) < market.min_size {
        return None;
    }
    let (base, quote) = (base as u64, quote as u64);
    Some(match level.side {
        Side::Ask => (base, quote),
        Side::Bid => (quote, base),
    })
}

/// Assemble the wire order for a level. Returns `None` when the amounts
/// don't fit the grid bounds.
#[allow(clippy::too_many_arguments)]
pub fn make_order(
    level: &LadderOrder,
    market: &Market,
    maker: SuiAddress,
    manager: SuiAddress,
    max_fee_bps: u64,
    expiry_ms: u64,
    salt: u64,
) -> Option<Order> {
    let (maker_amount, taker_amount) = amounts(level, market)?;
    let (maker_token, taker_token) = match level.side {
        Side::Ask => (market.base.clone(), market.quote.clone()),
        Side::Bid => (market.quote.clone(), market.base.clone()),
    };
    Some(Order {
        maker_token,
        taker_token,
        maker_amount,
        taker_amount,
        max_fee_bps,
        maker,
        maker_manager_id: manager,
        taker: SuiAddress::ZERO,
        sender: SuiAddress::ZERO,
        expiry_ms,
        salt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn market() -> Market {
        Market {
            symbol: "TBTC/TUSDC".into(),
            registry_id: SuiAddress::parse("0x5c").unwrap(),
            base: "0x00000000000000000000000000000000000000000000000000000000000000f8::tbtc::TBTC"
                .into(),
            quote:
                "0x00000000000000000000000000000000000000000000000000000000000000f8::tusdc::TUSDC"
                    .into(),
            // deployment-manager defaults: tick 0.001 TUSDC, lot = 1 whole
            // base token, min = lot/1000.
            tick_size: 1_000,
            min_size: 100_000,
            lot_size: 100_000_000,
            current_fee_bps: 10,
        }
    }

    #[test]
    fn mid_ticks_from_spot() {
        let m = market();
        // TBTC $60k, TUSDC $1: spot raw = 60_000 * 10^(6-8) = 600 quote-raw
        // per base-raw → ticks = 600 * 1e8 / 1e3 = 6e7.
        let spot = 600.0;
        assert_eq!(mid_ticks(spot, &m), Some(60_000_000));
        assert_eq!(mid_ticks(0.0, &m), None);
        assert_eq!(mid_ticks(f64::NAN, &m), None);
        // A price that would collapse below half a tick is refused.
        let dust = Market { tick_size: u64::MAX, ..m };
        assert_eq!(mid_ticks(1e-9, &dust), None);
    }

    #[test]
    fn ladder_shape_and_min_delta() {
        let levels =
            [LevelSpec { bps: 10, lots: 1 }, LevelSpec { bps: 25, lots: 2 }];
        let mid = 60_000_000;
        let l = build_ladder(mid, &levels);
        assert_eq!(l.len(), 4);
        assert_eq!(
            l[0],
            LadderOrder { side: Side::Bid, price_ticks: 59_940_000, lots: 1 }
        );
        assert_eq!(
            l[1],
            LadderOrder { side: Side::Ask, price_ticks: 60_060_000, lots: 1 }
        );
        assert_eq!(l[2].price_ticks, 59_850_000);
        assert_eq!(l[3].price_ticks, 60_150_000);
        // Tiny mid: delta clamps to >= 1 tick, bid at 0 ticks is skipped.
        let l = build_ladder(1, &[LevelSpec { bps: 1, lots: 1 }]);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].side, Side::Ask);
        assert_eq!(l[0].price_ticks, 2);
        // Zero-lot levels are dropped entirely.
        assert!(build_ladder(100, &[LevelSpec { bps: 10, lots: 0 }]).is_empty());
    }

    #[test]
    fn amounts_are_exact_on_the_grid() {
        let m = market();
        for level in build_ladder(60_000_000, &[LevelSpec { bps: 10, lots: 3 }]) {
            let order = make_order(
                &level,
                &m,
                SuiAddress::parse("0x9f").unwrap(),
                SuiAddress::parse("0x71").unwrap(),
                50,
                1_754_330_000_000,
                1,
            )
            .unwrap();
            // The book's own divisibility check must accept every ladder
            // order verbatim — this is the OFF_TICK guarantee.
            let (side, ticks, base) = exchange_book::price_and_size(&m, &order).unwrap();
            assert_eq!(side, level.side);
            assert_eq!(ticks, level.price_ticks);
            assert_eq!(base, level.lots * m.lot_size);
        }
    }

    #[test]
    fn amounts_respect_min_size_and_overflow() {
        let m = market();
        // 0 lots never reaches amounts() via build_ladder, but guard anyway.
        let dust = LadderOrder { side: Side::Ask, price_ticks: 100, lots: 0 };
        assert_eq!(amounts(&dust, &m), None);
        // Overflow: lots * ticks * tick_size past i64::MAX is refused.
        let huge =
            LadderOrder { side: Side::Ask, price_ticks: u64::MAX / 2, lots: u64::MAX / 2 };
        assert_eq!(amounts(&huge, &m), None);
    }
}
