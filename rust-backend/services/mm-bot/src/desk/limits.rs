//! Continuous-utilization limits engine (00-plan V1 §4/§6).
//!
//! Pure: takes a [`BookExposure`] snapshot plus an optional proposed fill
//! and returns either a [`Utilization`] snapshot (feeding
//! `vega_utilization` into the bid context — widen, never stop) or a
//! hard-decline reason. The kill switch persists a NAV history file so a
//! −10%-in-7d drawdown survives restarts.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// `[desk.limits]` — defaults are the 00-plan V1 starting parameters.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default)]
pub struct LimitsConfig {
    /// Soft premium budget, fraction of NAV (inventory penalty ramps
    /// against this). 00-plan: 30%.
    pub premium_budget_soft: f64,
    /// Hard premium budget, fraction of NAV. 00-plan: 35%.
    pub premium_budget_hard: f64,
    /// Net vega cap: fraction of NAV per vol point. 00-plan: 0.5%.
    pub vega_cap_nav_per_volpt: f64,
    /// Theta governor soft throttle, NAV fraction per day. 00-plan: 10bps.
    pub theta_soft_nav_per_day: f64,
    /// Theta governor hard cap, NAV fraction per day. 00-plan: 15bps.
    pub theta_hard_nav_per_day: f64,
    /// Max premium per expiry, fraction of NAV. 00-plan: 30%.
    pub per_expiry_max: f64,
    /// Max premium per strike-moneyness bucket, fraction of NAV.
    /// 00-plan: 15% (buckets <90 / 90–110 / >110%).
    pub per_strike_bucket_max: f64,
    /// Kill switch: stop new buys if NAV drops this fraction within
    /// `kill_window_days`. 00-plan: 10% in 7d.
    pub kill_drawdown: f64,
    pub kill_window_days: f64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            premium_budget_soft: 0.30,
            premium_budget_hard: 0.35,
            vega_cap_nav_per_volpt: 0.005,
            theta_soft_nav_per_day: 0.0010,
            theta_hard_nav_per_day: 0.0015,
            per_expiry_max: 0.30,
            per_strike_bucket_max: 0.15,
            kill_drawdown: 0.10,
            kill_window_days: 7.0,
        }
    }
}

/// Moneyness bucket index for the concentration cap: 0 = strike <90% of
/// spot, 1 = 90–110%, 2 = >110%.
pub fn strike_bucket(strike: f64, spot: f64) -> usize {
    if spot <= 0.0 {
        return 1;
    }
    let m = strike / spot;
    if m < 0.90 {
        0
    } else if m <= 1.10 {
        1
    } else {
        2
    }
}

/// Current book exposure, all in settlement raw units except where noted.
#[derive(Clone, Debug, Default)]
pub struct BookExposure {
    pub nav: f64,
    /// Mark-to-model premium in held options.
    pub premium_deployed: f64,
    /// Live quote reservations.
    pub reserved: f64,
    /// Net book vega per vol point (settlement raw per vol pt; signed —
    /// long-vol positive).
    pub net_vega_per_volpt: f64,
    /// Net theta bleed per day (positive = the book pays theta).
    pub theta_cost_per_day: f64,
    /// Premium per expiry (ms).
    pub premium_by_expiry: HashMap<u64, f64>,
    /// Premium per strike-moneyness bucket (see [`strike_bucket`]).
    pub premium_by_strike_bucket: [f64; 3],
    /// Kill switch already latched (from [`KillSwitch::check`]).
    pub kill_switch: bool,
}

/// The proposed new buy being tested against the caps.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProposedFill {
    /// Premium the fill would deploy, settlement raw.
    pub premium: f64,
    /// Vega the fill would add, settlement raw per vol pt.
    pub vega_per_volpt: f64,
    /// Theta cost the fill would add, per day.
    pub theta_cost_per_day: f64,
    pub expiry_ms: u64,
    pub strike_bucket: usize,
}

/// Continuous utilizations in [0, ∞) — 1.0 = at the (hard) limit. Feeds
/// the inventory penalty (`vega_utilization` in the bid context).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Utilization {
    pub premium: f64,
    pub vega: f64,
    pub theta: f64,
}

/// Hard-decline reasons — the only cases where the desk refuses to quote
/// the buy side (everything else degrades via the vol discount).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HardDecline {
    PremiumBudget,
    VegaCap,
    ThetaGovernor,
    ExpiryConcentration,
    StrikeConcentration,
    KillSwitch,
}

impl HardDecline {
    pub fn as_str(self) -> &'static str {
        match self {
            HardDecline::PremiumBudget => "premium budget hard cap",
            HardDecline::VegaCap => "net vega cap",
            HardDecline::ThetaGovernor => "theta governor hard cap",
            HardDecline::ExpiryConcentration => "per-expiry concentration cap",
            HardDecline::StrikeConcentration => "per-strike-bucket concentration cap",
            HardDecline::KillSwitch => "kill switch (NAV drawdown)",
        }
    }
}

/// Evaluate the caps for a proposed buy. `Ok(Utilization)` reflects the
/// book INCLUDING the fill (so the bid path prices post-fill inventory).
pub fn evaluate(
    cfg: &LimitsConfig,
    x: &BookExposure,
    fill: &ProposedFill,
) -> Result<Utilization, HardDecline> {
    if x.kill_switch {
        return Err(HardDecline::KillSwitch);
    }
    if x.nav <= 0.0 {
        return Err(HardDecline::PremiumBudget);
    }
    let premium_after = x.premium_deployed + x.reserved + fill.premium;
    if premium_after > cfg.premium_budget_hard * x.nav {
        return Err(HardDecline::PremiumBudget);
    }
    let vega_after = x.net_vega_per_volpt + fill.vega_per_volpt;
    let vega_cap = cfg.vega_cap_nav_per_volpt * x.nav;
    if vega_after.abs() > vega_cap {
        return Err(HardDecline::VegaCap);
    }
    let theta_after = x.theta_cost_per_day + fill.theta_cost_per_day;
    if theta_after > cfg.theta_hard_nav_per_day * x.nav {
        return Err(HardDecline::ThetaGovernor);
    }
    let expiry_after =
        x.premium_by_expiry.get(&fill.expiry_ms).copied().unwrap_or(0.0) + fill.premium;
    if expiry_after > cfg.per_expiry_max * x.nav {
        return Err(HardDecline::ExpiryConcentration);
    }
    let bucket = fill.strike_bucket.min(2);
    if x.premium_by_strike_bucket[bucket] + fill.premium > cfg.per_strike_bucket_max * x.nav {
        return Err(HardDecline::StrikeConcentration);
    }
    Ok(Utilization {
        premium: premium_after / (cfg.premium_budget_soft * x.nav).max(f64::MIN_POSITIVE),
        vega: vega_after.abs() / vega_cap.max(f64::MIN_POSITIVE),
        theta: (theta_after / (cfg.theta_soft_nav_per_day * x.nav).max(f64::MIN_POSITIVE))
            .max(0.0),
    })
}

// ── kill switch ────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Default)]
struct NavHistory {
    /// (unix ms, nav) samples, oldest first.
    samples: Vec<(u64, u64)>,
}

/// Persisted rolling-high-water kill switch: latched while NAV sits more
/// than `kill_drawdown` below the window's high water.
pub struct KillSwitch {
    path: PathBuf,
    history: NavHistory,
}

impl KillSwitch {
    pub fn load(path: PathBuf) -> Self {
        let history = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { path, history }
    }

    /// Record the NAV sample and return whether the switch is tripped.
    pub fn check(&mut self, cfg: &LimitsConfig, nav: u64, now_ms: u64) -> bool {
        let window_ms = (cfg.kill_window_days * 86_400_000.0) as u64;
        self.history.samples.push((now_ms, nav));
        self.history
            .samples
            .retain(|(t, _)| now_ms.saturating_sub(*t) <= window_ms);
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        if let Ok(json) = serde_json::to_string(&self.history) {
            if let Err(e) = std::fs::write(&self.path, json) {
                tracing::warn!(error = %e, path = %self.path.display(), "kill-switch persist failed");
            }
        }
        let high_water = self.history.samples.iter().map(|(_, n)| *n).max().unwrap_or(nav);
        (nav as f64) < (high_water as f64) * (1.0 - cfg.kill_drawdown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> LimitsConfig {
        LimitsConfig::default()
    }

    /// NAV 1e9; everything else empty.
    fn base() -> BookExposure {
        BookExposure { nav: 1e9, ..Default::default() }
    }

    fn fill() -> ProposedFill {
        ProposedFill {
            premium: 1e6,
            vega_per_volpt: 1e5,
            theta_cost_per_day: 1e4,
            expiry_ms: 42,
            strike_bucket: 1,
        }
    }

    #[test]
    fn happy_path_reports_utilizations() {
        let u = evaluate(&cfg(), &base(), &fill()).unwrap();
        // premium: 1e6 / (0.30 × 1e9) ≈ 0.0033
        assert!((u.premium - 1e6 / 3e8).abs() < 1e-9);
        // vega: 1e5 / (0.005 × 1e9) = 0.02
        assert!((u.vega - 0.02).abs() < 1e-9);
        // theta: 1e4 / (0.0010 × 1e9) = 0.01
        assert!((u.theta - 0.01).abs() < 1e-9);
    }

    #[test]
    fn premium_hard_cap_trips() {
        let mut x = base();
        x.premium_deployed = 0.34 * 1e9;
        x.reserved = 0.005 * 1e9;
        let mut f = fill();
        f.premium = 0.006 * 1e9; // 34% + 0.5% + 0.6% > 35%
        assert_eq!(evaluate(&cfg(), &x, &f), Err(HardDecline::PremiumBudget));
        // Reservations count against the budget.
        x.reserved = 0.0;
        assert!(evaluate(&cfg(), &x, &f).is_ok());
    }

    #[test]
    fn vega_cap_trips_on_absolute_value() {
        let mut x = base();
        x.net_vega_per_volpt = 0.0049 * 1e9;
        let mut f = fill();
        f.vega_per_volpt = 0.0002 * 1e9;
        assert_eq!(evaluate(&cfg(), &x, &f), Err(HardDecline::VegaCap));
        // Short vega breaches the same cap (V2 symmetry is handled by the
        // V2 band; V1's cap is |net|).
        x.net_vega_per_volpt = -0.0052 * 1e9;
        f.vega_per_volpt = 0.0;
        assert_eq!(evaluate(&cfg(), &x, &f), Err(HardDecline::VegaCap));
    }

    #[test]
    fn theta_governor_trips() {
        let mut x = base();
        x.theta_cost_per_day = 0.0014 * 1e9;
        let mut f = fill();
        f.theta_cost_per_day = 0.0002 * 1e9;
        assert_eq!(evaluate(&cfg(), &x, &f), Err(HardDecline::ThetaGovernor));
    }

    #[test]
    fn concentration_caps_trip() {
        let mut x = base();
        x.premium_by_expiry.insert(42, 0.299 * 1e9);
        let mut f = fill();
        f.premium = 0.002 * 1e9;
        assert_eq!(evaluate(&cfg(), &x, &f), Err(HardDecline::ExpiryConcentration));
        // A different expiry is fine.
        f.expiry_ms = 43;
        assert!(evaluate(&cfg(), &x, &f).is_ok());
        // Strike bucket.
        let mut x = base();
        x.premium_by_strike_bucket[2] = 0.149 * 1e9;
        f.expiry_ms = 42;
        f.strike_bucket = 2;
        assert_eq!(evaluate(&cfg(), &x, &f), Err(HardDecline::StrikeConcentration));
        f.strike_bucket = 0;
        assert!(evaluate(&cfg(), &x, &f).is_ok());
    }

    #[test]
    fn kill_switch_blocks_everything() {
        let mut x = base();
        x.kill_switch = true;
        assert_eq!(evaluate(&cfg(), &x, &fill()), Err(HardDecline::KillSwitch));
    }

    #[test]
    fn strike_buckets_split_at_90_and_110() {
        assert_eq!(strike_bucket(89.0, 100.0), 0);
        assert_eq!(strike_bucket(90.0, 100.0), 1);
        assert_eq!(strike_bucket(110.0, 100.0), 1);
        assert_eq!(strike_bucket(111.0, 100.0), 2);
    }

    #[test]
    fn kill_switch_trips_on_seven_day_drawdown_and_persists() {
        let path = std::env::temp_dir().join(format!(
            "mm-desk-kill-test-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let day = 86_400_000u64;
        let c = cfg();
        {
            let mut k = KillSwitch::load(path.clone());
            assert!(!k.check(&c, 1_000, day));
            assert!(!k.check(&c, 950, 2 * day)); // −5%: fine
            assert!(k.check(&c, 890, 3 * day)); // −11% from high water: trip
        }
        // Reload from disk: the high water survives the restart.
        {
            let mut k = KillSwitch::load(path.clone());
            assert!(k.check(&c, 890, 4 * day));
            // Once the 1_000 sample ages out of the window, 890 vs recent
            // high water 890 → no drawdown.
            assert!(!k.check(&c, 890, 9 * day));
        }
        let _ = std::fs::remove_file(&path);
    }
}
