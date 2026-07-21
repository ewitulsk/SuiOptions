//! Delta hedging: the `HedgeVenue` seam, the `paper` venue (simulated
//! fills at oracle spot, real accounting persisted to disk), and the
//! band rebalancer (00-plan V1 §3 — bands not clocks).
//!
//! Real venues (DeepBook margin, Bluefin) are follow-ups behind the same
//! trait; nothing else in the desk knows which venue is wired.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
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

/// One `[[desk.hedge.venues]]` entry. `kind = "paper"` (simulated) or
/// `kind = "deepbook_margin"` (the shared MarginManager, doc 04 §3c);
/// Bluefin is a follow-up behind the same seam.
#[derive(Clone, Debug, Deserialize)]
pub struct HedgeVenueToml {
    pub kind: String,
    /// Gauge/alert label + state-file key. Defaults to "paper" for the
    /// first entry, "paper{n}" after ("dbm"/"dbm{n}" for deepbook_margin).
    pub name: Option<String>,
    /// Defaults to `paper_slippage_bps`.
    pub slippage_bps: Option<f64>,
    /// Defaults to `paper_funding_rate_annual`. deepbook_margin venues
    /// ignore it — their carry is the live borrow APR.
    pub funding_rate_annual: Option<f64>,

    // ── deepbook_margin venue (all required for that kind; ignored by
    //    paper so legacy configs keep parsing) ──────────────────────────
    /// The shared `MarginManager` (owner = the 2-of-2 multisig).
    pub margin_manager_id: Option<String>,
    /// The manager's DeepBook spot pool (base/quote).
    pub deepbook_pool_id: Option<String>,
    pub base_margin_pool_id: Option<String>,
    pub quote_margin_pool_id: Option<String>,
    /// LATEST deepbook_margin package (call target).
    pub margin_package: Option<String>,
    pub margin_registry_id: Option<String>,
    pub base_type: Option<String>,
    pub quote_type: Option<String>,
    /// Pyth `PriceInfoObject`s for base/quote — DBM entry calls
    /// (borrow/deposit/order placement, risk ratio) take them by ref.
    pub base_price_info_id: Option<String>,
    pub quote_price_info_id: Option<String>,
    /// hedge-signer base URL + the 2-of-2 multisig address. Optional:
    /// without BOTH, the venue is read-only (position/funding/headroom)
    /// and `adjust_to`/`top_up` return a clear error.
    pub signer_url: Option<String>,
    pub multisig_address: Option<String>,
}

/// A resolved venue to instantiate (per underlying market).
#[derive(Clone, Debug, PartialEq)]
pub struct VenueSpec {
    pub name: String,
    pub slippage_bps: f64,
    pub funding_rate_annual: f64,
    /// `Some` for kind = "deepbook_margin" (parsed chain identity).
    pub dbm: Option<super::dbm::DbmVenueConfig>,
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
                dbm: None,
            }]);
        }
        let mut out = Vec::with_capacity(self.venues.len());
        for (i, v) in self.venues.iter().enumerate() {
            let (default_name, dbm) = match v.kind.as_str() {
                "paper" => (
                    if i == 0 { "paper".into() } else { format!("paper{}", i + 1) },
                    None,
                ),
                "deepbook_margin" => (
                    if i == 0 { "dbm".into() } else { format!("dbm{}", i + 1) },
                    Some(super::dbm::DbmVenueConfig::from_toml(v)?),
                ),
                other => bail!(
                    "[[desk.hedge.venues]] kind {other:?} not supported \
                     (only \"paper\" and \"deepbook_margin\")"
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
                dbm,
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
    pub fn load_named(
        name: impl Into<String>,
        path: PathBuf,
        slippage_bps: f64,
        funding_rate_annual: f64,
    ) -> Self {
        let state: PaperState = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
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
        &self.name
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
            venue = %self.name,
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
                dbm: None,
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
        assert_eq!(specs[0], VenueSpec { name: "paper".into(), slippage_bps: 3.0, funding_rate_annual: 0.0, dbm: None });
        assert_eq!(specs[1], VenueSpec { name: "paper-b".into(), slippage_bps: 7.0, funding_rate_annual: -0.2, dbm: None });

        let bad: HedgeConfig =
            toml::from_str("[[venues]]\nkind = \"bluefin\"\n").unwrap();
        assert!(bad.venue_specs().is_err());
    }

    #[test]
    fn deepbook_margin_venue_parses_alongside_paper() {
        let cfg: HedgeConfig = toml::from_str(
            "[[venues]]\n\
             kind = \"paper\"\n\
             [[venues]]\n\
             kind = \"deepbook_margin\"\n\
             margin_manager_id = \"0x11\"\n\
             deepbook_pool_id = \"0x12\"\n\
             base_margin_pool_id = \"0x13\"\n\
             quote_margin_pool_id = \"0x14\"\n\
             margin_package = \"0x15\"\n\
             margin_registry_id = \"0x16\"\n\
             base_type = \"0x2::sui::SUI\"\n\
             quote_type = \"0xa::dbusdc::DBUSDC\"\n\
             base_price_info_id = \"0x17\"\n\
             quote_price_info_id = \"0x18\"\n\
             signer_url = \"http://hedge-signer:9010\"\n\
             multisig_address = \"0x00000000000000000000000000000000000000000000000000000000000000ee\"\n",
        )
        .unwrap();
        let specs = cfg.venue_specs().unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "paper");
        assert!(specs[0].dbm.is_none());
        assert_eq!(specs[1].name, "dbm2");
        let dbm = specs[1].dbm.as_ref().unwrap();
        assert_eq!(dbm.margin_manager_id.to_hex_literal(), "0x11");
        assert_eq!(dbm.margin_package.to_hex_literal(), "0x15");
        // Types come out canonicalized.
        assert!(dbm.base_type.ends_with("::sui::SUI"));
        assert_eq!(dbm.signer_url.as_deref(), Some("http://hedge-signer:9010"));
        assert!(dbm.multisig_address.is_some());

        // A deepbook_margin entry missing chain ids is a config error.
        let bad: HedgeConfig = toml::from_str(
            "[[venues]]\nkind = \"deepbook_margin\"\nmargin_manager_id = \"0x11\"\n",
        )
        .unwrap();
        assert!(bad.venue_specs().is_err());

        // Without signer_url/multisig the venue still parses (read-only).
        let ro: HedgeConfig = toml::from_str(
            "[[venues]]\n\
             kind = \"deepbook_margin\"\n\
             margin_manager_id = \"0x11\"\n\
             deepbook_pool_id = \"0x12\"\n\
             base_margin_pool_id = \"0x13\"\n\
             quote_margin_pool_id = \"0x14\"\n\
             margin_package = \"0x15\"\n\
             margin_registry_id = \"0x16\"\n\
             base_type = \"0x2::sui::SUI\"\n\
             quote_type = \"0xa::dbusdc::DBUSDC\"\n\
             base_price_info_id = \"0x17\"\n\
             quote_price_info_id = \"0x18\"\n",
        )
        .unwrap();
        let specs = ro.venue_specs().unwrap();
        assert_eq!(specs[0].name, "dbm");
        let dbm = specs[0].dbm.as_ref().unwrap();
        assert!(dbm.signer_url.is_none() && dbm.multisig_address.is_none());
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
