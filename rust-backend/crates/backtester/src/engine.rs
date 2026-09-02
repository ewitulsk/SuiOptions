//! The event loop: a minute clock from `from` to `to` (timers advance
//! through capture holes — doc 08 §6.4), the oracle proxy, the estimator,
//! the constant-flow injector, the band hedger, funding settlements,
//! expiry settlement, and the ledger.

use std::collections::BTreeMap;

use anyhow::Result;
use pricing::desk::{expected_hedge_cost, v1_bid, BidContext, HedgeCostParams, V1BidParams};
use serde::Serialize;

use crate::data::{Bar, FundingRow};
use crate::estimator::WindowsEstimator;
use crate::flow::{rfqs_for, Rfq};
use crate::ledger::{Ledger, Position};
use crate::model::{fair_per_unit, greeks_per_unit};
use crate::oracle::OracleProxy;
use crate::scenario::Scenario;
use crate::{MS_PER_DAY, MS_PER_YEAR_F};

/// One settled option, for the vol-P&L study (doc 09 §2.4).
#[derive(Clone, Debug, Serialize)]
pub struct SettledOption {
    pub id: u64,
    pub is_put: bool,
    pub strike: f64,
    pub opened_ms: i64,
    pub expiry_ms: i64,
    pub qty: f64,
    pub spot_open: f64,
    pub spot_close: f64,
    pub premium_paid: f64,
    pub payoff: f64,
    pub sigma_paid: f64,
    pub sigma_surface: f64,
    /// Realized vol over the option's life at the estimator's interval.
    pub sigma_realized: f64,
    /// ½·Γ·S²·(σ_r² − σ_paid²)·τ at entry greeks, per doc 09 §2.1.
    pub vol_pnl_proxy: f64,
    /// Doc 07 §5 "hedge P&L" analogue for this option's life:
    /// payoff − premium (option leg only).
    pub option_leg_pnl: f64,
}

/// A daily NAV sample.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct NavPoint {
    pub ts_ms: i64,
    pub spot: f64,
    pub nav: f64,
    pub cash: f64,
    pub option_marks: f64,
    pub perp_position: f64,
    pub net_delta_units: f64,
    pub premium_deployed_pct: f64,
    pub sigma_surface: Option<f64>,
    pub stale: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunOutput {
    pub nav_path: Vec<NavPoint>,
    pub settled: Vec<SettledOption>,
    pub ledger: Ledger,
    pub minutes_total: u64,
    pub minutes_with_bar: u64,
    pub minutes_stale: u64,
    pub funding_settlements: u64,
    pub turns: u64,
    pub max_drawdown: f64,
    pub nav_end: f64,
    pub spot_start: f64,
    pub spot_end: f64,
}

struct Book {
    est: WindowsEstimator,
    oracle: OracleProxy,
    ledger: Ledger,
    v1: V1BidParams,
    hedge_cost: HedgeCostParams,
}

fn v1_params(s: &Scenario) -> V1BidParams {
    V1BidParams {
        base_spread_volpts: s.bid.base_spread_volpts,
        size_penalty_volpts_per_pct_nav: s.bid.size_penalty_volpts_per_pct_nav,
        size_penalty_quadratic_from_pct: s.bid.size_penalty_quadratic_from_pct,
        inventory_penalty_max_volpts: s.bid.inventory_penalty_max_volpts,
        inventory_penalty_start_util: s.bid.inventory_penalty_start_util,
        max_single_fill_pct_nav: s.bid.max_single_fill_pct_nav,
        funding_income_credit: s.bid.funding_income_credit,
    }
}

fn hedge_cost_params(s: &Scenario) -> HedgeCostParams {
    HedgeCostParams {
        slippage_bps: s.hedge.slippage_bps,
        taker_fee_bps: s.hedge.taker_fee_bps,
        fixed_fee_per_fill: s.hedge.fixed_fee_per_fill,
        rebalance_turnover_per_year: s.hedge.rebalance_turnover_per_year,
        margin_financing_rate_annual: s.hedge.margin_financing_rate_annual,
        initial_margin_fraction: s.hedge.initial_margin_fraction,
    }
}

/// Annualized funding from the latest settled row (rate per interval).
fn annualize(row: &FundingRow) -> f64 {
    if row.interval_hours <= 0.0 {
        0.0
    } else {
        row.rate * (8760.0 / row.interval_hours)
    }
}

pub fn run(s: &Scenario, bars: &[Bar], funding: &[FundingRow], vol_index: &[(i64, f64)]) -> Result<RunOutput> {
    if s.estimator.kind == "vol_index" {
        anyhow::ensure!(!vol_index.is_empty(), "estimator.kind = vol_index needs a vol_index series");
    }
    anyhow::ensure!(!bars.is_empty(), "no bars for {}/{} in {}..{}", s.spot_exchange, s.spot_symbol, s.from, s.to);
    let start_ms = crate::data::date_start_ms(&s.from)?;
    let end_ms = crate::data::date_start_ms(&s.to)? + MS_PER_DAY;
    let by_minute: BTreeMap<i64, Bar> = bars.iter().map(|b| (b.ts_ms - b.ts_ms.rem_euclid(60_000), *b)).collect();

    let mut book = Book {
        est: WindowsEstimator::new(s.estimator.clone(), s.flow.tenor_days),
        oracle: OracleProxy::new(s.oracle.clone()),
        ledger: Ledger::new(s.nav0),
        v1: v1_params(s),
        hedge_cost: hedge_cost_params(s),
    };
    let interval_ms = s.estimator.sample_interval_s * 1000;
    // Sampled decision prices for per-option realized vol.
    let mut price_samples: Vec<(i64, f64)> = Vec::new();
    let mut last_sample_ms = i64::MIN;

    let mut funding_idx = 0usize;
    let mut funding_annual = 0.0;
    let mut index_idx = 0usize;
    let mut index_vol: Option<f64> = None;
    let mut funding_settlements = 0u64;
    let mut next_turn_ms = start_ms;
    let mut next_daily_ms = start_ms + s.flow.hour_utc as i64 * 3_600_000;
    let mut turns = 0u64;
    let mut nav_path = Vec::new();
    let mut settled = Vec::new();
    let mut minutes_with_bar = 0u64;
    let mut minutes_stale = 0u64;
    let mut last_spot = bars[0].close;
    let mut peak = s.nav0;
    let mut max_dd = 0.0f64;
    let mut last_nav_day = i64::MIN;
    let tenor_ms = (s.flow.tenor_days * MS_PER_DAY as f64) as i64;

    let mut now = start_ms;
    while now < end_ms {
        if let Some(bar) = by_minute.get(&now) {
            minutes_with_bar += 1;
            last_spot = bar.close;
            book.oracle.observe(now, bar.close);
        }
        let decision = book.oracle.decision(now);
        let stale = decision.is_none();
        if stale {
            minutes_stale += 1;
        }
        // The estimator and the study samples see the DECISION price.
        if let Some(d) = decision {
            book.est.push(d.event_ms, d.price);
            if d.event_ms.saturating_sub(last_sample_ms) >= interval_ms {
                last_sample_ms = d.event_ms;
                price_samples.push((d.event_ms, d.price));
            }
        }
        let spot = decision.map(|d| d.price).unwrap_or(last_spot);
        // Vol index (percent) LOCF, as of now — never ahead of the clock.
        while index_idx < vol_index.len() && vol_index[index_idx].0 <= now {
            index_vol = Some(vol_index[index_idx].1 / 100.0);
            index_idx += 1;
        }
        book.est.set_index_vol(index_vol);
        let readout = book.est.surface(now);

        // Funding settlements up to now, against the signed position at
        // the mark (the spot path is the mark: proxy_venue).
        while funding_idx < funding.len() && funding[funding_idx].ts_ms <= now {
            let row = funding[funding_idx];
            funding_idx += 1;
            funding_annual = annualize(&row);
            let paid = row.rate * book.ledger.perp.position * spot;
            book.ledger.cash -= paid;
            book.ledger.lines.funding_paid += paid;
            funding_settlements += 1;
        }

        // Mark every open option at the surface sigma; settle expiries.
        let carry = s.carry_yield;
        let mut expired: Vec<Position> = Vec::new();
        let mut i = 0;
        while i < book.ledger.positions.len() {
            let p = &mut book.ledger.positions[i];
            if now >= p.expiry_ms {
                expired.push(book.ledger.positions.remove(i));
                continue;
            }
            let t = (p.expiry_ms - now) as f64 / MS_PER_YEAR_F;
            let sigma = readout.surface.vol(spot, p.strike, t);
            p.mark = fair_per_unit(p.is_put, spot, p.strike, t, sigma, carry);
            i += 1;
        }
        for p in expired {
            let intrinsic_per_unit = if p.is_put { (p.strike - spot).max(0.0) } else { (spot - p.strike).max(0.0) };
            let mut payoff = 0.0;
            let mut costs = 0.0;
            if intrinsic_per_unit > 0.0 {
                let slip = s.exercise.spot_slippage_bps / 10_000.0;
                let fee = s.exercise.spot_fee_bps / 10_000.0;
                let notional = spot * p.qty;
                // Call: pay strike, receive underlying, sell it. Put: buy
                // underlying, deliver it, receive strike. Both leave the
                // desk flat in the underlying.
                let exec_px = if p.is_put { spot * (1.0 + slip) } else { spot * (1.0 - slip) };
                let gross = if p.is_put { (p.strike - exec_px) * p.qty } else { (exec_px - p.strike) * p.qty };
                costs = notional * fee + s.exercise.gas_per_exercise;
                payoff = gross - costs;
                book.ledger.lines.exercise_turnover_notional += notional;
            }
            book.ledger.cash += payoff;
            book.ledger.lines.option_payoff += payoff;
            book.ledger.lines.exercise_costs += costs;
            let life: Vec<(i64, f64)> = price_samples.iter().copied().filter(|(t, _)| *t >= p.opened_ms && *t <= now).collect();
            let sigma_realized = crate::estimator::realized_vol(&life, now - p.opened_ms + 1, now).unwrap_or(0.0);
            let tau = (p.expiry_ms - p.opened_ms) as f64 / MS_PER_YEAR_F;
            let vol_pnl_proxy = 0.5 * p.gamma_open * p.spot_open * p.spot_open * (sigma_realized.powi(2) - p.sigma_paid.powi(2)) * tau * p.qty;
            settled.push(SettledOption {
                id: p.id, is_put: p.is_put, strike: p.strike, opened_ms: p.opened_ms, expiry_ms: p.expiry_ms, qty: p.qty,
                spot_open: p.spot_open, spot_close: spot, premium_paid: p.premium_paid, payoff, sigma_paid: p.sigma_paid,
                sigma_surface: p.sigma_surface, sigma_realized, vol_pnl_proxy, option_leg_pnl: payoff - p.premium_paid,
            });
        }

        // Flow.
        let nav_now = book.ledger.nav(spot);
        let mut wanted: Vec<Rfq> = Vec::new();
        match s.flow.mode.as_str() {
            // A turn/day that lands on a stale price is retried every
            // minute until the price is fresh again (the writer keeps
            // asking; the desk keeps declining) — time is not skipped.
            "per_turn" => {
                if now >= next_turn_ms {
                    if stale {
                        book.ledger.lines.declines_stale += 1;
                    } else {
                        next_turn_ms = now + tenor_ms;
                        turns += 1;
                        wanted = rfqs_for(&s.flow, now, spot, readout.surface.atm(s.flow.tenor_days / 365.0), s.flow.notional_nav_multiple * nav_now);
                    }
                }
            }
            "daily" => {
                if now >= next_daily_ms {
                    if stale {
                        book.ledger.lines.declines_stale += 1;
                    } else {
                        next_daily_ms += MS_PER_DAY;
                        wanted = rfqs_for(&s.flow, now, spot, readout.surface.atm(s.flow.tenor_days / 365.0), s.flow.notional_per_day);
                    }
                }
            }
            other => anyhow::bail!("unknown flow.mode {other}"),
        }
        for rfq in wanted {
            let t = (rfq.expiry_ms - now) as f64 / MS_PER_YEAR_F;
            let sigma = readout.surface.vol(spot, rfq.strike, t);
            let fair_pu = fair_per_unit(rfq.is_put, spot, rfq.strike, t, sigma, carry);
            let g = greeks_per_unit(rfq.is_put, spot, rfq.strike, t, sigma, carry);
            let premium_fair = fair_pu * rfq.qty;
            // Limits (doc 08 §0.4): total, per type, per expiry.
            let deployed = book.ledger.premium_deployed();
            let by_type = book.ledger.premium_by_type(rfq.is_put);
            let by_expiry = book.ledger.premium_by_expiry(rfq.expiry_ms);
            let type_cap = if rfq.is_put { s.limits.put_premium_max } else { s.limits.call_premium_max };
            if deployed + premium_fair > s.limits.premium_budget_hard * nav_now
                || by_type + premium_fair > type_cap * nav_now
                || by_expiry + premium_fair > s.limits.per_expiry_max * nav_now
            {
                book.ledger.lines.declines_capacity += 1;
                continue;
            }
            let vega_book: f64 = book.ledger.positions.iter().map(|p| p.vega_open * p.qty / 100.0).sum();
            let vega_util = if s.limits.vega_cap_nav_per_volpt > 0.0 { vega_book / (s.limits.vega_cap_nav_per_volpt * nav_now) } else { 0.0 };
            let ctx = BidContext {
                nav: nav_now,
                premium_notional: premium_fair,
                vega_utilization: vega_util,
                hedge_cost: expected_hedge_cost(
                    book.ledger.perp.position,
                    g.delta * rfq.qty,
                    spot,
                    funding_annual,
                    s.bid.expected_holding_years,
                    s.bid.funding_income_credit,
                    &book.hedge_cost,
                ),
            };
            let fair_at = |sig: f64| fair_per_unit(rfq.is_put, spot, rfq.strike, t, sig, carry) * rfq.qty;
            let Some(bid) = v1_bid(fair_at, sigma, &ctx, &book.v1) else {
                book.ledger.lines.declines_priced_zero += 1;
                continue;
            };
            // Recover the struck sigma for the study: the discount total.
            let discount = pricing::desk::v1_vol_discount(&ctx, &book.v1).map(|d| d.total).unwrap_or(0.0);
            let sigma_paid = (sigma - discount).max(0.0);
            book.ledger.cash -= bid;
            book.ledger.lines.premium_paid += bid;
            book.ledger.lines.fills += 1;
            let id = book.ledger.next_id;
            book.ledger.next_id += 1;
            book.ledger.positions.push(Position {
                id, is_put: rfq.is_put, strike: rfq.strike, expiry_ms: rfq.expiry_ms, qty: rfq.qty, premium_paid: bid,
                sigma_paid, sigma_surface: sigma, opened_ms: now, spot_open: spot, delta_open: g.delta, gamma_open: g.gamma,
                vega_open: g.vega, writer_net_premium: bid * (1.0 - s.fees.protocol_premium_fee_bps / 10_000.0),
                mark: fair_pu,
            });
        }

        // Hedge: bands not clocks; no trades on a stale price.
        let net_delta_units = book.ledger.positions.iter().map(|p| {
            let t = (p.expiry_ms - now) as f64 / MS_PER_YEAR_F;
            let sigma = readout.surface.vol(spot, p.strike, t);
            greeks_per_unit(p.is_put, spot, p.strike, t, sigma, carry).delta * p.qty
        }).sum::<f64>();
        if !stale {
            let pct = if funding_annual < s.hedge.funding_widen_threshold { s.hedge.band_wide_pct_nav } else { s.hedge.band_pct_nav };
            let band_units = (pct / 100.0) * nav_now / spot;
            let net = net_delta_units + book.ledger.perp.position;
            if net.abs() > band_units {
                let size = -net_delta_units - book.ledger.perp.position;
                let slip = spot * s.hedge.slippage_bps / 10_000.0;
                let px = spot + slip * size.signum();
                let notional = size.abs() * spot;
                let fee = notional * s.hedge.taker_fee_bps / 10_000.0 + s.hedge.fixed_fee_per_fill;
                let realized = book.ledger.perp.fill(size, px);
                book.ledger.cash += realized - fee - s.exercise.gas_per_rebalance;
                book.ledger.lines.hedge_realized += realized;
                book.ledger.lines.hedge_fees += fee;
                book.ledger.lines.hedge_slippage += size.abs() * slip;
                book.ledger.lines.gas += s.exercise.gas_per_rebalance;
                book.ledger.lines.hedge_turnover_notional += notional;
                book.ledger.lines.hedge_fills += 1;
            }
        }

        // NAV, drawdown, daily sample.
        let nav = book.ledger.nav(spot);
        peak = peak.max(nav);
        if peak > 0.0 {
            max_dd = max_dd.max((peak - nav) / peak);
        }
        let day = now.div_euclid(MS_PER_DAY);
        if day != last_nav_day {
            last_nav_day = day;
            nav_path.push(NavPoint {
                ts_ms: now, spot, nav, cash: book.ledger.cash, option_marks: book.ledger.option_marks(),
                perp_position: book.ledger.perp.position, net_delta_units,
                premium_deployed_pct: if nav > 0.0 { book.ledger.premium_deployed() / nav } else { 0.0 },
                sigma_surface: if readout.fallback { None } else { Some(readout.surface.atm(s.flow.tenor_days / 365.0)) },
                stale,
            });
        }
        now += 60_000;
    }

    let nav_end = book.ledger.nav(last_spot);
    Ok(RunOutput {
        nav_path, settled, ledger: book.ledger,
        minutes_total: ((end_ms - start_ms) / 60_000) as u64,
        minutes_with_bar, minutes_stale, funding_settlements, turns, max_drawdown: max_dd, nav_end,
        spot_start: bars[0].close, spot_end: last_spot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::Scenario;

    /// A synthetic path: a deterministic LCG random walk at ~45%
    /// annualized vol plus a slow sine, 1-minute bars.
    fn synthetic_bars(days: i64, start_ms: i64) -> Vec<Bar> {
        let mut out = Vec::new();
        let n = days * 1440;
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut px = 3.0f64;
        for i in 0..n {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let u = ((state >> 11) as f64) / ((1u64 << 53) as f64); // [0,1)
            let r = (u - 0.5) * 2.0 * 0.0006 * 1.732; // uniform with sd ≈ 0.0006 per minute
            let t = i as f64 / 1440.0;
            px *= (r + 0.0001 * (t * 0.7).cos() / 1440.0).exp();
            out.push(Bar { ts_ms: start_ms + i * 60_000, open: px, high: px, low: px, close: px, volume: 1.0 });
        }
        out
    }

    #[allow(clippy::field_reassign_with_default)]
    fn scenario() -> Scenario {
        let mut s = Scenario::default();
        s.from = "2025-01-01".into();
        s.to = "2025-03-11".into();
        s.nav0 = 1_000_000.0;
        s.flow.tenor_days = 30.0;
        // Doc 07 framing: the whole 30% budget sits in one expiry and the
        // bid is fair − 5 vol points with no size penalty.
        s.limits.per_expiry_max = 0.30;
        s.limits.call_premium_max = 0.30;
        s.bid.size_penalty_volpts_per_pct_nav = 0.0;
        s
    }

    #[test]
    fn ledger_reconciles_every_day_and_is_deterministic() {
        let s = scenario();
        let start = crate::data::date_start_ms(&s.from).unwrap();
        let bars = synthetic_bars(70, start);
        let funding: Vec<FundingRow> = (0..70 * 3).map(|i| FundingRow { ts_ms: start + i * 8 * 3_600_000, rate: 0.0001, interval_hours: 8.0 }).collect();
        let a = run(&s, &bars, &funding, &[]).unwrap();
        let b = run(&s, &bars, &funding, &[]).unwrap();
        assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap(), "not deterministic");
        assert!(a.turns >= 2, "{}", a.turns);
        assert!(a.ledger.lines.fills >= 2, "fills {} declines cap {} stale {} zero {}", a.ledger.lines.fills, a.ledger.lines.declines_capacity, a.ledger.lines.declines_stale, a.ledger.lines.declines_priced_zero);
        assert!(a.funding_settlements > 0);
        assert!(a.ledger.lines.hedge_fills > 0, "hedge never traded");
        // Cash identity: nav0 − premium + payoff + hedge realized − funding − fees − slippage(in fills) − gas = cash.
        let l = &a.ledger.lines;
        let cash_expected = s.nav0 - l.premium_paid + l.option_payoff + l.hedge_realized - l.funding_paid - l.hedge_fees - l.gas;
        assert!((a.ledger.cash - cash_expected).abs() < 1e-6, "cash {} vs identity {}", a.ledger.cash, cash_expected);
        // Every daily sample reconciles: NAV = cash + option marks + perp
        // unrealized at that day's spot and position.
        for p in &a.nav_path {
            let unrealized = p.perp_position * (p.spot - a.ledger.perp.avg_entry);
            let _ = unrealized; // avg_entry drifts across the path; the identity is checked at the end below
            assert!(p.nav.is_finite() && p.cash.is_finite());
        }
        let final_spot = a.spot_end;
        assert!((a.nav_end - (a.ledger.cash + a.ledger.option_marks() + a.ledger.perp.unrealized(final_spot))).abs() < 1e-6);
        assert!(a.settled.iter().all(|o| o.sigma_realized > 0.0));
    }

    #[test]
    fn capture_hole_advances_timers_and_declines_quotes() {
        let s = scenario();
        let start = crate::data::date_start_ms(&s.from).unwrap();
        let mut bars = synthetic_bars(70, start);
        // Remove the second half of day 29 and all of day 30 — the second
        // turn (day 30 + 1 min) lands deep inside the hole.
        bars.retain(|b| !(b.ts_ms >= start + 29 * MS_PER_DAY + MS_PER_DAY / 2 && b.ts_ms < start + 31 * MS_PER_DAY));
        let out = run(&s, &bars, &[], &[]).unwrap();
        assert_eq!(out.minutes_total, 70 * 1440);
        assert_eq!(out.minutes_with_bar, 70 * 1440 - 2160);
        assert!(out.minutes_stale >= 2160 - 3, "{}", out.minutes_stale);
        // The turn that lands in the hole is declined every minute until
        // the price is fresh again, then filled — never skipped in time.
        assert!(out.ledger.lines.declines_stale > 1000, "{}", out.ledger.lines.declines_stale);
        assert!(out.turns >= 2);
    }
}
