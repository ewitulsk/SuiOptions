//! Signed hedge policy (00-plan V1 §3 — bands not clocks; SO-428, doc 08
//! §4.2): the order/event vocabulary every venue speaks, the `[desk.hedge]`
//! knobs, the band math and the working-order tracker.
//!
//! SIGNED positions: `position_units > 0` is a LONG perp, `< 0` a short;
//! the neutral target is `-book_delta` for call, put, and mixed books. The
//! venue seam itself (`HedgeVenue`, the paper venue) is runtime and lives
//! in `services/mm-bot`; everything here is pure.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// Client-assigned hedge order id (unique per process run).
pub type OrderId = u64;

/// One hedge order: signed market-style size in underlying units
/// (positive = buy / increase position, negative = sell).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HedgeOrder {
    pub id: OrderId,
    /// Signed size, underlying units. Positive buys.
    pub size_units: f64,
    /// Reference spot the caller priced the order at.
    pub spot: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HedgeCommand {
    Submit(HedgeOrder),
    Cancel(OrderId),
    Replace { old: OrderId, new: HedgeOrder },
}

/// One (possibly partial) fill.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fill {
    pub order: OrderId,
    /// Signed size filled (same sign convention as the order).
    pub size_units: f64,
    pub price: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HedgeEvent {
    Acknowledged(OrderId),
    PartiallyFilled(Fill),
    Filled(Fill),
    Rejected { order: OrderId, reason: String },
    Cancelled(OrderId),
}

/// `[desk.hedge]` knobs. Defaults are the 00-plan V1 parameters.
/// `Serialize` so `/desk/state` can echo the effective config (SO-348).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct HedgeConfig {
    /// Rebalance band, % of NAV of net delta notional. Doc 07 §5: risk
    /// reduction saturates at 10–20% bands; the 00-plan 1.5% cost 6.8× the
    /// turnover for 0.8pp of P&L std. Provisional call-book value 15.
    pub band_pct_nav: f64,
    /// Widened band while the required hedge direction is expensive.
    /// Doc 07 §5: 25.
    pub band_wide_pct_nav: f64,
    /// The band widens when the short's funding rate drops below this
    /// (i.e. the short PAYS more than 25%/yr). 00-plan: −0.25.
    pub funding_widen_threshold: f64,
    /// Rebalance check cadence. Bands decide; the clock only samples.
    pub interval_secs: u64,
    /// Paper venue: simulated slippage, bps of spot per fill. Doc 07
    /// §6.1/§11: 3.5 bp Bluefin taker + spread; sweep it.
    pub paper_slippage_bps: f64,
    /// Paper venue: fixed annualized funding rate (0 = flat).
    pub paper_funding_rate_annual: f64,
    /// Paper venue state file (per-underlying suffix is appended).
    pub paper_state_path: String,
    /// Multi-venue roster (`[[desk.hedge.venues]]`). Empty = the legacy
    /// single paper venue built from the `paper_*` knobs above, so
    /// pre-multi-venue configs keep working unchanged.
    pub venues: Vec<HedgeVenueToml>,
    /// Expected-hedge-cost inputs for the bid (doc 08 §4.3, SO-437).
    /// Taker fee, bps of traded notional. Doc 07 §6.1: Bluefin 3.5 bp.
    pub taker_fee_bps: f64,
    /// Flat fee per fill, settlement raw units (Bluefin: 0.03 USDC).
    pub fixed_fee_per_fill: f64,
    /// Expected extra rebalance fills per year per unit of initial hedge
    /// notional (doc 07 §5: ~11.3× per 30d turn at 20% bands ≈ 137/yr).
    pub rebalance_turnover_per_year: f64,
    /// Annual financing rate on cash parked as venue margin.
    pub margin_financing_rate_annual: f64,
    /// Initial margin fraction of hedge notional at the venue.
    pub initial_margin_fraction: f64,
    /// Cancel a working hedge order that has not fully filled after this
    /// long (SO-438). The rebalancer re-plans on the next tick.
    pub order_timeout_secs: u64,
}

impl HedgeConfig {
    /// The bid's venue cost inputs, with the primary venue's slippage.
    pub fn cost_params(&self, slippage_bps: f64) -> pricing::desk::HedgeCostParams {
        pricing::desk::HedgeCostParams {
            slippage_bps,
            taker_fee_bps: self.taker_fee_bps,
            fixed_fee_per_fill: self.fixed_fee_per_fill,
            rebalance_turnover_per_year: self.rebalance_turnover_per_year,
            margin_financing_rate_annual: self.margin_financing_rate_annual,
            initial_margin_fraction: self.initial_margin_fraction,
        }
    }
}

impl Default for HedgeConfig {
    fn default() -> Self {
        Self {
            band_pct_nav: 15.0,
            band_wide_pct_nav: 25.0,
            funding_widen_threshold: -0.25,
            interval_secs: 30,
            paper_slippage_bps: 3.5,
            paper_funding_rate_annual: 0.0,
            paper_state_path: "services/mm-bot/state/paper-hedge".into(),
            venues: Vec::new(),
            taker_fee_bps: 3.5,
            fixed_fee_per_fill: 0.0,
            rebalance_turnover_per_year: 0.0,
            margin_financing_rate_annual: 0.0,
            initial_margin_fraction: 0.10,
            order_timeout_secs: 60,
        }
    }
}

/// One `[[desk.hedge.venues]]` entry. `kind = "paper"` (simulated) is the
/// only kind today; Bluefin is a follow-up behind the same seam.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HedgeVenueToml {
    pub kind: String,
    /// Gauge/alert label + state-file key. Defaults to "paper" for the
    /// first entry, "paper{n}" after.
    pub name: Option<String>,
    /// Defaults to `paper_slippage_bps`.
    pub slippage_bps: Option<f64>,
    /// Defaults to `paper_funding_rate_annual`.
    pub funding_rate_annual: Option<f64>,
    /// Paper only: fraction of each order filled synchronously; the
    /// remainder rests and fills on the next `poll_events` (default 1.0 =
    /// instant full fills). Lets staging exercise the partial-fill path.
    pub fill_fraction: Option<f64>,
}

/// A resolved venue to instantiate (per underlying market).
#[derive(Clone, Debug, PartialEq)]
pub struct VenueSpec {
    pub name: String,
    pub slippage_bps: f64,
    pub funding_rate_annual: f64,
    pub fill_fraction: f64,
}

impl HedgeConfig {
    /// Resolve the venue roster: the `venues` array when present, else
    /// the compat default of ONE paper venue from the legacy `paper_*`
    /// knobs. Always non-empty; the first spec is the desk's primary
    /// (execution) venue.
    pub fn venue_specs(&self) -> Result<Vec<VenueSpec>> {
        if self.venues.is_empty() {
            return Ok(vec![VenueSpec {
                name: "paper".into(),
                slippage_bps: self.paper_slippage_bps,
                funding_rate_annual: self.paper_funding_rate_annual,
                fill_fraction: 1.0,
            }]);
        }
        let mut out = Vec::with_capacity(self.venues.len());
        for (i, v) in self.venues.iter().enumerate() {
            let default_name: String = match v.kind.as_str() {
                "paper" => {
                    if i == 0 { "paper".into() } else { format!("paper{}", i + 1) }
                }
                other => bail!(
                    "[[desk.hedge.venues]] kind {other:?} not supported (only \"paper\")"
                ),
            };
            let name = v.name.clone().unwrap_or(default_name);
            if out.iter().any(|s: &VenueSpec| s.name == name) {
                bail!("[[desk.hedge.venues]] duplicate venue name {name:?}");
            }
            out.push(VenueSpec {
                name,
                slippage_bps: v.slippage_bps.unwrap_or(self.paper_slippage_bps),
                funding_rate_annual: v
                    .funding_rate_annual
                    .unwrap_or(self.paper_funding_rate_annual),
                fill_fraction: v.fill_fraction.unwrap_or(1.0).clamp(0.0, 1.0),
            });
        }
        Ok(out)
    }
}

// ── band math (pure) ───────────────────────────────────────────────────

/// Band width in underlying units: `band_pct · NAV / spot`, using the
/// wide band when funding is below the widen threshold.
pub fn band_units(cfg: &HedgeConfig, nav: f64, spot: f64, funding_annual: f64) -> f64 {
    let pct = if funding_annual < cfg.funding_widen_threshold {
        cfg.band_wide_pct_nav
    } else {
        cfg.band_pct_nav
    };
    band_units_for(pct, nav, spot)
}

/// Band width for an explicit percentage (monitor convenience).
pub fn band_units_for(pct: f64, nav: f64, spot: f64) -> f64 {
    if spot <= 0.0 {
        return f64::INFINITY;
    }
    (pct / 100.0) * nav / spot
}

/// The rebalance decision: rebalance to delta-neutral
/// (`target_perp = -book_delta`, signed) only when the net delta
/// (`book_delta + perp_position`) leaves the band. A negative book delta
/// (puts) targets a LONG perp.
pub fn rebalance_target(
    book_delta_units: f64,
    perp_position_units: f64,
    band_units: f64,
) -> Option<f64> {
    let net = book_delta_units + perp_position_units;
    if net.abs() > band_units {
        Some(-book_delta_units)
    } else {
        None
    }
}

/// The order to submit this tick, if any (SO-438): the signed size that
/// brings `position + working` to the neutral target when the net delta
/// INCLUDING unfilled working orders is outside the band. Counting the
/// working remainder stops a slow fill from being re-submitted every tick.
pub fn plan_hedge_order(
    book_delta_units: f64,
    perp_position_units: f64,
    working_units: f64,
    band_units: f64,
) -> Option<f64> {
    let effective = perp_position_units + working_units;
    let target = rebalance_target(book_delta_units, effective, band_units)?;
    let size = target - effective;
    if size == 0.0 {
        None
    } else {
        Some(size)
    }
}

/// One order the rebalancer submitted that the venue has not finished
/// resolving.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorkingOrder {
    pub size_units: f64,
    pub filled_units: f64,
    pub submitted_ms: u64,
}

impl WorkingOrder {
    pub fn remaining_units(&self) -> f64 {
        self.size_units - self.filled_units
    }
}

/// The rebalancer's view of its working orders (SO-438). Pure: fed by
/// the events a venue returns synchronously and by `poll_events`.
/// Fill-after-cancel races resolve in the venue's favour: a fill for an
/// order this tracker already dropped is still returned to the caller,
/// because the venue's position already reflects it.
#[derive(Debug, Default)]
pub struct OpenOrders {
    orders: std::collections::HashMap<OrderId, WorkingOrder>,
}

impl OpenOrders {
    pub fn submit(&mut self, order: &HedgeOrder, now_ms: u64) {
        self.orders.insert(
            order.id,
            WorkingOrder { size_units: order.size_units, filled_units: 0.0, submitted_ms: now_ms },
        );
    }

    /// Apply one event; returns the fill it carried, if any.
    pub fn apply(&mut self, ev: &HedgeEvent) -> Option<Fill> {
        match ev {
            HedgeEvent::Acknowledged(_) => None,
            HedgeEvent::PartiallyFilled(f) => {
                if let Some(w) = self.orders.get_mut(&f.order) {
                    w.filled_units += f.size_units;
                    // A partial that completes the order closes it.
                    if w.remaining_units().abs() <= 1e-12 {
                        self.orders.remove(&f.order);
                    }
                }
                Some(*f)
            }
            HedgeEvent::Filled(f) => {
                self.orders.remove(&f.order);
                Some(*f)
            }
            HedgeEvent::Rejected { order, .. } | HedgeEvent::Cancelled(order) => {
                self.orders.remove(order);
                None
            }
        }
    }

    /// Signed unfilled size across working orders.
    pub fn working_units(&self) -> f64 {
        self.orders.values().map(WorkingOrder::remaining_units).sum()
    }

    /// Orders working longer than `timeout_ms`.
    pub fn stale(&self, now_ms: u64, timeout_ms: u64) -> Vec<OrderId> {
        let mut ids: Vec<OrderId> = self
            .orders
            .iter()
            .filter(|(_, w)| now_ms.saturating_sub(w.submitted_ms) >= timeout_ms)
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        ids
    }

    pub fn len(&self) -> usize {
        self.orders.len()
    }

    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_fills_reduce_working_and_plan_avoids_resubmit() {
        let mut open = OpenOrders::default();
        let order = HedgeOrder { id: 1, size_units: -100.0, spot: 10.0 };
        open.submit(&order, 0);
        assert_eq!(open.working_units(), -100.0);
        // Book delta +100, venue position 0, working −100: effective net
        // is 0 → nothing to submit while the order works.
        assert_eq!(plan_hedge_order(100.0, 0.0, open.working_units(), 10.0), None);
        let f = open.apply(&HedgeEvent::PartiallyFilled(Fill { order: 1, size_units: -40.0, price: 10.0 }));
        assert_eq!(f.map(|f| f.size_units), Some(-40.0));
        assert_eq!(open.working_units(), -60.0);
        // Venue now reports −40; still nothing to do.
        assert_eq!(plan_hedge_order(100.0, -40.0, open.working_units(), 10.0), None);
        // A partial that completes the order closes it.
        open.apply(&HedgeEvent::PartiallyFilled(Fill { order: 1, size_units: -60.0, price: 10.0 }));
        assert!(open.is_empty());
        assert_eq!(open.working_units(), 0.0);
    }

    #[test]
    fn fill_after_cancel_is_still_counted() {
        let mut open = OpenOrders::default();
        open.submit(&HedgeOrder { id: 7, size_units: 50.0, spot: 10.0 }, 0);
        assert_eq!(open.stale(59_999, 60_000), Vec::<OrderId>::new());
        assert_eq!(open.stale(60_000, 60_000), vec![7]);
        // Cancel drops the working remainder…
        assert!(open.apply(&HedgeEvent::Cancelled(7)).is_none());
        assert_eq!(open.working_units(), 0.0);
        // …but a fill that raced the cancel is still surfaced to the
        // caller: the venue's position already includes it.
        let late = open.apply(&HedgeEvent::Filled(Fill { order: 7, size_units: 50.0, price: 10.0 }));
        assert_eq!(late.map(|f| f.size_units), Some(50.0));
        assert!(open.is_empty());
        // Rejects clear too.
        open.submit(&HedgeOrder { id: 8, size_units: 5.0, spot: 10.0 }, 0);
        open.apply(&HedgeEvent::Rejected { order: 8, reason: "x".into() });
        assert!(open.is_empty());
    }

    #[test]
    fn band_widens_when_funding_is_expensive() {
        let cfg = HedgeConfig::default();
        // NAV 1e9, spot 100 → base band = 15% × 1e9 / 100 = 1.5e6 units
        // (doc 07 §5 correction, SO-436).
        let base = band_units(&cfg, 1e9, 100.0, 0.0);
        assert!((base - 1_500_000.0).abs() < 1e-6);
        // Funding below −25%: wide band (25%).
        let wide = band_units(&cfg, 1e9, 100.0, -0.30);
        assert!((wide - 2_500_000.0).abs() < 1e-6);
        // Receiving funding keeps the tight band.
        assert!((band_units(&cfg, 1e9, 100.0, 0.10) - base).abs() < 1e-9);
    }

    #[test]
    fn rebalance_only_outside_band_signed() {
        // Long calls (book delta +100), short −60 → net +40, inside 50: hold.
        assert_eq!(rebalance_target(100.0, -60.0, 50.0), None);
        // Net +70 outside the band: target the full short (−100).
        assert_eq!(rebalance_target(100.0, -30.0, 50.0), Some(-100.0));
        // Over-hedged beyond the band: buy back up to −10.
        assert_eq!(rebalance_target(10.0, -200.0, 50.0), Some(-10.0));
        // Long puts (book delta −80): target a LONG perp (+80).
        assert_eq!(rebalance_target(-80.0, 0.0, 50.0), Some(80.0));
        // A mixed book already netted needs no trade.
        assert_eq!(rebalance_target(-80.0, 80.0, 50.0), None);
    }

    #[test]
    fn legacy_single_venue_config_still_parses_to_one_paper_venue() {
        // A pre-multi-venue [desk.hedge] TOML: no `venues` array at all.
        let cfg: HedgeConfig = toml::from_str(
            "band_pct_nav = 2.0\npaper_slippage_bps = 3.0\npaper_funding_rate_annual = 0.1\n",
        )
        .unwrap();
        assert!((cfg.band_pct_nav - 2.0).abs() < 1e-12);
        let specs = cfg.venue_specs().unwrap();
        assert_eq!(
            specs,
            vec![VenueSpec {
                name: "paper".into(),
                slippage_bps: 3.0,
                funding_rate_annual: 0.1,
                fill_fraction: 1.0,
            }]
        );
    }

    #[test]
    fn venues_array_parses_with_defaults_and_rejects_unknown_kinds() {
        let cfg: HedgeConfig = toml::from_str(
            "paper_slippage_bps = 3.0\n\
             [[venues]]\n\
             kind = \"paper\"\n\
             [[venues]]\n\
             kind = \"paper\"\n\
             name = \"paper-b\"\n\
             slippage_bps = 7.0\n\
             funding_rate_annual = -0.2\n",
        )
        .unwrap();
        let specs = cfg.venue_specs().unwrap();
        assert_eq!(specs.len(), 2);
        // First entry inherits the legacy knobs and the "paper" name (and
        // with it the legacy state-file path).
        assert_eq!(
            specs[0],
            VenueSpec { name: "paper".into(), slippage_bps: 3.0, funding_rate_annual: 0.0, fill_fraction: 1.0 }
        );
        assert_eq!(
            specs[1],
            VenueSpec { name: "paper-b".into(), slippage_bps: 7.0, funding_rate_annual: -0.2, fill_fraction: 1.0 }
        );

        let bad: HedgeConfig =
            toml::from_str("[[venues]]\nkind = \"bluefin\"\n").unwrap();
        assert!(bad.venue_specs().is_err());
    }
}
