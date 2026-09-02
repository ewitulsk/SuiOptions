//! The mark pass (the book refresher's per-tick core, pure): every
//! held / written line priced at the current surface and spot into
//! per-unit marks + greeks, and the [`BookExposure`] surfaces the limits
//! engine reads — net vega/theta, premium by expiry and strike bucket,
//! the composition surfaces of doc 08 §4.5 (SO-445) and the capital
//! demands of §4.6 (SO-444).
//!
//! The caller (the live refresher on its `MarkUpdate` event, the
//! backtester on its clock) supplies the chain-read inputs — NAV,
//! reservations, free balances — and finishes the exposure with the
//! capital snapshot ([`crate::limits::build_capital_snapshot`]).

use std::collections::HashMap;

use protocol_types::ids::ObjectId;

use crate::book::{Holding, Written};
use crate::limits::{self, BookExposure};
use crate::model::{Greeks, MarketModel};

/// Per-bucket mark snapshot written by the mark pass: model fair, the
/// sigma/spot it was computed at, and per-unit greeks. `/desk/state`
/// serves it so a snapshot never re-prices.
#[derive(Clone, Copy, Debug)]
pub struct MarkSnapshot {
    pub mark_per_unit: f64,
    pub sigma: f64,
    pub spot: f64,
    pub greeks: Greeks,
    pub at_ms: u64,
}

/// Per-symbol spot written each mark pass.
#[derive(Clone, Copy, Debug)]
pub struct SpotSnapshot {
    pub spot: f64,
    pub at_ms: u64,
}

/// What one mark pass needs.
pub struct MarkInputs<'a> {
    pub models: &'a [MarketModel],
    pub holdings: &'a [Holding],
    pub written: &'a [Written],
    /// Fresh spot per model this tick (`None` = stale / unavailable:
    /// that market's lines are skipped, exactly as before).
    pub spot_by_model: &'a [Option<f64>],
    pub now_ms: u64,
    /// The monitors' stress gaps as positive fractions (`|gap|`).
    pub stress_gap_down: f64,
    pub stress_gap_up: f64,
    /// Configured flash capacities (`[desk.capital]`).
    pub quote_flash_capacity: f64,
    pub base_flash_capacity: f64,
}

/// The mark pass output: the exposure with every mark-derived surface
/// filled (NAV, reservations, capital snapshot and the kill switch are
/// the caller's), the marks, and the capital-snapshot demands of the
/// marked book.
#[derive(Clone, Debug, Default)]
pub struct MarkPass {
    pub exposure: BookExposure,
    pub marks: HashMap<ObjectId, MarkSnapshot>,
    /// Net book delta per underlying coin type, underlying raw units.
    pub delta_by_coin: HashMap<String, f64>,
    /// Mark-to-model premium in held options.
    pub deployed: f64,
    /// Strike cash the held calls need at exercise / underlying value
    /// the held puts deliver.
    pub call_strike_cash: f64,
    pub put_underlying_value: f64,
    pub exercise_demand_by_expiry: HashMap<u64, f64>,
    /// |delta|·spot of the held lines, total and per expiry.
    pub hedge_notional: f64,
    pub hedge_notional_by_expiry: HashMap<u64, f64>,
}

/// Price every line at the surface and aggregate the exposure surfaces.
/// Written lines subtract their full greeks so quoting sees TRUE nets
/// (net vega = held − written, same for delta / gamma / theta).
pub fn mark_book(i: MarkInputs<'_>) -> MarkPass {
    let now = i.now_ms;
    let mut out = MarkPass::default();
    let exposure = &mut out.exposure;
    for h in i.holdings {
        let Some(mi) = i.models.iter().position(|m| m.coin_type == h.asset_coin_type) else {
            continue;
        };
        let Some(spot) = i.spot_by_model[mi] else {
            continue;
        };
        let t = h.expiry_ms.saturating_sub(now) as f64 / 1000.0 / 86_400.0 / 365.0;
        let k = h.strike_scaled();
        let (sigma, _) = i.models[mi].sigma(spot, k, t);
        let mark = i.models[mi].fair_per_unit(h.is_put, spot, k, t, sigma);
        let g = i.models[mi].greeks_per_unit(h.is_put, spot, k, t, sigma);
        out.marks.insert(
            h.bucket_id,
            MarkSnapshot { mark_per_unit: mark, sigma, spot, greeks: g, at_ms: now },
        );
        let amt = h.amount() as f64;
        out.deployed += mark * amt;
        exposure.net_vega_per_volpt += g.vega * amt / 100.0;
        exposure.theta_cost_per_day += (-g.theta * amt).max(0.0);
        *exposure.premium_by_expiry.entry(h.expiry_ms).or_default() += mark * amt;
        exposure.premium_by_strike_bucket[limits::strike_bucket(k, spot)] += mark * amt;
        // Composition surfaces (doc 08 §4.5, SO-431).
        if h.is_put {
            exposure.put_premium += mark * amt;
            exposure.gamma_units_puts += g.gamma * amt;
        } else {
            exposure.call_premium += mark * amt;
            exposure.gamma_units_calls += g.gamma * amt;
        }
        let line_delta = g.delta * amt;
        if line_delta >= 0.0 {
            exposure.delta_units_positive += line_delta;
        } else {
            exposure.delta_units_negative += line_delta;
        }
        *out.delta_by_coin.entry(h.asset_coin_type.clone()).or_default() += g.delta * amt;
        let demand = if h.is_put { spot } else { k } * amt;
        let line_hedge = line_delta.abs() * spot;
        let gamma_by_type = exposure.gamma_by_expiry.entry(h.expiry_ms).or_default();
        let gamma_notional_per_pct = g.gamma * amt * 0.01 * spot * spot;
        // Composition surfaces per side (doc 08 §4.5, SO-445): puts need
        // underlying and their LONG hedge loses in a crash; calls need
        // strike cash and their SHORT hedge loses in a rally.
        if h.is_put {
            out.put_underlying_value += demand;
            *exposure.put_underlying_value_by_expiry.entry(h.expiry_ms).or_default() += demand;
            gamma_by_type.puts_units += g.gamma * amt;
            gamma_by_type.puts_notional_per_pct += gamma_notional_per_pct;
            exposure.crash_loss_put_hedges += line_hedge * i.stress_gap_down;
        } else {
            out.call_strike_cash += demand;
            *exposure.call_settlement_cash_by_expiry.entry(h.expiry_ms).or_default() += demand;
            gamma_by_type.calls_units += g.gamma * amt;
            gamma_by_type.calls_notional_per_pct += gamma_notional_per_pct;
            exposure.rally_loss_call_hedges += line_hedge * i.stress_gap_up;
        }
        *out.exercise_demand_by_expiry.entry(h.expiry_ms).or_default() += demand;
        out.hedge_notional += line_hedge;
        *out.hedge_notional_by_expiry.entry(h.expiry_ms).or_default() += line_hedge;
    }
    exposure.stress_gap_down = i.stress_gap_down;
    exposure.stress_gap_up = i.stress_gap_up;
    exposure.concurrent_demand = limits::concurrent_demand(
        &exposure.call_settlement_cash_by_expiry,
        &exposure.put_underlying_value_by_expiry,
        exposure.crash_loss_put_hedges,
        exposure.rally_loss_call_hedges,
    );
    for (e, cash) in &exposure.call_settlement_cash_by_expiry {
        exposure
            .quote_flash_util_by_expiry
            .insert(*e, limits::flash_utilization(*cash, i.quote_flash_capacity));
    }
    for (e, value) in &exposure.put_underlying_value_by_expiry {
        exposure
            .base_flash_util_by_expiry
            .insert(*e, limits::flash_utilization(*value, i.base_flash_capacity));
    }
    // A written bucket with no held coin still needs per-unit marks
    // computed here.
    for w in i.written {
        let Some(mi) = i.models.iter().position(|m| m.coin_type == w.asset_coin_type) else {
            continue;
        };
        let g = match out.marks.get(&w.bucket_id) {
            Some(m) => m.greeks,
            None => {
                let Some(spot) = i.spot_by_model[mi] else {
                    continue;
                };
                let t = w.expiry_ms.saturating_sub(now) as f64 / 1000.0 / 86_400.0 / 365.0;
                let k = w.strike_scaled();
                let (sigma, _) = i.models[mi].sigma(spot, k, t);
                let mark = i.models[mi].fair_per_unit(w.is_put, spot, k, t, sigma);
                let g = i.models[mi].greeks_per_unit(w.is_put, spot, k, t, sigma);
                out.marks.insert(
                    w.bucket_id,
                    MarkSnapshot { mark_per_unit: mark, sigma, spot, greeks: g, at_ms: now },
                );
                g
            }
        };
        let amt = w.amount as f64;
        exposure.net_vega_per_volpt -= g.vega * amt / 100.0;
        exposure.theta_cost_per_day -= (-g.theta * amt).max(0.0);
        *out.delta_by_coin.entry(w.asset_coin_type.clone()).or_default() -= g.delta * amt;
    }
    out
}
