//! Delta hedging: the `HedgeVenue` seam, the `paper` venue (simulated
//! fills at oracle spot, real accounting persisted to disk), and the
//! band rebalancer (00-plan V1 §3 — bands not clocks).
//!
//! Real venues (DeepBook margin, Bluefin) are follow-ups behind the same
//! trait; nothing else in the desk knows which venue is wired.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A short-perp (or equivalent) venue hedging one underlying.
#[async_trait]
pub trait HedgeVenue: Send + Sync {
    fn name(&self) -> &str;
    /// Current SHORT position in underlying units (positive = short).
    async fn position_units(&self) -> Result<f64>;
    /// Adjust the short to `target_short_units` at (about) `spot`.
    async fn adjust_to(&self, target_short_units: f64, spot: f64) -> Result<()>;
    /// Annualized funding rate as seen by the short: positive = the short
    /// RECEIVES funding, negative = it pays.
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
#[derive(Clone, Debug, Deserialize)]
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
        }
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

/// The rebalance decision: rebalance to delta-neutral (target short =
/// book delta) only when the net-of-hedge delta leaves the band.
pub fn rebalance_target(
    book_delta_units: f64,
    hedge_short_units: f64,
    band_units: f64,
) -> Option<f64> {
    let net = book_delta_units - hedge_short_units;
    if net.abs() > band_units {
        Some(book_delta_units.max(0.0))
    } else {
        None
    }
}

// ── paper venue ────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PaperState {
    /// Current short size, underlying units.
    pub short_units: f64,
    /// Volume-weighted average entry price of the open short.
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
    path: PathBuf,
    slippage_bps: f64,
    funding_rate_annual: f64,
    state: tokio::sync::Mutex<PaperState>,
}

impl PaperVenue {
    pub fn load(path: PathBuf, slippage_bps: f64, funding_rate_annual: f64) -> Self {
        let state: PaperState = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
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

    /// Apply one fill to the state (pure w.r.t. the struct; extracted for
    /// unit tests). `delta_units > 0` increases the short.
    fn apply_fill(state: &mut PaperState, delta_units: f64, spot: f64, slippage_bps: f64) {
        if delta_units == 0.0 || spot <= 0.0 {
            return;
        }
        let slip = spot * slippage_bps / 10_000.0;
        // Increasing a short sells (worse = lower price); reducing buys
        // back (worse = higher price).
        let px = if delta_units > 0.0 { spot - slip } else { spot + slip };
        let notional = delta_units.abs() * spot;
        state.traded_notional += notional;
        state.slippage_paid += delta_units.abs() * slip;
        if delta_units > 0.0 {
            // Extend the short: new VWAP entry.
            let new_size = state.short_units + delta_units;
            state.avg_entry = if new_size > 0.0 {
                (state.avg_entry * state.short_units + px * delta_units) / new_size
            } else {
                0.0
            };
            state.short_units = new_size;
        } else {
            // Buy back: realize (entry − exit) × size on a short.
            let close = delta_units.abs().min(state.short_units);
            state.realized_pnl += (state.avg_entry - px) * close;
            state.short_units -= close;
            if state.short_units <= 1e-12 {
                state.short_units = 0.0;
                state.avg_entry = 0.0;
            }
        }
    }

    /// Current state snapshot (monitors / P&L attribution).
    pub async fn snapshot(&self) -> PaperState {
        self.state.lock().await.clone()
    }
}

#[async_trait]
impl HedgeVenue for PaperVenue {
    fn name(&self) -> &str {
        "paper"
    }

    async fn position_units(&self) -> Result<f64> {
        Ok(self.state.lock().await.short_units)
    }

    async fn adjust_to(&self, target_short_units: f64, spot: f64) -> Result<()> {
        let mut state = self.state.lock().await;
        let delta = target_short_units.max(0.0) - state.short_units;
        Self::apply_fill(&mut state, delta, spot, self.slippage_bps);
        self.persist(&state)?;
        tracing::info!(
            venue = "paper",
            target = target_short_units,
            short = state.short_units,
            realized = state.realized_pnl,
            "hedge adjusted"
        );
        Ok(())
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
    fn rebalance_only_outside_band() {
        // Inside the band: hold.
        assert_eq!(rebalance_target(100.0, 60.0, 50.0), None);
        // Outside: rebalance to neutral (short = book delta).
        assert_eq!(rebalance_target(100.0, 30.0, 50.0), Some(100.0));
        // Over-hedged beyond the band: buy back down to neutral.
        assert_eq!(rebalance_target(10.0, 200.0, 50.0), Some(10.0));
        // Negative book delta targets a flat short, never a long.
        assert_eq!(rebalance_target(-80.0, 0.0, 50.0), Some(0.0));
    }

    #[test]
    fn paper_fill_accounting_round_trips() {
        let mut s = PaperState::default();
        // Open 10 short at spot 100, 10bps slip → entry 99.9.
        PaperVenue::apply_fill(&mut s, 10.0, 100.0, 10.0);
        assert!((s.short_units - 10.0).abs() < 1e-12);
        assert!((s.avg_entry - 99.9).abs() < 1e-9);
        // Extend 10 more at 110 → entry VWAP (99.9 + 109.89)/2.
        PaperVenue::apply_fill(&mut s, 10.0, 110.0, 10.0);
        assert!((s.avg_entry - (99.9 + 109.89) / 2.0).abs() < 1e-9);
        // Buy the whole 20 back at 90 (pays 90.09): pnl = (entry − exit)×20.
        PaperVenue::apply_fill(&mut s, -20.0, 90.0, 10.0);
        let expected = ((99.9 + 109.89) / 2.0 - 90.09) * 20.0;
        assert!((s.realized_pnl - expected).abs() < 1e-6, "{}", s.realized_pnl);
        assert_eq!(s.short_units, 0.0);
        assert_eq!(s.avg_entry, 0.0);
        // Slippage: 10×0.1 + 10×0.11 + 20×0.09 = 3.9.
        assert!((s.slippage_paid - 3.9).abs() < 1e-9);
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
            v.adjust_to(42.0, 100.0).await.unwrap();
            assert!((v.position_units().await.unwrap() - 42.0).abs() < 1e-12);
        }
        {
            let v = PaperVenue::load(path.clone(), 0.0, 0.0);
            assert!((v.position_units().await.unwrap() - 42.0).abs() < 1e-12);
            assert!((v.snapshot().await.avg_entry - 100.0).abs() < 1e-12);
        }
        let _ = std::fs::remove_file(&path);
    }
}
