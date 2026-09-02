//! Desk monitors + nightly stress suite (00-plan Phase 4).
//!
//! One task emits gauges each tick and fires `error!(alert_id = …)` on
//! breach:
//!   - `mm-desk-delta-band`      net-of-hedge delta outside the band
//!   - `mm-desk-vega-cap`        |net vega| over the cap
//!   - `mm-desk-theta-governor`  theta bleed over the hard cap
//!   - `mm-desk-bleed`           rolling (scalp+spread) < (theta+funding)
//!   - `mm-desk-reserves`        reservations + deployed over NAV
//!   - `mm-desk-margin-headroom` a hedge venue's margin headroom under
//!     the floor (the alert names the venue)
//!   - `mm-desk-kill-switch`     the NAV-drawdown switch latched
//!
//! Multi-venue aggregation (SO-299): the monitors hold the whole hedge
//! roster. The delta band compares each underlying's book delta against
//! the TOTAL signed position across that underlying's venues; margin headroom is
//! the MIN across venues; the funding rate fed to pricing
//! (`DeskShared::funding_rate_annual`) is the notional-weighted average
//! across venues (simple mean while every venue is flat). Gauges carry
//! `venue`/`symbol` labels.
//!
//! The nightly stress job revalues the live book via the model at
//! −60% / +80% spot gaps, projects theta over a flat 6 months, and
//! haircuts funding −50%; worst drawdown > 25% NAV sets the V2 gate
//! (`DeskShared::stress_blocked`) that blocks new short risk.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use pyth_client::{PriceCache, PriceFeedId};

use crate::pricing::{compute_spot_from_cache, Staleness};

use super::book::Book;
use super::hedge::HedgeVenue;
use super::limits::LimitsConfig;
use super::model::MarketModel;
use super::DeskShared;

/// `[desk.monitors]` knobs. `Serialize` so `/desk/state` can echo the
/// effective config (SO-348).
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
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
    /// Stress spot gaps (fractions): the crash the long-perp put hedges
    /// are sized for and the rally the short-perp call hedges are sized
    /// for. 00-plan: −60% / +80%. Also the capital policy's top-up gap.
    pub stress_gap_down: f64,
    pub stress_gap_up: f64,
}

impl Default for MonitorsConfig {
    fn default() -> Self {
        Self {
            interval_secs: 60,
            bleed_window_hours: 24.0,
            margin_headroom_floor: 0.25,
            stress_interval_secs: 86_400,
            stress_max_drawdown: 0.25,
            stress_gap_down: 0.60,
            stress_gap_up: 0.80,
        }
    }
}

/// One monitored hedge-venue instance: `venue` hedges `symbol`.
pub struct MonitorVenue {
    pub symbol: String,
    pub venue: Arc<dyn HedgeVenue>,
}

/// One venue's monitor snapshot (pure input to [`aggregate_venues`]).
#[derive(Clone, Debug, PartialEq)]
pub struct VenueReading {
    pub name: String,
    pub symbol: String,
    /// Signed perp position, underlying units (positive = long — SO-428).
    pub position_units: f64,
    pub funding_annual: f64,
    pub margin_headroom: f64,
    /// |position| × spot, settlement raw — the funding weight.
    pub notional: f64,
}

/// Cross-venue aggregates the monitors act on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VenueAggregate {
    /// Total SIGNED hedge position per underlying symbol
    /// (delta-band input; positive = long).
    pub position_by_symbol: HashMap<String, f64>,
    /// The venue with the least margin room: (venue name, headroom).
    pub min_headroom: Option<(String, f64)>,
    /// Notional-weighted funding across venues; simple mean when every
    /// venue is flat (all weights zero).
    pub funding_weighted: f64,
}

/// Pure multi-venue aggregation (unit-tested).
pub fn aggregate_venues(readings: &[VenueReading]) -> VenueAggregate {
    let mut agg = VenueAggregate::default();
    let mut weighted_sum = 0.0;
    let mut weight = 0.0;
    for r in readings {
        *agg.position_by_symbol.entry(r.symbol.clone()).or_default() += r.position_units;
        weighted_sum += r.funding_annual * r.notional;
        weight += r.notional;
        let worse = match &agg.min_headroom {
            Some((_, h)) => r.margin_headroom < *h,
            None => true,
        };
        if worse {
            agg.min_headroom = Some((r.name.clone(), r.margin_headroom));
        }
    }
    agg.funding_weighted = if weight > 0.0 {
        weighted_sum / weight
    } else if readings.is_empty() {
        0.0
    } else {
        readings.iter().map(|r| r.funding_annual).sum::<f64>() / readings.len() as f64
    };
    agg
}

/// Read one venue's snapshot at `spot` (its underlying's price); `None`
/// when any venue call errors (the tick logs and moves on).
pub async fn read_venue(mv: &MonitorVenue, spot: f64) -> Option<VenueReading> {
    let position_units = mv.venue.position_units().await.ok()?;
    let funding_annual = mv.venue.funding_rate_annual().await.ok()?;
    let margin_headroom = mv.venue.margin_headroom().await.ok()?;
    Some(VenueReading {
        name: mv.venue.name().to_string(),
        symbol: mv.symbol.clone(),
        position_units,
        funding_annual,
        margin_headroom,
        notional: position_units.abs() * spot.max(0.0),
    })
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
    /// The whole hedge roster (every venue instance, per underlying).
    pub venues: Vec<MonitorVenue>,
    pub hedge_band_pct_nav: f64,
    /// Venue margin fractions of hedge notional (`[desk.hedge]` initial,
    /// `[desk.capital]` maintenance) — the capital snapshot's margin
    /// picture (SO-444).
    pub initial_margin_fraction: f64,
    pub maintenance_margin_fraction: f64,
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
        venues: p
            .venues
            .iter()
            .map(|v| MonitorVenue { symbol: v.symbol.clone(), venue: Arc::clone(&v.venue) })
            .collect(),
        hedge_band_pct_nav: p.hedge_band_pct_nav,
        initial_margin_fraction: p.initial_margin_fraction,
        maintenance_margin_fraction: p.maintenance_margin_fraction,
    }
}

/// Fresh spot per model symbol (venues + stress share it per tick).
fn spots_by_symbol(p: &MonitorsParams) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    for (i, model) in p.models.iter().enumerate() {
        let (feed, decimals) = p.market_feeds[i];
        if let Ok(s) = compute_spot_from_cache(
            &p.price_cache,
            feed,
            p.settlement_feed,
            decimals,
            p.settlement_decimals,
            p.staleness,
        ) {
            out.insert(model.symbol.clone(), s);
        }
    }
    out
}

/// Read every venue on the roster; failed venue reads are logged and
/// dropped from the tick's aggregates.
async fn read_all_venues(p: &MonitorsParams, spots: &HashMap<String, f64>) -> Vec<VenueReading> {
    let mut readings = Vec::with_capacity(p.venues.len());
    for mv in &p.venues {
        let spot = spots.get(&mv.symbol).copied().unwrap_or(0.0);
        match read_venue(mv, spot).await {
            Some(r) => readings.push(r),
            None => tracing::warn!(
                venue = mv.venue.name(),
                symbol = %mv.symbol,
                "hedge venue read failed; excluded from this tick's aggregates"
            ),
        }
    }
    readings
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

    // Venue roster: per-venue labelled gauges + margin-floor alerts, then
    // the cross-venue aggregates.
    let spots = spots_by_symbol(p);
    let readings = read_all_venues(p, &spots).await;
    for r in &readings {
        metrics::gauge!("mm_desk_hedge_position_units", "venue" => r.name.clone(), "symbol" => r.symbol.clone())
            .set(r.position_units);
        metrics::gauge!("mm_desk_funding_rate_annual", "venue" => r.name.clone(), "symbol" => r.symbol.clone())
            .set(r.funding_annual);
        metrics::gauge!("mm_desk_margin_headroom", "venue" => r.name.clone(), "symbol" => r.symbol.clone())
            .set(r.margin_headroom);
        if r.margin_headroom < p.cfg.margin_headroom_floor {
            tracing::error!(
                alert_id = "mm-desk-margin-headroom",
                venue = %r.name,
                symbol = %r.symbol,
                headroom = r.margin_headroom,
                floor = p.cfg.margin_headroom_floor,
                "hedge margin headroom under the floor"
            );
        }
    }
    let agg = aggregate_venues(&readings);
    if let Some((_, headroom)) = &agg.min_headroom {
        metrics::gauge!("mm_desk_margin_headroom_min").set(*headroom);
    }
    if !readings.is_empty() {
        // The funding input to pricing: notional-weighted across venues.
        metrics::gauge!("mm_desk_funding_rate_annual_weighted").set(agg.funding_weighted);
        *p.shared.funding_rate_annual.write() = agg.funding_weighted;
        // The venue margin picture the capital snapshot reads (SO-444):
        // margin posted = Σ|position|·spot × initial fraction, the
        // maintenance requirement, and the min headroom across venues.
        let notional: f64 = readings.iter().map(|r| r.notional).sum();
        *p.shared.venue_margin.write() = super::limits::VenueMarginInputs {
            initial_margin: notional * p.initial_margin_fraction,
            maintenance_margin: notional * p.maintenance_margin_fraction,
            headroom: agg.min_headroom.as_ref().map(|(_, h)| *h).unwrap_or(1.0),
            at_ms: now,
        };
    }

    // Delta vs band, per market, against the TOTAL signed position
    // across that market's venues.
    let deltas = p.shared.book_delta_units.read().clone();
    for model in p.models.iter() {
        let delta_units = deltas.get(&model.coin_type).copied().unwrap_or(0.0);
        metrics::gauge!("mm_desk_book_delta_units", "symbol" => model.symbol.clone())
            .set(delta_units);
        let Some(spot) = spots.get(&model.symbol).copied() else {
            continue;
        };
        let hedge_position = agg.position_by_symbol.get(&model.symbol).copied().unwrap_or(0.0);
        let band = super::hedge::band_units_for(p.hedge_band_pct_nav, nav, spot);
        let net = delta_units + hedge_position;
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

/// Revalue the live book under the 00-plan stress scenarios and set the
/// stress gate. Results are logged + exported as gauges. Hedge legs use
/// the per-underlying total SIGNED position across venues at that
/// underlying's spot.
async fn stress_tick(p: &MonitorsParams) {
    let holdings = p.book.read().holdings.clone();
    let nav = p.shared.exposure.read().nav;
    let now = super::auctions::now_ms();
    if nav <= 0.0 || holdings.is_empty() {
        p.shared.stress_blocked.store(false, Ordering::Relaxed);
        *p.shared.stress.write() =
            Some(super::StressSnapshot { at_ms: now, ..Default::default() });
        return;
    }
    let mut worst_drawdown: f64 = 0.0;
    let mut gap_drawdowns = [0.0f64; 2];

    let spots = spots_by_symbol(p);
    let readings = read_all_venues(p, &spots).await;
    let agg = aggregate_venues(&readings);
    // Per-symbol SIGNED hedge notional (position × that symbol's spot;
    // negative = net short) and the short-only slice (funding stress:
    // only shorts RECEIVE positive funding, so only they lose income
    // when it halves).
    let hedge_notional: f64 = agg
        .position_by_symbol
        .iter()
        .map(|(sym, pos)| pos * spots.get(sym).copied().unwrap_or(0.0))
        .sum();
    let short_notional: f64 = agg
        .position_by_symbol
        .iter()
        .map(|(sym, pos)| (-pos * spots.get(sym).copied().unwrap_or(0.0)).max(0.0))
        .sum();

    // Spot gaps (config; 00-plan −60% / +80%), book delta-hedged (each
    // underlying's hedge short offsets spot P&L 1:1 on delta; the option
    // legs reprice through the model).
    for gap in [-p.cfg.stress_gap_down.abs(), p.cfg.stress_gap_up.abs()] {
        let mut pnl = 0.0;
        for h in &holdings {
            let Some(mi) = p.models.iter().position(|m| m.coin_type == h.asset_coin_type) else {
                continue;
            };
            let Some(spot) = spots.get(&p.models[mi].symbol).copied() else {
                continue;
            };
            let t = h.expiry_ms.saturating_sub(now) as f64 / 1000.0 / 86_400.0 / 365.0;
            let k = h.strike_scaled();
            let (sigma, _) = p.models[mi].sigma(spot, k, t);
            let before = p.models[mi].fair_per_unit(h.is_put, spot, k, t, sigma);
            let after = p.models[mi].fair_per_unit(h.is_put, spot * (1.0 + gap), k, t, sigma);
            pnl += (after - before) * h.amount() as f64;
        }
        // Hedge legs: signed — a long gains when spot rises, a short
        // when it falls.
        pnl += hedge_notional * gap;
        let drawdown = (-pnl / nav).max(0.0);
        worst_drawdown = worst_drawdown.max(drawdown);
        gap_drawdowns[usize::from(gap > 0.0)] = drawdown;
        metrics::gauge!("mm_desk_stress_drawdown", "scenario" => if gap < 0.0 { "gap_down_60" } else { "gap_up_80" })
            .set(drawdown);
    }

    // Flat 6 months: pure theta projection at today's bleed rate.
    let theta_cost = p.shared.exposure.read().theta_cost_per_day;
    let flat_6mo = (theta_cost * 182.0 / nav).max(0.0);
    worst_drawdown = worst_drawdown.max(flat_6mo);
    metrics::gauge!("mm_desk_stress_drawdown", "scenario" => "flat_6mo").set(flat_6mo);

    // Funding −50%: the (notional-weighted) funding leg halves for the
    // projection window.
    let funding = agg.funding_weighted;
    let funding_hit = if funding > 0.0 {
        (funding * 0.5 * short_notional * (30.0 / 365.0) / nav).max(0.0)
    } else {
        0.0
    };
    worst_drawdown = worst_drawdown.max(funding_hit);
    metrics::gauge!("mm_desk_stress_drawdown", "scenario" => "funding_minus_50").set(funding_hit);

    let blocked = worst_drawdown > p.cfg.stress_max_drawdown;
    p.shared.stress_blocked.store(blocked, Ordering::Relaxed);
    *p.shared.stress.write() = Some(super::StressSnapshot {
        at_ms: now,
        gap_down_60: gap_drawdowns[0],
        gap_up_80: gap_drawdowns[1],
        flat_6mo,
        funding_minus_50: funding_hit,
        worst_drawdown,
        blocked,
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desk::hedge::PaperVenue;

    fn reading(name: &str, symbol: &str, position: f64, funding: f64, headroom: f64, spot: f64) -> VenueReading {
        VenueReading {
            name: name.into(),
            symbol: symbol.into(),
            position_units: position,
            funding_annual: funding,
            margin_headroom: headroom,
            notional: position.abs() * spot,
        }
    }

    #[test]
    fn aggregate_sums_positions_takes_min_headroom_and_weights_funding() {
        let readings = vec![
            reading("paper", "TBTC", -10.0, 0.10, 0.50, 100.0), // notional 1000
            reading("paper-b", "TBTC", -5.0, 0.40, 0.20, 100.0), // notional 500
            reading("paper", "TWAL", 7.0, 0.10, 0.90, 0.0),      // flat weight
        ];
        let agg = aggregate_venues(&readings);
        assert!((agg.position_by_symbol["TBTC"] - -15.0).abs() < 1e-9);
        assert!((agg.position_by_symbol["TWAL"] - 7.0).abs() < 1e-9);
        // Min headroom names the venue.
        assert_eq!(agg.min_headroom, Some(("paper-b".into(), 0.20)));
        // Weighted funding: (0.10×1000 + 0.40×500) / 1500 = 0.20.
        assert!((agg.funding_weighted - 0.20).abs() < 1e-9, "{}", agg.funding_weighted);
    }

    #[test]
    fn aggregate_falls_back_to_mean_funding_when_all_flat() {
        let readings = vec![
            reading("a", "TBTC", 0.0, 0.10, 1.0, 100.0),
            reading("b", "TBTC", 0.0, 0.30, 1.0, 100.0),
        ];
        let agg = aggregate_venues(&readings);
        assert!((agg.funding_weighted - 0.20).abs() < 1e-9);
        assert_eq!(aggregate_venues(&[]).funding_weighted, 0.0);
    }

    #[tokio::test]
    async fn two_paper_venues_aggregate_summed_delta_and_min_margin() {
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let path_a = dir.join(format!("mm-desk-mv-a-{pid}.json"));
        let path_b = dir.join(format!("mm-desk-mv-b-{pid}.json"));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
        let a = PaperVenue::load_named("paper", path_a.clone(), 0.0, 0.10);
        let b = PaperVenue::load_named("paper-b", path_b.clone(), 0.0, 0.30);
        use crate::desk::hedge::{HedgeCommand, HedgeOrder};
        a.execute(HedgeCommand::Submit(HedgeOrder { id: 1, size_units: -10.0, spot: 100.0 }))
            .await
            .unwrap();
        b.execute(HedgeCommand::Submit(HedgeOrder { id: 2, size_units: -5.0, spot: 100.0 }))
            .await
            .unwrap();
        let venues = vec![
            MonitorVenue { symbol: "TBTC".into(), venue: Arc::new(a) },
            MonitorVenue { symbol: "TBTC".into(), venue: Arc::new(b) },
        ];
        let mut readings = Vec::new();
        for mv in &venues {
            readings.push(read_venue(mv, 100.0).await.unwrap());
        }
        let agg = aggregate_venues(&readings);
        // Summed signed position across both venues on the same underlying.
        assert!((agg.position_by_symbol["TBTC"] - -15.0).abs() < 1e-9);
        // Paper margin never binds: min is 1.0 and still names a venue.
        assert_eq!(agg.min_headroom.as_ref().map(|(_, h)| *h), Some(1.0));
        // Weighted funding: (0.10×1000 + 0.30×500) / 1500.
        assert!((agg.funding_weighted - (0.10 * 1000.0 + 0.30 * 500.0) / 1500.0).abs() < 1e-9);
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }
}
