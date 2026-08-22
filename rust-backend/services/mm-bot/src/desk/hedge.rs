//! Delta hedging: the `HedgeVenue` seam, the `paper` venue (simulated
//! fills at oracle spot, real accounting persisted to disk), and the
//! band rebalancer (00-plan V1 §3 — bands not clocks).
//!
//! SIGNED positions (SO-428, doc 08 §4.2): `position_units > 0` is a
//! LONG perp, `< 0` a short; the neutral target is `-book_delta` for
//! call, put, and mixed books. The venue interface is order/event
//! oriented — commands in, acknowledgement/fill/reject events out — so
//! live venues (Bluefin) and the backtester's simulated venues share one
//! seam. The paper venue resolves every order synchronously (ack + full
//! fill in the returned events); a live venue returns what it has and
//! delivers the rest through its event stream.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
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

/// A perp (or equivalent) venue hedging one underlying.
#[async_trait]
pub trait HedgeVenue: Send + Sync {
    fn name(&self) -> &str;
    /// Current SIGNED perp position in underlying units
    /// (positive = long, negative = short).
    async fn position_units(&self) -> Result<f64>;
    /// Execute one command. The returned events are everything the venue
    /// resolved synchronously; async outcomes arrive through the venue's
    /// own event stream when a live venue lands behind this seam.
    async fn execute(&self, cmd: HedgeCommand) -> Result<Vec<HedgeEvent>>;
    /// Annualized funding rate, market convention: positive = longs PAY
    /// shorts (a short receives, a long pays).
    async fn funding_rate_annual(&self) -> Result<f64>;
    /// Margin headroom as a fraction of the position's requirement
    /// (1.0 = fully free; 0.0 = at margin call).
    async fn margin_headroom(&self) -> Result<f64>;
    /// Cumulative realized P&L on the venue (settlement raw units). Feeds
    /// the scalp attribution line; venues without statements report 0.
    async fn realized_pnl(&self) -> Result<f64> {
        Ok(0.0)
    }
}

/// `[desk.hedge]` knobs. Defaults are the 00-plan V1 parameters.
/// `Serialize` so `/desk/state` can echo the effective config (SO-348).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct HedgeConfig {
    /// Rebalance band, % of NAV of net delta notional. 00-plan: 1.5.
    pub band_pct_nav: f64,
    /// Widened band while shorting funding is expensive. 00-plan: 2.5.
    pub band_wide_pct_nav: f64,
    /// The band widens when the short's funding rate drops below this
    /// (i.e. the short PAYS more than 25%/yr). 00-plan: −0.25.
    pub funding_widen_threshold: f64,
    /// Rebalance check cadence. Bands decide; the clock only samples.
    pub interval_secs: u64,
    /// Paper venue: simulated slippage, bps of spot per fill.
    pub paper_slippage_bps: f64,
    /// Paper venue: fixed annualized funding rate (0 = flat).
    pub paper_funding_rate_annual: f64,
    /// Paper venue state file (per-underlying suffix is appended).
    pub paper_state_path: String,
    /// Multi-venue roster (`[[desk.hedge.venues]]`). Empty = the legacy
    /// single paper venue built from the `paper_*` knobs above, so
    /// pre-multi-venue configs keep working unchanged.
    pub venues: Vec<HedgeVenueToml>,
}

impl Default for HedgeConfig {
    fn default() -> Self {
        Self {
            band_pct_nav: 1.5,
            band_wide_pct_nav: 2.5,
            funding_widen_threshold: -0.25,
            interval_secs: 30,
            paper_slippage_bps: 5.0,
            paper_funding_rate_annual: 0.0,
            paper_state_path: "services/mm-bot/state/paper-hedge".into(),
            venues: Vec::new(),
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
}

/// A resolved venue to instantiate (per underlying market).
#[derive(Clone, Debug, PartialEq)]
pub struct VenueSpec {
    pub name: String,
    pub slippage_bps: f64,
    pub funding_rate_annual: f64,
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

// ── paper venue ────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PaperState {
    /// Signed perp position, underlying units (positive = long).
    pub position_units: f64,
    /// Volume-weighted average entry price of the open position.
    pub avg_entry: f64,
    /// Realized P&L (settlement raw units), fills only.
    pub realized_pnl: f64,
    /// Total slippage paid (already included in realized_pnl).
    pub slippage_paid: f64,
    /// Cumulative traded notional (diagnostics).
    pub traded_notional: f64,
}

/// Simulated perp venue: fills at oracle spot ± slippage, accounting is
/// real and persisted to a JSON state file so restarts don't reset the
/// position.
pub struct PaperVenue {
    name: String,
    path: PathBuf,
    slippage_bps: f64,
    funding_rate_annual: f64,
    state: tokio::sync::Mutex<PaperState>,
}

impl PaperVenue {
    pub fn load(path: PathBuf, slippage_bps: f64, funding_rate_annual: f64) -> Self {
        Self::load_named("paper", path, slippage_bps, funding_rate_annual)
    }

    /// A paper venue with an explicit monitor label (multi-venue roster).
    /// Legacy (pre-SO-428) state files stored `short_units` with
    /// positive = short; they migrate to the signed convention on load.
    pub fn load_named(
        name: impl Into<String>,
        path: PathBuf,
        slippage_bps: f64,
        funding_rate_annual: f64,
    ) -> Self {
        let state = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .map(|mut v| {
                // Sign-flip migration: `short_units: 10` ⇒ position −10.
                if let Some(short) = v.get("short_units").and_then(|s| s.as_f64()) {
                    if v.get("position_units").is_none() {
                        v["position_units"] = serde_json::json!(-short);
                    }
                }
                serde_json::from_value::<PaperState>(v).unwrap_or_default()
            })
            .unwrap_or_default();
        Self {
            name: name.into(),
            path,
            slippage_bps,
            funding_rate_annual,
            state: tokio::sync::Mutex::new(state),
        }
    }

    fn persist(&self, state: &PaperState) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(&self.path, serde_json::to_string_pretty(state)?)
            .with_context(|| format!("writing {}", self.path.display()))
    }

    /// Apply one signed fill to the state (extracted for unit tests).
    /// `delta_units > 0` buys (extends a long / reduces a short). Returns
    /// the fill price, or `None` for a no-op. Handles extend, reduce,
    /// close, and direction reversal (the closed slice realizes P&L; the
    /// remainder re-opens at the fill price).
    fn apply_fill(
        state: &mut PaperState,
        delta_units: f64,
        spot: f64,
        slippage_bps: f64,
    ) -> Option<f64> {
        if delta_units == 0.0 || spot <= 0.0 {
            return None;
        }
        let slip = spot * slippage_bps / 10_000.0;
        // Buys pay above spot; sells receive below it.
        let px = spot + slip * delta_units.signum();
        state.traded_notional += delta_units.abs() * spot;
        state.slippage_paid += delta_units.abs() * slip;
        let pos = state.position_units;
        if pos == 0.0 || pos.signum() == delta_units.signum() {
            // Extend: new VWAP entry.
            let new = pos + delta_units;
            state.avg_entry = (state.avg_entry * pos.abs() + px * delta_units.abs()) / new.abs();
            state.position_units = new;
        } else {
            // Reduce / close / reverse. Realize on the closed slice:
            // long realizes (exit − entry), short (entry − exit).
            let close = delta_units.abs().min(pos.abs());
            state.realized_pnl += (px - state.avg_entry) * close * pos.signum();
            let new = pos + delta_units;
            if new.abs() <= 1e-12 {
                state.position_units = 0.0;
                state.avg_entry = 0.0;
            } else if new.signum() != pos.signum() {
                // Reversal: the surviving remainder opened at this fill.
                state.position_units = new;
                state.avg_entry = px;
            } else {
                // Plain reduce: entry unchanged.
                state.position_units = new;
            }
        }
        Some(px)
    }

    /// Current state snapshot (monitors / P&L attribution).
    pub async fn snapshot(&self) -> PaperState {
        self.state.lock().await.clone()
    }
}

#[async_trait]
impl HedgeVenue for PaperVenue {
    fn name(&self) -> &str {
        &self.name
    }

    async fn position_units(&self) -> Result<f64> {
        Ok(self.state.lock().await.position_units)
    }

    async fn execute(&self, cmd: HedgeCommand) -> Result<Vec<HedgeEvent>> {
        match cmd {
            HedgeCommand::Submit(order) => {
                if order.size_units == 0.0 || order.spot <= 0.0 {
                    return Ok(vec![HedgeEvent::Rejected {
                        order: order.id,
                        reason: "zero size or non-positive spot".into(),
                    }]);
                }
                let mut state = self.state.lock().await;
                let px = Self::apply_fill(
                    &mut state,
                    order.size_units,
                    order.spot,
                    self.slippage_bps,
                )
                .expect("nonzero order fills on the paper venue");
                self.persist(&state)?;
                tracing::info!(
                    venue = %self.name,
                    order = order.id,
                    size = order.size_units,
                    position = state.position_units,
                    realized = state.realized_pnl,
                    "hedge order filled (paper)"
                );
                Ok(vec![
                    HedgeEvent::Acknowledged(order.id),
                    HedgeEvent::Filled(Fill {
                        order: order.id,
                        size_units: order.size_units,
                        price: px,
                    }),
                ])
            }
            // Paper orders fill instantly; nothing ever rests.
            HedgeCommand::Cancel(id) => Ok(vec![HedgeEvent::Cancelled(id)]),
            HedgeCommand::Replace { old, new } => {
                let mut events = vec![HedgeEvent::Cancelled(old)];
                events.extend(self.execute(HedgeCommand::Submit(new)).await?);
                Ok(events)
            }
        }
    }

    async fn funding_rate_annual(&self) -> Result<f64> {
        Ok(self.funding_rate_annual)
    }

    async fn margin_headroom(&self) -> Result<f64> {
        // Paper margin is never called; report fully free.
        Ok(1.0)
    }

    async fn realized_pnl(&self) -> Result<f64> {
        Ok(self.state.lock().await.realized_pnl)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_widens_when_funding_is_expensive() {
        let cfg = HedgeConfig::default();
        // NAV 1e9, spot 100 → base band = 1.5% × 1e9 / 100 = 150_000 units.
        let base = band_units(&cfg, 1e9, 100.0, 0.0);
        assert!((base - 150_000.0).abs() < 1e-6);
        // Funding below −25%: wide band (2.5%).
        let wide = band_units(&cfg, 1e9, 100.0, -0.30);
        assert!((wide - 250_000.0).abs() < 1e-6);
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
    fn paper_fill_accounting_round_trips_short() {
        let mut s = PaperState::default();
        // Sell 10 at spot 100, 10bps slip → entry 99.9, position −10.
        PaperVenue::apply_fill(&mut s, -10.0, 100.0, 10.0);
        assert!((s.position_units - -10.0).abs() < 1e-12);
        assert!((s.avg_entry - 99.9).abs() < 1e-9);
        // Extend the short 10 more at 110 → entry VWAP (99.9 + 109.89)/2.
        PaperVenue::apply_fill(&mut s, -10.0, 110.0, 10.0);
        assert!((s.avg_entry - (99.9 + 109.89) / 2.0).abs() < 1e-9);
        // Buy the whole 20 back at 90 (pays 90.09): pnl = (entry − exit)×20.
        PaperVenue::apply_fill(&mut s, 20.0, 90.0, 10.0);
        let expected = ((99.9 + 109.89) / 2.0 - 90.09) * 20.0;
        assert!((s.realized_pnl - expected).abs() < 1e-6, "{}", s.realized_pnl);
        assert_eq!(s.position_units, 0.0);
        assert_eq!(s.avg_entry, 0.0);
        // Slippage: 10×0.1 + 10×0.11 + 20×0.09 = 3.9.
        assert!((s.slippage_paid - 3.9).abs() < 1e-9);
    }

    #[test]
    fn paper_fill_accounting_round_trips_long() {
        let mut s = PaperState::default();
        // Buy 10 at 100 (pays 100.1), sell at 110 (receives 109.89).
        PaperVenue::apply_fill(&mut s, 10.0, 100.0, 10.0);
        assert!((s.position_units - 10.0).abs() < 1e-12);
        assert!((s.avg_entry - 100.1).abs() < 1e-9);
        PaperVenue::apply_fill(&mut s, -10.0, 110.0, 10.0);
        assert!((s.realized_pnl - (109.89 - 100.1) * 10.0).abs() < 1e-9);
        assert_eq!(s.position_units, 0.0);
    }

    #[test]
    fn paper_reversal_realizes_closed_slice_and_reopens_remainder() {
        let mut s = PaperState::default();
        // Short 10 at 100 (no slip), then buy 25 at 90: closes the 10
        // (pnl +100), leaves a 15 long opened at 90.
        PaperVenue::apply_fill(&mut s, -10.0, 100.0, 0.0);
        PaperVenue::apply_fill(&mut s, 25.0, 90.0, 0.0);
        assert!((s.realized_pnl - 100.0).abs() < 1e-9);
        assert!((s.position_units - 15.0).abs() < 1e-12);
        assert!((s.avg_entry - 90.0).abs() < 1e-12);
        // Sell the 15 long at 95: +75 more.
        PaperVenue::apply_fill(&mut s, -15.0, 95.0, 0.0);
        assert!((s.realized_pnl - 175.0).abs() < 1e-9);
        assert_eq!(s.position_units, 0.0);
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
        assert_eq!(specs[0], VenueSpec { name: "paper".into(), slippage_bps: 3.0, funding_rate_annual: 0.0 });
        assert_eq!(specs[1], VenueSpec { name: "paper-b".into(), slippage_bps: 7.0, funding_rate_annual: -0.2 });

        let bad: HedgeConfig =
            toml::from_str("[[venues]]\nkind = \"bluefin\"\n").unwrap();
        assert!(bad.venue_specs().is_err());
    }

    #[tokio::test]
    async fn paper_venue_persists_across_reload() {
        let path = std::env::temp_dir().join(format!(
            "mm-desk-paper-test-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let v = PaperVenue::load(path.clone(), 0.0, 0.0);
            let events = v
                .execute(HedgeCommand::Submit(HedgeOrder { id: 1, size_units: -42.0, spot: 100.0 }))
                .await
                .unwrap();
            assert!(matches!(events[0], HedgeEvent::Acknowledged(1)));
            assert!(
                matches!(events[1], HedgeEvent::Filled(Fill { order: 1, size_units, .. }) if (size_units - -42.0).abs() < 1e-12)
            );
            assert!((v.position_units().await.unwrap() - -42.0).abs() < 1e-12);
        }
        {
            let v = PaperVenue::load(path.clone(), 0.0, 0.0);
            assert!((v.position_units().await.unwrap() - -42.0).abs() < 1e-12);
            assert!((v.snapshot().await.avg_entry - 100.0).abs() < 1e-12);
        }
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn legacy_short_units_state_file_migrates_to_signed() {
        let path = std::env::temp_dir().join(format!(
            "mm-desk-paper-migrate-test-{}.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"{"short_units": 42.0, "avg_entry": 99.5, "realized_pnl": 7.0,
                "slippage_paid": 1.0, "traded_notional": 4200.0}"#,
        )
        .unwrap();
        let v = PaperVenue::load(path.clone(), 0.0, 0.0);
        // 42 short (legacy) = signed −42.
        assert!((v.position_units().await.unwrap() - -42.0).abs() < 1e-12);
        assert!((v.snapshot().await.avg_entry - 99.5).abs() < 1e-12);
        assert!((v.realized_pnl().await.unwrap() - 7.0).abs() < 1e-12);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn paper_rejects_zero_size_and_replace_cancels_then_fills() {
        let path = std::env::temp_dir().join(format!(
            "mm-desk-paper-events-test-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let v = PaperVenue::load(path.clone(), 0.0, 0.0);
        let events = v
            .execute(HedgeCommand::Submit(HedgeOrder { id: 7, size_units: 0.0, spot: 100.0 }))
            .await
            .unwrap();
        assert!(matches!(&events[0], HedgeEvent::Rejected { order: 7, .. }));
        let events = v
            .execute(HedgeCommand::Replace {
                old: 7,
                new: HedgeOrder { id: 8, size_units: 5.0, spot: 100.0 },
            })
            .await
            .unwrap();
        assert!(matches!(events[0], HedgeEvent::Cancelled(7)));
        assert!(matches!(events[1], HedgeEvent::Acknowledged(8)));
        assert!(matches!(events[2], HedgeEvent::Filled(_)));
        assert!((v.position_units().await.unwrap() - 5.0).abs() < 1e-12);
        let _ = std::fs::remove_file(&path);
    }
}
