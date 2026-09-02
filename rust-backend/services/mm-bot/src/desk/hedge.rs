//! Hedge venues: the `HedgeVenue` seam and the `paper` venue (simulated
//! fills at oracle spot, real accounting persisted to disk). The signed
//! hedge POLICY — order/event vocabulary, `[desk.hedge]` config, band
//! math, `plan_hedge_order`, `OpenOrders` — is `desk_core::hedge`,
//! re-exported here (SO-450).
//!
//! The venue interface is order/event oriented — commands in,
//! acknowledgement/fill/reject events out — so live venues (Bluefin) and
//! the backtester's simulated venues share one seam. The paper venue
//! resolves every order synchronously (ack + full fill in the returned
//! events); a live venue returns what it has and delivers the rest
//! through its event stream.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub use desk_core::hedge::*;

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
    /// Cumulative realized P&L on the venue (settlement raw units), fills
    /// only. Feeds the scalp attribution line; venues without statements
    /// report 0.
    async fn realized_pnl(&self) -> Result<f64> {
        Ok(0.0)
    }
    /// Accrue funding on the current SIGNED position up to `now_ms` at
    /// `mark` and return the cash newly accrued (positive = PAID, i.e. a
    /// long under positive funding). Live venues settle on their own
    /// schedule and report the settled amount here; the paper venue
    /// accrues continuously (SO-438, doc 08 §4.2/§7.4).
    async fn accrue_funding(&self, _now_ms: u64, _mark: f64) -> Result<f64> {
        Ok(0.0)
    }
    /// Cumulative funding paid (positive) on the venue.
    async fn funding_paid(&self) -> Result<f64> {
        Ok(0.0)
    }
    /// Drain asynchronous outcomes (partial fills, late fills, cancels)
    /// that arrived since the last call — the event half of the seam
    /// (SO-438). The paper venue resolves its working remainders here.
    async fn poll_events(&self) -> Result<Vec<HedgeEvent>> {
        Ok(Vec::new())
    }
}

/// Continuous funding accrual for a paper position (pure, SO-438):
/// `rate × position × mark × Δt`, positive = paid (a long under positive
/// funding pays; a short receives). The first call only stamps the clock.
pub fn accrue_funding_step(
    state: &mut PaperState,
    now_ms: u64,
    mark: f64,
    funding_rate_annual: f64,
) -> f64 {
    const MS_PER_YEAR: f64 = 365.0 * 86_400.0 * 1000.0;
    if state.last_funding_ms == 0 || now_ms <= state.last_funding_ms || mark <= 0.0 {
        if now_ms > state.last_funding_ms {
            state.last_funding_ms = now_ms;
        }
        return 0.0;
    }
    let dt_years = (now_ms - state.last_funding_ms) as f64 / MS_PER_YEAR;
    state.last_funding_ms = now_ms;
    let paid = funding_rate_annual * state.position_units * mark * dt_years;
    state.funding_paid += paid;
    paid
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
    /// Cumulative funding PAID (positive; a receiving short drives it
    /// negative). Kept separate from `realized_pnl` (fills only) so the
    /// scalp and funding P&L lines never double count (SO-438).
    #[serde(default)]
    pub funding_paid: f64,
    /// Clock of the last funding accrual, ms since epoch (0 = never).
    #[serde(default)]
    pub last_funding_ms: u64,
}

/// Simulated perp venue: fills at oracle spot ± slippage, accounting is
/// real and persisted to a JSON state file so restarts don't reset the
/// position.
pub struct PaperVenue {
    name: String,
    path: PathBuf,
    slippage_bps: f64,
    funding_rate_annual: f64,
    /// Fraction of each order filled synchronously (1.0 = instant full
    /// fill). The remainder rests in `working` until `poll_events`.
    fill_fraction: f64,
    state: tokio::sync::Mutex<PaperState>,
    working: tokio::sync::Mutex<Vec<HedgeOrder>>,
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
            fill_fraction: 1.0,
            state: tokio::sync::Mutex::new(state),
            working: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    /// Fill only `fraction` of each order synchronously; the remainder
    /// fills on the next `poll_events` unless cancelled first.
    pub fn with_fill_fraction(mut self, fraction: f64) -> Self {
        self.fill_fraction = fraction.clamp(0.0, 1.0);
        self
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
                let now_units = order.size_units * self.fill_fraction;
                let mut events = vec![HedgeEvent::Acknowledged(order.id)];
                if now_units != 0.0 {
                    let mut state = self.state.lock().await;
                    let px = Self::apply_fill(&mut state, now_units, order.spot, self.slippage_bps)
                        .expect("nonzero order fills on the paper venue");
                    self.persist(&state)?;
                    tracing::info!(
                        venue = %self.name,
                        order = order.id,
                        size = now_units,
                        position = state.position_units,
                        realized = state.realized_pnl,
                        "hedge order filled (paper)"
                    );
                    let fill = Fill { order: order.id, size_units: now_units, price: px };
                    if self.fill_fraction >= 1.0 {
                        events.push(HedgeEvent::Filled(fill));
                        return Ok(events);
                    }
                    events.push(HedgeEvent::PartiallyFilled(fill));
                }
                // The remainder rests until `poll_events` or a cancel.
                self.working.lock().await.push(HedgeOrder {
                    id: order.id,
                    size_units: order.size_units - now_units,
                    spot: order.spot,
                });
                Ok(events)
            }
            HedgeCommand::Cancel(id) => {
                // Drop the resting remainder if there is one; a cancel for
                // an order that already fully filled is still acknowledged
                // as cancelled (the fill events were already delivered).
                self.working.lock().await.retain(|w| w.id != id);
                Ok(vec![HedgeEvent::Cancelled(id)])
            }
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

    async fn accrue_funding(&self, now_ms: u64, mark: f64) -> Result<f64> {
        let mut state = self.state.lock().await;
        let paid = accrue_funding_step(&mut state, now_ms, mark, self.funding_rate_annual);
        if paid != 0.0 {
            self.persist(&state)?;
        }
        Ok(paid)
    }

    async fn funding_paid(&self) -> Result<f64> {
        Ok(self.state.lock().await.funding_paid)
    }

    /// Resting remainders fill in full at their reference spot.
    async fn poll_events(&self) -> Result<Vec<HedgeEvent>> {
        let resting: Vec<HedgeOrder> = std::mem::take(&mut *self.working.lock().await);
        if resting.is_empty() {
            return Ok(Vec::new());
        }
        let mut state = self.state.lock().await;
        let mut events = Vec::with_capacity(resting.len());
        for w in resting {
            if let Some(px) = Self::apply_fill(&mut state, w.size_units, w.spot, self.slippage_bps) {
                events.push(HedgeEvent::Filled(Fill { order: w.id, size_units: w.size_units, price: px }));
            }
        }
        self.persist(&state)?;
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn funding_accrues_against_signed_position() {
        let mut s = PaperState { position_units: 100.0, ..Default::default() };
        // First call stamps the clock only.
        assert_eq!(accrue_funding_step(&mut s, 1_000, 10.0, 0.10), 0.0);
        // One year later at +10%/yr: a LONG pays 0.10 × 100 × 10 = 100.
        let year = 365 * 86_400 * 1000;
        let paid = accrue_funding_step(&mut s, 1_000 + year, 10.0, 0.10);
        assert!((paid - 100.0).abs() < 1e-9, "{paid}");
        assert!((s.funding_paid - 100.0).abs() < 1e-9);
        // A SHORT receives under the same rate.
        s.position_units = -100.0;
        let paid = accrue_funding_step(&mut s, 1_000 + 2 * year, 10.0, 0.10);
        assert!((paid + 100.0).abs() < 1e-9, "{paid}");
        // Negative funding flips both.
        s.position_units = 100.0;
        let paid = accrue_funding_step(&mut s, 1_000 + 3 * year, 10.0, -0.10);
        assert!((paid + 100.0).abs() < 1e-9, "{paid}");
        // Time never runs backwards.
        assert_eq!(accrue_funding_step(&mut s, 5, 10.0, 0.10), 0.0);
    }

    #[tokio::test]
    async fn paper_venue_partial_fill_then_poll_or_cancel() {
        let path = std::env::temp_dir().join(format!("so438-partial-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let v = PaperVenue::load(path.clone(), 0.0, 0.0).with_fill_fraction(0.25);
        let ev = v
            .execute(HedgeCommand::Submit(HedgeOrder { id: 1, size_units: -100.0, spot: 10.0 }))
            .await
            .unwrap();
        assert!(matches!(ev[1], HedgeEvent::PartiallyFilled(Fill { size_units, .. }) if size_units == -25.0));
        assert_eq!(v.position_units().await.unwrap(), -25.0);
        // The remainder fills on poll.
        let late = v.poll_events().await.unwrap();
        assert!(matches!(late[0], HedgeEvent::Filled(Fill { order: 1, size_units, .. }) if size_units == -75.0));
        assert_eq!(v.position_units().await.unwrap(), -100.0);
        assert!(v.poll_events().await.unwrap().is_empty());
        // A cancelled remainder never fills.
        v.execute(HedgeCommand::Submit(HedgeOrder { id: 2, size_units: 40.0, spot: 10.0 })).await.unwrap();
        v.execute(HedgeCommand::Cancel(2)).await.unwrap();
        assert!(v.poll_events().await.unwrap().is_empty());
        assert_eq!(v.position_units().await.unwrap(), -90.0);
        // Funding accrues on the signed position and persists.
        let v = PaperVenue::load(path.clone(), 0.0, 0.10);
        v.accrue_funding(1_000, 10.0).await.unwrap();
        let paid = v.accrue_funding(1_000 + 365 * 86_400 * 1000, 10.0).await.unwrap();
        assert!((paid + 90.0).abs() < 1e-9, "short receives: {paid}");
        assert!((v.funding_paid().await.unwrap() + 90.0).abs() < 1e-9);
        assert_eq!(v.realized_pnl().await.unwrap(), 0.0, "funding never leaks into fill P&L");
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
