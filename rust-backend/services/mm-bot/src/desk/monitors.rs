//! Desk monitors + nightly stress suite (00-plan Phase 4).
//!
//! One task emits gauges each tick and fires `error!(alert_id = …)` on
//! breach:
//!   - `mm-desk-delta-band`      net-of-hedge delta outside the band
//!   - `mm-desk-vega-cap`        |net vega| over the cap
//!   - `mm-desk-theta-governor`  theta bleed over the hard cap
//!   - `mm-desk-bleed`           rolling (scalp+spread) < (theta+funding)
//!   - `mm-desk-reserves`        reservations + deployed over NAV
//!   - `mm-desk-margin-headroom` hedge margin headroom under floor
//!   - `mm-desk-kill-switch`     the NAV-drawdown switch latched
//!
//! The nightly stress job revalues the live book via the model at
//! −60% / +80% spot gaps, projects theta over a flat 6 months, and
//! haircuts funding −50%; worst drawdown > 25% NAV sets the V2 gate
//! (`DeskShared::stress_blocked`) that blocks new short risk.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use serde::Deserialize;

use pyth_client::{PriceCache, PriceFeedId};

use crate::pricing::{compute_spot_from_cache, Staleness};

use super::book::Book;
use super::hedge::HedgeVenue;
use super::limits::LimitsConfig;
use super::model::MarketModel;
use super::DeskShared;

/// `[desk.monitors]` knobs.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct MonitorsConfig {
    pub interval_secs: u64,
    /// Bleed alarm window, hours: alert when (scalp+spread) < (theta+funding)
    /// over the window. Counters are since-boot; the window snapshots them.
    pub bleed_window_hours: f64,
    /// Alert when hedge margin headroom drops under this fraction.
    pub margin_headroom_floor: f64,
    /// Stress cadence.
    pub stress_interval_secs: u64,
    /// V2 gate: block new short risk when stressed drawdown exceeds this
    /// fraction of NAV. 00-plan V2 §7: 25%.
    pub stress_max_drawdown: f64,
}

impl Default for MonitorsConfig {
    fn default() -> Self {
        Self {
            interval_secs: 60,
            bleed_window_hours: 24.0,
            margin_headroom_floor: 0.25,
            stress_interval_secs: 86_400,
            stress_max_drawdown: 0.25,
        }
    }
}

pub struct MonitorsParams {
    pub cfg: MonitorsConfig,
    pub limits: LimitsConfig,
    pub shared: Arc<DeskShared>,
    pub book: Arc<RwLock<Book>>,
    pub models: Arc<Vec<MarketModel>>,
    pub market_feeds: Vec<(PriceFeedId, u8)>,
    pub price_cache: PriceCache,
    pub settlement_feed: PriceFeedId,
    pub settlement_decimals: u8,
    pub staleness: Staleness,
    pub hedge: Arc<dyn HedgeVenue>,
    pub hedge_band_pct_nav: f64,
}

pub fn spawn_monitors(p: MonitorsParams) {
    let stress = MonitorsParams { ..clone_params(&p) };
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(p.cfg.interval_secs.max(10)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Rolling bleed window: (timestamp, income, cost) snapshots.
        let mut bleed_samples: Vec<(u64, f64, f64)> = Vec::new();
        loop {
            ticker.tick().await;
            monitor_tick(&p, &mut bleed_samples).await;
        }
    });
    tokio::spawn(async move {
        let mut ticker =
            tokio::time::interval(Duration::from_secs(stress.cfg.stress_interval_secs.max(300)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            stress_tick(&stress).await;
        }
    });
}

// MonitorsParams holds Arcs + copies only; a manual clone keeps the
// struct free of a Clone bound on the trait object.
fn clone_params(p: &MonitorsParams) -> MonitorsParams {
    MonitorsParams {
        cfg: p.cfg,
        limits: p.limits,
        shared: Arc::clone(&p.shared),
        book: Arc::clone(&p.book),
        models: Arc::clone(&p.models),
        market_feeds: p.market_feeds.clone(),
        price_cache: p.price_cache.clone(),
        settlement_feed: p.settlement_feed,
        settlement_decimals: p.settlement_decimals,
        staleness: p.staleness,
        hedge: Arc::clone(&p.hedge),
        hedge_band_pct_nav: p.hedge_band_pct_nav,
    }
}

async fn monitor_tick(p: &MonitorsParams, bleed_samples: &mut Vec<(u64, f64, f64)>) {
    let now = super::auctions::now_ms();
    let exposure = p.shared.exposure.read().clone();
    let nav = exposure.nav.max(0.0);

    // Reserves.
    let (reserved, deployed, pnl) = {
        let book = p.book.read();
        (book.reserved_total() as f64, book.deployed as f64, book.pnl)
    };
    metrics::gauge!("mm_desk_nav").set(nav);
    metrics::gauge!("mm_desk_reserved").set(reserved);
    metrics::gauge!("mm_desk_deployed").set(deployed);
    if reserved + deployed > nav && nav > 0.0 {
        tracing::error!(
            alert_id = "mm-desk-reserves",
            reserved,
            deployed,
            nav,
            "reservations + deployed exceed NAV"
        );
    }

    // Vega cap.
    let vega_cap = p.limits.vega_cap_nav_per_volpt * nav;
    metrics::gauge!("mm_desk_net_vega_per_volpt").set(exposure.net_vega_per_volpt);
    if vega_cap > 0.0 && exposure.net_vega_per_volpt.abs() > vega_cap {
        tracing::error!(
            alert_id = "mm-desk-vega-cap",
            net_vega = exposure.net_vega_per_volpt,
            cap = vega_cap,
            "net vega over the cap"
        );
    }

    // Theta governor.
    let theta_cap = p.limits.theta_hard_nav_per_day * nav;
    metrics::gauge!("mm_desk_theta_cost_per_day").set(exposure.theta_cost_per_day);
    if theta_cap > 0.0 && exposure.theta_cost_per_day > theta_cap {
        tracing::error!(
            alert_id = "mm-desk-theta-governor",
            theta_per_day = exposure.theta_cost_per_day,
            cap = theta_cap,
            "theta bleed over the hard governor"
        );
    }

    // Kill switch.
    if exposure.kill_switch {
        tracing::error!(alert_id = "mm-desk-kill-switch", nav, "kill switch latched: new buys stopped");
    }

    // Delta vs band, per market. The monitor's hedge handle covers the
    // first venue; per-market monitoring shares the aggregate short
    // (single-underlying deployments; refine with multi-venue wiring).
    let funding = p.hedge.funding_rate_annual().await.unwrap_or(0.0);
    metrics::gauge!("mm_desk_funding_rate_annual").set(funding);
    let hedge_short = p.hedge.position_units().await.unwrap_or(0.0);
    metrics::gauge!("mm_desk_hedge_short_units").set(hedge_short);
    let deltas = p.shared.book_delta_units.read().clone();
    for (i, model) in p.models.iter().enumerate() {
        let delta_units = deltas.get(&model.coin_type).copied().unwrap_or(0.0);
        metrics::gauge!("mm_desk_book_delta_units", "symbol" => model.symbol.clone())
            .set(delta_units);
        let (feed, decimals) = p.market_feeds[i];
        let Ok(spot) = compute_spot_from_cache(
            &p.price_cache,
            feed,
            p.settlement_feed,
            decimals,
            p.settlement_decimals,
            p.staleness,
        ) else {
            continue;
        };
        let band = super::hedge::band_units_for(p.hedge_band_pct_nav, nav, spot);
        let net = delta_units - hedge_short;
        metrics::gauge!("mm_desk_delta_net_of_hedge_units", "symbol" => model.symbol.clone())
            .set(net);
        if net.abs() > band && band.is_finite() {
            tracing::error!(
                alert_id = "mm-desk-delta-band",
                symbol = %model.symbol,
                net_delta_units = net,
                band_units = band,
                "net-of-hedge delta outside the band"
            );
        }
    }

    // Margin headroom.
    let headroom = p.hedge.margin_headroom().await.unwrap_or(1.0);
    metrics::gauge!("mm_desk_margin_headroom").set(headroom);
    if headroom < p.cfg.margin_headroom_floor {
        tracing::error!(
            alert_id = "mm-desk-margin-headroom",
            headroom,
            floor = p.cfg.margin_headroom_floor,
            "hedge margin headroom under the floor"
        );
    }

    // Bleed: rolling (scalp + spread) vs (theta + funding cost).
    let income = pnl.scalp + pnl.spread;
    let cost = (-pnl.theta).max(0.0) + (-pnl.funding).max(0.0);
    bleed_samples.push((now, income, cost));
    let window_ms = (p.cfg.bleed_window_hours * 3_600_000.0) as u64;
    bleed_samples.retain(|(t, _, _)| now.saturating_sub(*t) <= window_ms);
    if let Some((_, income0, cost0)) = bleed_samples.first().copied() {
        let d_income = income - income0;
        let d_cost = cost - cost0;
        metrics::gauge!("mm_desk_bleed_window_net").set(d_income - d_cost);
        // Only meaningful once the window spans real time and any bleed
        // exists at all.
        if bleed_samples.len() > 3 && d_cost > 0.0 && d_income < d_cost {
            tracing::error!(
                alert_id = "mm-desk-bleed",
                window_income = d_income,
                window_cost = d_cost,
                "bleed alarm: (scalp + spread) < (theta + funding) over the window"
            );
        }
    }
}

fn first_fresh_spot(p: &MonitorsParams) -> Option<f64> {
    for (i, _) in p.models.iter().enumerate() {
        let (feed, decimals) = p.market_feeds[i];
        if let Ok(s) = compute_spot_from_cache(
            &p.price_cache,
            feed,
            p.settlement_feed,
            decimals,
            p.settlement_decimals,
            p.staleness,
        ) {
            return Some(s);
        }
    }
    None
}

/// Revalue the live book under the 00-plan stress scenarios and set the
/// V2 gate. Results are logged + exported as gauges.
async fn stress_tick(p: &MonitorsParams) {
    let holdings = p.book.read().holdings.clone();
    let nav = p.shared.exposure.read().nav;
    if nav <= 0.0 || holdings.is_empty() {
        p.shared.stress_blocked.store(false, Ordering::Relaxed);
        return;
    }
    let now = super::auctions::now_ms();
    let mut worst_drawdown: f64 = 0.0;

    // Spot gaps: −60% / +80%, book delta-hedged (the hedge short offsets
    // spot P&L 1:1 on delta; the option legs reprice through the model).
    let hedge_short = p.hedge.position_units().await.unwrap_or(0.0);
    for gap in [-0.60, 0.80] {
        let mut pnl = 0.0;
        for h in &holdings {
            let Some(mi) = p.models.iter().position(|m| m.coin_type == h.asset_coin_type) else {
                continue;
            };
            let (feed, decimals) = p.market_feeds[mi];
            let Ok(spot) = compute_spot_from_cache(
                &p.price_cache,
                feed,
                p.settlement_feed,
                decimals,
                p.settlement_decimals,
                p.staleness,
            ) else {
                continue;
            };
            let t = h.expiry_ms.saturating_sub(now) as f64 / 1000.0 / 86_400.0 / 365.0;
            let k = h.strike_scaled();
            let (sigma, _) = p.models[mi].sigma(spot, k, t);
            let before = p.models[mi].fair_per_unit(h.is_put, spot, k, t, sigma);
            let after = p.models[mi].fair_per_unit(h.is_put, spot * (1.0 + gap), k, t, sigma);
            pnl += (after - before) * h.amount() as f64;
        }
        // Hedge leg: a short of `hedge_short` units gains when spot falls.
        // Applied against the first fresh spot (single-underlying
        // deployments; multi-underlying hedge attribution is a
        // TODO(SO-299) refinement).
        if let Some(spot) = first_fresh_spot(p) {
            pnl += -hedge_short * spot * gap;
        }
        let drawdown = (-pnl / nav).max(0.0);
        worst_drawdown = worst_drawdown.max(drawdown);
        metrics::gauge!("mm_desk_stress_drawdown", "scenario" => if gap < 0.0 { "gap_down_60" } else { "gap_up_80" })
            .set(drawdown);
    }

    // Flat 6 months: pure theta projection at today's bleed rate.
    let theta_cost = p.shared.exposure.read().theta_cost_per_day;
    let flat_6mo = (theta_cost * 182.0 / nav).max(0.0);
    worst_drawdown = worst_drawdown.max(flat_6mo);
    metrics::gauge!("mm_desk_stress_drawdown", "scenario" => "flat_6mo").set(flat_6mo);

    // Funding −50%: the funding leg halves for the projection window.
    let funding = p.hedge.funding_rate_annual().await.unwrap_or(0.0);
    let hedge_notional = hedge_short * first_fresh_spot(p).unwrap_or(0.0);
    let funding_hit = if funding > 0.0 {
        (funding * 0.5 * hedge_notional * (30.0 / 365.0) / nav).max(0.0)
    } else {
        0.0
    };
    worst_drawdown = worst_drawdown.max(funding_hit);
    metrics::gauge!("mm_desk_stress_drawdown", "scenario" => "funding_minus_50").set(funding_hit);

    let blocked = worst_drawdown > p.cfg.stress_max_drawdown;
    p.shared.stress_blocked.store(blocked, Ordering::Relaxed);
    metrics::gauge!("mm_desk_stress_worst_drawdown").set(worst_drawdown);
    if blocked {
        tracing::warn!(
            worst_drawdown,
            gate = p.cfg.stress_max_drawdown,
            "stress gate: blocking new short risk"
        );
    } else {
        tracing::info!(worst_drawdown, "nightly stress complete");
    }
}
