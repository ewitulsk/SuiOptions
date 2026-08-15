//! Admissibility guards for permissionless RFQ specs.
//!
//! Until the any-strike overhaul the desk only ever saw strikes the scheduler
//! had rolled: a short, known list. It now quotes a **continuous surface** —
//! anyone can ask it to price any minute-aligned expiry at any strike — and
//! the model has no opinion about whether a request is reasonable, only about
//! what it is worth.
//!
//! Two things go wrong without a gate in front of the model:
//!
//! * **Adverse selection.** A counterparty can walk a one-tick ladder across
//!   the wing looking for where the surface misprices, and take only those.
//!   Every quote is individually defensible; the portfolio is not.
//! * **Denial of service.** Distinct specs are free to invent, and each one
//!   is a cache miss, an indexer query and a pricing pass. An unbounded set of
//!   them is a cheap way to saturate the desk and api-service.
//!
//! These are pure predicates so they can be unit-tested and tuned without a
//! chain. Each returns a distinct reason string: a decline the desk *chose* is
//! not the same operational event as a decline the model produced, and an
//! over-tight band should show up as a metric rather than as silence.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use protocol_types::bucket_spec::BucketSpec;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct GuardConfig {
    /// Refuse expiries further out than this. The board lists at most two
    /// month-ends, so anything much past that is a strike nobody is trading —
    /// and one the vol surface has no calibration for.
    pub max_expiry_days: f64,
    /// Refuse expiries closer than this. Gamma near expiry is violent and the
    /// hedge cannot keep up; the near-expiry size throttle in `price_trader_flow`
    /// assumes something already excluded the last few minutes.
    pub min_expiry_minutes: f64,
    /// Half-width of the quotable strike window in standard deviations:
    /// quote only while `|ln(K/S)| <= max_moneyness_z * sigma * sqrt(tau)`.
    /// This is the adverse-selection gate — deep wings are where a static
    /// surface is most wrong and least able to notice.
    pub max_moneyness_z: f64,
    /// Sigma used for the moneyness band when the model has no vol for the
    /// pair yet. Deliberately wide: a cold buffer should not silently narrow
    /// the book to nothing.
    pub fallback_sigma: f64,
    /// Max signed quotes per distinct spec per `rate_window_secs`.
    pub max_quotes_per_spec: u32,
    pub rate_window_secs: u64,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            max_expiry_days: 120.0,
            min_expiry_minutes: 30.0,
            max_moneyness_z: 3.0,
            fallback_sigma: 0.6,
            max_quotes_per_spec: 30,
            rate_window_secs: 60,
        }
    }
}

/// Why a spec was refused before pricing. Distinct variants so each gets its
/// own metric label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    NotCreatable,
    Expired,
    TooNearExpiry,
    TooFarExpiry,
    OutsideMoneynessBand,
    RateLimited,
}

impl Refusal {
    /// Stable label for `mm_bot_quote_failures_total`.
    pub fn label(self) -> &'static str {
        match self {
            Self::NotCreatable => "spec_not_creatable",
            Self::Expired => "spec_expired",
            Self::TooNearExpiry => "spec_too_near_expiry",
            Self::TooFarExpiry => "spec_too_far_expiry",
            Self::OutsideMoneynessBand => "spec_outside_band",
            Self::RateLimited => "spec_rate_limited",
        }
    }

    pub fn reason(self) -> String {
        match self {
            Self::NotCreatable => "spec cannot be created on chain".into(),
            Self::Expired => "spec has expired".into(),
            Self::TooNearExpiry => "too close to expiry".into(),
            Self::TooFarExpiry => "expiry beyond the quotable horizon".into(),
            Self::OutsideMoneynessBand => "strike outside the quotable moneyness band".into(),
            Self::RateLimited => "too many quotes for this spec".into(),
        }
    }
}

/// Time-and-strike admissibility. Pure; `spot` is in the same settlement-raw
/// per underlying-raw units as the spec's strike, and `sigma` is the model's
/// vol for the pair (annualized).
pub fn admissible(
    cfg: &GuardConfig,
    spec: &BucketSpec,
    spot: f64,
    sigma: f64,
    now_ms: u64,
) -> Result<(), Refusal> {
    if !spec.is_creatable() {
        return Err(Refusal::NotCreatable);
    }
    if spec.expiry_ms <= now_ms {
        return Err(Refusal::Expired);
    }
    let minutes = (spec.expiry_ms - now_ms) as f64 / 60_000.0;
    if minutes < cfg.min_expiry_minutes {
        return Err(Refusal::TooNearExpiry);
    }
    if minutes / (60.0 * 24.0) > cfg.max_expiry_days {
        return Err(Refusal::TooFarExpiry);
    }

    let strike = spec.strike_scaled();
    if !(strike > 0.0 && spot > 0.0 && strike.is_finite() && spot.is_finite()) {
        return Err(Refusal::OutsideMoneynessBand);
    }
    let sigma = if sigma.is_finite() && sigma > 0.0 {
        sigma
    } else {
        cfg.fallback_sigma
    };
    let tau = minutes / (60.0 * 24.0 * 365.0);
    let band = cfg.max_moneyness_z * sigma * tau.sqrt();
    if (strike / spot).ln().abs() > band {
        return Err(Refusal::OutsideMoneynessBand);
    }
    Ok(())
}

/// Per-spec sliding-window rate limiter. Shared across the RFQ and bulk-view
/// paths so a client cannot dodge it by alternating between them.
pub struct SpecRateLimiter {
    window: Duration,
    max: u32,
    seen: Mutex<HashMap<BucketSpec, Vec<Instant>>>,
}

impl SpecRateLimiter {
    pub fn new(cfg: &GuardConfig) -> Self {
        Self {
            window: Duration::from_secs(cfg.rate_window_secs.max(1)),
            max: cfg.max_quotes_per_spec,
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// Record an attempt; false if this spec is over its budget.
    pub fn allow(&self, spec: &BucketSpec) -> bool {
        self.allow_at(spec, Instant::now())
    }

    fn allow_at(&self, spec: &BucketSpec, now: Instant) -> bool {
        let mut seen = self.seen.lock();
        // Bound the table: drop every spec whose whole window has lapsed, not
        // just this one's. Without it a spec walker leaks an entry per strike.
        seen.retain(|_, hits| hits.iter().any(|t| now.duration_since(*t) < self.window));

        let hits = seen.entry(spec.clone()).or_default();
        hits.retain(|t| now.duration_since(*t) < self.window);
        if hits.len() as u32 >= self.max {
            return false;
        }
        hits.push(now);
        true
    }

    pub fn tracked_specs(&self) -> usize {
        self.seen.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minute-aligned, as every creatable expiry must be — an unaligned
    /// fixture would be refused by `NotCreatable` before any other check ran.
    const NOW: u64 = 1_699_999_980_000;
    const DAY_MS: u64 = 86_400_000;

    fn spec_at(strike: u64, expiry_ms: u64) -> BucketSpec {
        BucketSpec::new("0x9::a::A", "0x9::b::B", expiry_ms, strike as u128, 0, false).unwrap()
    }

    /// 30 days out, spot 100, sigma 0.6 → band = 3 * 0.6 * sqrt(30/365) ≈ 0.516,
    /// so strikes roughly within [59.6, 167.8] are quotable.
    fn thirty_day(strike: u64) -> BucketSpec {
        spec_at(strike, NOW + 30 * DAY_MS)
    }

    #[test]
    fn atm_is_admissible() {
        assert_eq!(
            admissible(&GuardConfig::default(), &thirty_day(100), 100.0, 0.6, NOW),
            Ok(())
        );
    }

    #[test]
    fn deep_wings_are_refused() {
        let cfg = GuardConfig::default();
        assert_eq!(
            admissible(&cfg, &thirty_day(300), 100.0, 0.6, NOW),
            Err(Refusal::OutsideMoneynessBand)
        );
        assert_eq!(
            admissible(&cfg, &thirty_day(20), 100.0, 0.6, NOW),
            Err(Refusal::OutsideMoneynessBand)
        );
    }

    /// The band scales with sqrt(tau): a strike quotable at 30 days is not
    /// necessarily quotable at 1 day, which is the point — a week-out 20%
    /// OTM strike is ordinary, a one-day 20% OTM strike is a lottery ticket.
    #[test]
    fn the_band_tightens_as_expiry_approaches() {
        let cfg = GuardConfig::default();
        let far = spec_at(140, NOW + 30 * DAY_MS);
        let near = spec_at(140, NOW + DAY_MS);
        assert_eq!(admissible(&cfg, &far, 100.0, 0.6, NOW), Ok(()));
        assert_eq!(
            admissible(&cfg, &near, 100.0, 0.6, NOW),
            Err(Refusal::OutsideMoneynessBand)
        );
    }

    #[test]
    fn expiry_horizon_is_bounded_both_ways() {
        let cfg = GuardConfig::default();
        assert_eq!(
            admissible(&cfg, &spec_at(100, NOW + 60_000), 100.0, 0.6, NOW),
            Err(Refusal::TooNearExpiry)
        );
        assert_eq!(
            admissible(&cfg, &spec_at(100, NOW + 400 * DAY_MS), 100.0, 0.6, NOW),
            Err(Refusal::TooFarExpiry)
        );
        assert_eq!(
            admissible(&cfg, &spec_at(100, NOW - DAY_MS), 100.0, 0.6, NOW),
            Err(Refusal::Expired)
        );
    }

    /// A cold vol buffer must not collapse the book — an absent sigma falls
    /// back to a deliberately wide one rather than to zero.
    #[test]
    fn a_cold_sigma_falls_back_instead_of_closing_the_book() {
        let cfg = GuardConfig::default();
        assert_eq!(admissible(&cfg, &thirty_day(105), 100.0, f64::NAN, NOW), Ok(()));
        assert_eq!(admissible(&cfg, &thirty_day(105), 100.0, 0.0, NOW), Ok(()));
    }

    #[test]
    fn unaligned_expiries_are_refused_before_anything_else() {
        let cfg = GuardConfig::default();
        let s = BucketSpec::new("0x9::a::A", "0x9::b::B", NOW + 30 * DAY_MS + 1, 100, 0, false)
            .unwrap();
        assert_eq!(admissible(&cfg, &s, 100.0, 0.6, NOW), Err(Refusal::NotCreatable));
    }

    #[test]
    fn rate_limiter_caps_a_single_spec_and_expires_its_window() {
        let cfg = GuardConfig {
            max_quotes_per_spec: 2,
            rate_window_secs: 60,
            ..GuardConfig::default()
        };
        let rl = SpecRateLimiter::new(&cfg);
        let s = thirty_day(100);
        let t0 = Instant::now();
        assert!(rl.allow_at(&s, t0));
        assert!(rl.allow_at(&s, t0));
        assert!(!rl.allow_at(&s, t0), "third within the window must be refused");
        // A different spec has its own budget.
        assert!(rl.allow_at(&thirty_day(105), t0));
        // Past the window the budget resets — and the lapsed entries are
        // dropped rather than accumulating one per strike walked.
        let later = t0 + Duration::from_secs(61);
        assert!(rl.allow_at(&s, later));
        assert_eq!(rl.tracked_specs(), 1, "lapsed specs must be evicted");
    }
}
