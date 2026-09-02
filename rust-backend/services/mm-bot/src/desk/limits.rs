//! Continuous-utilization limits engine (00-plan V1 §4/§6) and the
//! vault-scaled capital policy (doc 08 §0.4 / §4.6, SO-444).
//!
//! Pure: takes a [`BookExposure`] snapshot plus a proposed fill and
//! returns either a [`Utilization`] snapshot (feeding `vega_utilization`
//! into the bid context — widen, never stop) or a hard-decline reason.
//! Every dollar cap scales from the [`CapitalSnapshot`]'s conservative
//! `risk_nav` (never from `latest_pps × shares`) and the call / put /
//! per-expiry caps are the LESSER of their NAV budget and the measured
//! venue capacity ([`CapitalPolicy`]). A stale snapshot declines new
//! risk. The kill switch persists a NAV history file so a −10%-in-7d
//! drawdown survives restarts.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// `[desk.limits]` — defaults are the 00-plan starting parameters
/// (retrofit to the long-only strategy, doc 08 §0.4/§4.5: soft 25 /
/// hard 30 total, 20% per side, 10% per expiry and strike bucket).
/// `Serialize` so `/desk/state` can echo the effective limits (SO-348).
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct LimitsConfig {
    /// Soft premium budget, fraction of NAV (inventory penalty ramps
    /// against this). 00-plan: 25%.
    pub premium_budget_soft: f64,
    /// Hard premium budget, fraction of NAV. 00-plan: 30%.
    pub premium_budget_hard: f64,
    /// Max premium in CALLS, fraction of NAV (doc 08 §0.4: 20%).
    pub call_premium_max: f64,
    /// Max premium in PUTS, fraction of NAV (doc 08 §0.4: 20%).
    pub put_premium_max: f64,
    /// Net vega cap: fraction of NAV per vol point. 00-plan: 0.5%.
    pub vega_cap_nav_per_volpt: f64,
    /// Theta governor soft throttle, NAV fraction per day. 00-plan: 10bps.
    pub theta_soft_nav_per_day: f64,
    /// Theta governor hard cap, NAV fraction per day. 00-plan: 15bps.
    pub theta_hard_nav_per_day: f64,
    /// Max premium per expiry (calls + puts), fraction of NAV.
    /// 00-plan retrofit: 10%.
    pub per_expiry_max: f64,
    /// Max premium per strike-moneyness bucket, fraction of NAV.
    /// 00-plan retrofit: 10% (buckets <90 / 90–110 / >110%).
    pub per_strike_bucket_max: f64,
    /// Kill switch: stop new buys if NAV drops this fraction within
    /// `kill_window_days`. 00-plan: 10% in 7d.
    pub kill_drawdown: f64,
    pub kill_window_days: f64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            premium_budget_soft: 0.25,
            premium_budget_hard: 0.30,
            call_premium_max: 0.20,
            put_premium_max: 0.20,
            vega_cap_nav_per_volpt: 0.005,
            theta_soft_nav_per_day: 0.0010,
            theta_hard_nav_per_day: 0.0015,
            per_expiry_max: 0.10,
            per_strike_bucket_max: 0.10,
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

// ── capital snapshot + policy (doc 08 §0.4 / §4.6, SO-444) ─────────────

/// `[desk.capital]` — freshness gates, the liquidity reserve, and the
/// venue/flash capacity assumptions that stand in until pollers exist.
/// Policy stays in ratios; the flash / spot figures are MEASUREMENTS
/// (settlement raw units) and are labeled as assumptions in the
/// snapshot. `Serialize` so `/desk/state` can echo the effective config.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct CapitalConfig {
    /// Appraised NAV older than this ⇒ new quotes decline.
    pub appraisal_max_age_secs: u64,
    /// External-account equity older than this (while exposure is live)
    /// ⇒ new quotes decline.
    pub external_equity_max_age_secs: u64,
    /// Snapshot older than this at quote time (refresher stalled) ⇒
    /// new quotes decline.
    pub snapshot_max_age_secs: u64,
    /// A signed quote's reservation outlives its TTL by this much so a
    /// late-detected fill never finds its capacity already re-lent.
    pub reservation_grace_secs: u64,
    /// Required liquidity reserve, fraction of NAV (00-plan: 10%, floor
    /// 5%). Subtracted from `risk_nav` and withheld from free quote cash.
    pub liquidity_reserve_frac: f64,
    /// Max required margin top-up in 24h, fraction of risk NAV (doc 08
    /// §0.4: 10%); the on-chain daily release remaining also bounds it.
    pub margin_topup_max: f64,
    /// Venue margin capacity comes from the vault's on-chain external
    /// budget / daily release. `false` (paper venue, no margin posted)
    /// leaves it unbounded — labeled in the snapshot.
    pub venue_margin_from_chain: bool,
    /// Maintenance margin fraction of hedge notional at the venue.
    pub maintenance_margin_fraction: f64,
    /// Flash-loan capacity in UNDERLYING (base) — settlement value of it
    /// — and in SETTLEMENT (quote). Configured assumptions until an
    /// on-chain pool-balance poller exists (doc 08 §4.6, decision
    /// 2026-08-22).
    pub base_flash_capacity: f64,
    pub quote_flash_capacity: f64,
    /// Spot BUY (settlement → underlying) and SELL capacity by slippage
    /// tier, settlement value. From config until a router poller exists.
    pub spot_buy_capacity_by_slippage: Vec<SlippageTier>,
    pub spot_sell_capacity_by_slippage: Vec<SlippageTier>,
    /// Reference ATM ratios for the HEADLINE effective capacities on
    /// `/desk/state` (a real fill is evaluated on its own delta and
    /// strike): hedge notional per unit of premium (≈ 0.5·S / 0.08·S)
    /// and exercise cash per unit of premium (≈ K / 0.08·S).
    pub reference_hedge_notional_per_premium: f64,
    pub reference_exercise_cash_per_premium: f64,
}

impl Default for CapitalConfig {
    fn default() -> Self {
        Self {
            appraisal_max_age_secs: 3_600,
            external_equity_max_age_secs: 900,
            snapshot_max_age_secs: 300,
            reservation_grace_secs: 300,
            liquidity_reserve_frac: 0.10,
            margin_topup_max: 0.10,
            venue_margin_from_chain: false,
            maintenance_margin_fraction: 0.05,
            base_flash_capacity: 0.0,
            quote_flash_capacity: 0.0,
            spot_buy_capacity_by_slippage: Vec::new(),
            spot_sell_capacity_by_slippage: Vec::new(),
            reference_hedge_notional_per_premium: 6.0,
            reference_exercise_cash_per_premium: 12.0,
        }
    }
}

/// Spot capacity available within a slippage tier, settlement value.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SlippageTier {
    pub slippage_bps: f64,
    pub capacity: f64,
}

/// The freshness thresholds a snapshot was built under, carried inside
/// it so quote-time re-validation needs no config.
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Freshness {
    pub appraisal_max_age_ms: u64,
    pub external_equity_max_age_ms: u64,
    pub snapshot_max_age_ms: u64,
}

/// Why a snapshot cannot back new risk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stale {
    NoSnapshot,
    SnapshotAge,
    Appraisal,
    ExternalEquity,
    QueuedWithdrawals,
}

impl Stale {
    pub fn as_str(self) -> &'static str {
        match self {
            Stale::NoSnapshot => "no capital snapshot yet",
            Stale::SnapshotAge => "capital snapshot stale (refresher stalled)",
            Stale::Appraisal => "appraised NAV missing or stale",
            Stale::ExternalEquity => "external hedge equity missing or stale",
            Stale::QueuedWithdrawals => "queued withdrawals cannot be valued",
        }
    }
}

/// Live quote reservations aggregated the way the policy numerators
/// need them (built by `Book::reserved_split`). Every reservation lands
/// ONCE in each applicable numerator — total, its side, its expiry —
/// so call, put, total and per-expiry caps see the same cash.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReservedSplit {
    pub total: f64,
    pub calls: f64,
    pub puts: f64,
    pub by_expiry: HashMap<u64, f64>,
    /// Strike cash the reserved CALLS would need at exercise.
    pub call_strike_cash: f64,
    /// Underlying value the reserved PUTS would need to deliver.
    pub put_underlying_value: f64,
    pub exercise_demand_by_expiry: HashMap<u64, f64>,
    pub hedge_notional: f64,
    pub hedge_notional_by_expiry: HashMap<u64, f64>,
}

/// The fresh, auditable capital picture one quote is sized against
/// (doc 08 §4.6). Settlement raw units unless noted. Built by
/// [`build_capital_snapshot`] each refresher tick, re-validated for
/// freshness at quote time ([`CapitalSnapshot::risk_nav_at`]).
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapitalSnapshot {
    /// Latest appraised NAV (risk-bearing measure: junior on a tranched
    /// vault — `book::budget_base`).
    pub appraised_nav: Option<f64>,
    /// free settlement + free underlying + marked options + external
    /// equity, from what the desk can read itself.
    pub locally_reconstructed_nav: Option<f64>,
    /// `min(appraised, local) − queued withdrawals − unresolved debits −
    /// liquidity reserve`, at `observed_at`. `None` while stale.
    pub risk_nav: Option<f64>,
    pub free_settlement: f64,
    /// Symbol → settlement value of the vault's free underlying balance.
    pub free_underlying_by_asset: HashMap<String, f64>,
    /// `None` when a queued withdrawal cannot be valued (missing pps).
    pub queued_withdrawal_value: Option<f64>,
    /// In-flight fill / PTB worst-case debits. Not tracked yet (no
    /// in-flight PTB ledger) — always 0, labeled.
    pub unresolved_debits: f64,
    pub liquidity_reserve: f64,
    pub call_premium_marked: f64,
    pub put_premium_marked: f64,
    pub call_quote_reservations: f64,
    pub put_quote_reservations: f64,
    /// Every live reservation incl. legacy/auction keys.
    pub total_quote_reservations: f64,
    /// Marked + reserved premium per expiry.
    pub premium_by_expiry: HashMap<u64, f64>,
    /// Strike cash the held + reserved calls need at exercise.
    pub call_strike_cash_required: f64,
    /// Underlying value the held + reserved puts need to deliver.
    pub put_underlying_value_required: f64,
    /// Exercise demand (strike cash + underlying value) per expiry.
    pub exercise_demand_by_expiry: HashMap<u64, f64>,
    /// |delta|·spot of held + reserved options (the hedge the book needs).
    pub hedge_notional: f64,
    pub hedge_notional_by_expiry: HashMap<u64, f64>,
    pub external_exposure: f64,
    pub external_equity: Option<f64>,
    /// On-chain remaining values (`vault::external_limits`), never the
    /// configured venue margin.
    pub external_budget_remaining: f64,
    pub external_daily_release_remaining: f64,
    /// Margin currently posted at the venues (|position|·spot × initial
    /// fraction), the maintenance requirement, and the min headroom.
    pub venue_initial_margin: f64,
    pub venue_maintenance_margin: f64,
    pub venue_margin_headroom: f64,
    /// Paper venue: no margin is posted, so margin capacity is unbounded.
    pub venue_margin_unbounded: bool,
    pub initial_margin_fraction: f64,
    /// Adverse gap the top-up capacity is sized for (monitors' stress).
    pub stress_gap: f64,
    /// `min(margin_topup_max × risk_nav, daily release remaining)`.
    pub margin_topup_cap: f64,
    pub base_flash_capacity: f64,
    pub quote_flash_capacity: f64,
    pub spot_buy_capacity_by_slippage: Vec<SlippageTier>,
    pub spot_sell_capacity_by_slippage: Vec<SlippageTier>,
    pub observed_at: u64,
    pub appraisal_at: Option<u64>,
    pub external_equity_at: Option<u64>,
    pub freshness: Freshness,
    /// Every input that is an assumption rather than a measurement.
    pub assumptions: Vec<String>,
    /// Why `risk_nav` is `None` at build time.
    pub stale: Vec<Stale>,
}

impl CapitalSnapshot {
    /// The risk NAV this snapshot backs at `now_ms`, or why it cannot.
    pub fn risk_nav_at(&self, now_ms: u64) -> Result<f64, Stale> {
        if self.observed_at == 0 {
            return Err(Stale::NoSnapshot);
        }
        if now_ms.saturating_sub(self.observed_at) > self.freshness.snapshot_max_age_ms {
            return Err(Stale::SnapshotAge);
        }
        match self.appraisal_at {
            Some(at) if now_ms.saturating_sub(at) <= self.freshness.appraisal_max_age_ms => {}
            _ => return Err(Stale::Appraisal),
        }
        if self.external_exposure > 0.0 {
            match self.external_equity_at {
                Some(at)
                    if self.external_equity.is_some()
                        && now_ms.saturating_sub(at)
                            <= self.freshness.external_equity_max_age_ms => {}
                _ => return Err(Stale::ExternalEquity),
            }
        }
        if self.queued_withdrawal_value.is_none() {
            return Err(Stale::QueuedWithdrawals);
        }
        self.risk_nav.ok_or(Stale::Appraisal)
    }

    /// Best spot capacity across the configured tiers.
    fn spot_buy_max(&self) -> f64 {
        self.spot_buy_capacity_by_slippage.iter().map(|t| t.capacity).fold(0.0, f64::max)
    }
    fn spot_sell_max(&self) -> f64 {
        self.spot_sell_capacity_by_slippage.iter().map(|t| t.capacity).fold(0.0, f64::max)
    }

    /// Margin cash a new hedge position can draw on: the on-chain
    /// budget / daily release remaining plus the free margin already at
    /// the venue. `None` = unbounded (paper venue).
    fn margin_cash_available(&self) -> Option<f64> {
        if self.venue_margin_unbounded {
            return None;
        }
        let free_at_venue = (self.venue_margin_headroom * self.venue_initial_margin).max(0.0);
        Some(
            self.external_budget_remaining.min(self.external_daily_release_remaining).max(0.0)
                + free_at_venue,
        )
    }

    /// Test-only: a fresh, generously-funded snapshot at `nav` observed
    /// `now_ms` — every non-NAV constraint is far from binding.
    #[cfg(test)]
    pub(crate) fn test_fresh(nav: f64, now_ms: u64) -> Self {
        Self {
            appraised_nav: Some(nav),
            locally_reconstructed_nav: Some(nav),
            risk_nav: Some(nav),
            free_settlement: nav,
            queued_withdrawal_value: Some(0.0),
            venue_margin_unbounded: true,
            initial_margin_fraction: 0.10,
            stress_gap: 0.80,
            margin_topup_cap: f64::INFINITY,
            base_flash_capacity: 1e3 * nav,
            quote_flash_capacity: 1e3 * nav,
            observed_at: now_ms.max(1),
            appraisal_at: Some(now_ms),
            freshness: Freshness {
                appraisal_max_age_ms: u64::MAX / 4,
                external_equity_max_age_ms: u64::MAX / 4,
                snapshot_max_age_ms: u64::MAX / 4,
            },
            ..Default::default()
        }
    }
}

/// External-account inputs read from the indexer view + chain.
#[derive(Clone, Debug, Default)]
pub struct ExternalInputs {
    pub exposure: f64,
    pub equity: Option<f64>,
    pub equity_at: Option<u64>,
    pub budget_bps: u64,
    pub daily_release_bps: u64,
    pub released_in_window: f64,
    pub window_start_ms: u64,
    /// Total appraised NAV — what `release_external` sizes the limits
    /// against on chain.
    pub nav_for_limits: Option<f64>,
}

/// Venue margin picture from the monitors' last roster read.
#[derive(Clone, Copy, Debug, Default)]
pub struct VenueMarginInputs {
    pub initial_margin: f64,
    pub maintenance_margin: f64,
    pub headroom: f64,
    pub at_ms: u64,
}

/// Everything [`build_capital_snapshot`] needs, kept together so the
/// refresher call site stays readable. Marked figures come from the
/// holdings pass; reserved ones from `Book::reserved_split`.
pub struct CapitalInputs<'a> {
    pub now_ms: u64,
    pub appraised_nav: Option<f64>,
    pub appraisal_at: Option<u64>,
    pub free_settlement: f64,
    pub free_underlying_by_asset: HashMap<String, f64>,
    pub premium_deployed: f64,
    pub call_premium_marked: f64,
    pub put_premium_marked: f64,
    pub premium_by_expiry_marked: &'a HashMap<u64, f64>,
    pub call_strike_cash_marked: f64,
    pub put_underlying_value_marked: f64,
    pub exercise_demand_by_expiry_marked: &'a HashMap<u64, f64>,
    pub hedge_notional_marked: f64,
    pub hedge_notional_by_expiry_marked: &'a HashMap<u64, f64>,
    pub reserved: &'a ReservedSplit,
    pub queued_withdrawal_value: Option<f64>,
    pub external: Option<ExternalInputs>,
    pub venue: VenueMarginInputs,
    pub initial_margin_fraction: f64,
    pub stress_gap: f64,
}

/// Rolling 24h window mirror of `vault::release_external`.
const RELEASE_WINDOW_MS: u64 = 86_400_000;

/// Build the snapshot (pure; unit-tested). `risk_nav` is `None` — and
/// `stale` says why — whenever appraisal, external equity, or queued
/// withdrawals cannot back new risk.
pub fn build_capital_snapshot(cfg: &CapitalConfig, i: CapitalInputs<'_>) -> CapitalSnapshot {
    let freshness = Freshness {
        appraisal_max_age_ms: cfg.appraisal_max_age_secs.saturating_mul(1000),
        external_equity_max_age_ms: cfg.external_equity_max_age_secs.saturating_mul(1000),
        snapshot_max_age_ms: cfg.snapshot_max_age_secs.saturating_mul(1000),
    };
    let mut assumptions = vec![
        "unresolved_debits: not tracked (no in-flight PTB ledger) — 0".to_string(),
        "base_flash_capacity / quote_flash_capacity: configured assumption (no pool poller)"
            .to_string(),
        "spot_buy/sell_capacity_by_slippage: configured assumption (no router poller)"
            .to_string(),
    ];
    if !cfg.venue_margin_from_chain {
        assumptions.push(
            "venue margin capacity: unbounded (paper venue; set capital.venue_margin_from_chain \
             for a live venue)"
                .to_string(),
        );
    }

    let ext = i.external.clone().unwrap_or_default();
    let (budget_remaining, daily_remaining) = match ext.nav_for_limits {
        Some(nav) => {
            let budget = nav * ext.budget_bps as f64 / 10_000.0;
            let daily = nav * ext.daily_release_bps as f64 / 10_000.0;
            let released = if i.now_ms >= ext.window_start_ms.saturating_add(RELEASE_WINDOW_MS) {
                0.0
            } else {
                ext.released_in_window
            };
            ((budget - ext.exposure).max(0.0), (daily - released).max(0.0))
        }
        None => (0.0, 0.0),
    };

    let free_underlying_total: f64 = i.free_underlying_by_asset.values().sum();
    let local = i.free_settlement
        + free_underlying_total
        + i.premium_deployed
        + ext.equity.unwrap_or(0.0);

    let mut stale = Vec::new();
    match (i.appraised_nav, i.appraisal_at) {
        (Some(_), Some(at)) if i.now_ms.saturating_sub(at) <= freshness.appraisal_max_age_ms => {}
        _ => stale.push(Stale::Appraisal),
    }
    if ext.exposure > 0.0 {
        match (ext.equity, ext.equity_at) {
            (Some(_), Some(at))
                if i.now_ms.saturating_sub(at) <= freshness.external_equity_max_age_ms => {}
            _ => stale.push(Stale::ExternalEquity),
        }
    }
    if i.queued_withdrawal_value.is_none() {
        stale.push(Stale::QueuedWithdrawals);
    }
    let base = i.appraised_nav.map(|a| a.min(local));
    let liquidity_reserve = base.unwrap_or(0.0).max(0.0) * cfg.liquidity_reserve_frac;
    let risk_nav = if stale.is_empty() {
        base.map(|b| {
            (b - i.queued_withdrawal_value.unwrap_or(0.0) - liquidity_reserve).max(0.0)
        })
    } else {
        None
    };
    let margin_topup_cap = match risk_nav {
        Some(r) if cfg.venue_margin_from_chain => (cfg.margin_topup_max * r).min(daily_remaining),
        Some(r) => cfg.margin_topup_max * r,
        None => 0.0,
    };

    let r = i.reserved;
    let mut premium_by_expiry = i.premium_by_expiry_marked.clone();
    for (e, v) in &r.by_expiry {
        *premium_by_expiry.entry(*e).or_default() += v;
    }
    let mut exercise_demand_by_expiry = i.exercise_demand_by_expiry_marked.clone();
    for (e, v) in &r.exercise_demand_by_expiry {
        *exercise_demand_by_expiry.entry(*e).or_default() += v;
    }
    let mut hedge_notional_by_expiry = i.hedge_notional_by_expiry_marked.clone();
    for (e, v) in &r.hedge_notional_by_expiry {
        *hedge_notional_by_expiry.entry(*e).or_default() += v;
    }

    CapitalSnapshot {
        appraised_nav: i.appraised_nav,
        locally_reconstructed_nav: Some(local),
        risk_nav,
        free_settlement: i.free_settlement,
        free_underlying_by_asset: i.free_underlying_by_asset,
        queued_withdrawal_value: i.queued_withdrawal_value,
        unresolved_debits: 0.0,
        liquidity_reserve,
        call_premium_marked: i.call_premium_marked,
        put_premium_marked: i.put_premium_marked,
        call_quote_reservations: r.calls,
        put_quote_reservations: r.puts,
        total_quote_reservations: r.total,
        premium_by_expiry,
        call_strike_cash_required: i.call_strike_cash_marked + r.call_strike_cash,
        put_underlying_value_required: i.put_underlying_value_marked + r.put_underlying_value,
        exercise_demand_by_expiry,
        hedge_notional: i.hedge_notional_marked + r.hedge_notional,
        hedge_notional_by_expiry,
        external_exposure: ext.exposure,
        external_equity: ext.equity,
        external_budget_remaining: budget_remaining,
        external_daily_release_remaining: daily_remaining,
        venue_initial_margin: i.venue.initial_margin,
        venue_maintenance_margin: i.venue.maintenance_margin,
        venue_margin_headroom: i.venue.headroom,
        venue_margin_unbounded: !cfg.venue_margin_from_chain,
        initial_margin_fraction: i.initial_margin_fraction,
        stress_gap: i.stress_gap,
        margin_topup_cap,
        base_flash_capacity: cfg.base_flash_capacity,
        quote_flash_capacity: cfg.quote_flash_capacity,
        spot_buy_capacity_by_slippage: cfg.spot_buy_capacity_by_slippage.clone(),
        spot_sell_capacity_by_slippage: cfg.spot_sell_capacity_by_slippage.clone(),
        observed_at: i.now_ms.max(1),
        appraisal_at: i.appraisal_at,
        external_equity_at: ext.equity_at,
        freshness,
        assumptions,
        stale,
    }
}

/// How one unit of premium translates into the venue and exercise
/// demands the capacity minima are stated in. A real fill supplies its
/// own (|delta|·S·amount / premium, strike cash / premium); the headline
/// `/desk/state` figures use the configured reference ATM ratios.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FillRatios {
    pub hedge_notional_per_premium: f64,
    pub exercise_cash_per_premium: f64,
}

/// Which constraint produced an effective capacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Binding {
    PremiumBudget,
    VenueMargin,
    ExerciseCapacity,
    FreeQuoteCash,
    MarginTopUp,
    ConcurrentExercise,
}

/// One effective capacity: remaining premium and the binding constraint.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capacity {
    pub premium: f64,
    pub binding: Binding,
}

fn min_capacity(candidates: &[(f64, Binding)]) -> Capacity {
    let mut best = Capacity { premium: f64::INFINITY, binding: Binding::PremiumBudget };
    for (v, b) in candidates {
        if *v < best.premium {
            best = Capacity { premium: *v, binding: *b };
        }
    }
    best.premium = best.premium.max(0.0);
    best
}

/// The effective-capacity minima of doc 08 §4.6 over a fresh snapshot
/// at `risk_nav`. Every figure is REMAINING premium (the marked book and
/// live reservations are already netted out of each numerator).
pub struct CapitalPolicy<'a> {
    pub limits: &'a LimitsConfig,
    pub snap: &'a CapitalSnapshot,
    pub risk_nav: f64,
}

impl CapitalPolicy<'_> {
    /// Cash a new quote could pay from: free settlement less the
    /// liquidity reserve and everything already reserved.
    fn free_quote_cash(&self) -> f64 {
        self.snap.free_settlement - self.snap.liquidity_reserve - self.snap.total_quote_reservations
    }

    /// Premium whose hedge the remaining margin cash can carry.
    fn margin_capacity(&self, r: &FillRatios) -> f64 {
        match self.snap.margin_cash_available() {
            None => f64::INFINITY,
            Some(cash) => {
                cash / self.snap.initial_margin_fraction.max(1e-9) / r.hedge_notional_per_premium.max(1e-9)
            }
        }
    }

    /// `min(call premium budget, short-perp margin capacity, call
    /// exercise settlement/sale capacity, free quote cash)`.
    pub fn call_capacity(&self, r: &FillRatios) -> Capacity {
        let s = self.snap;
        let budget = self.limits.call_premium_max * self.risk_nav
            - s.call_premium_marked
            - s.call_quote_reservations;
        let exercise_cash = s.free_settlement + s.quote_flash_capacity + s.spot_sell_max()
            - s.call_strike_cash_required;
        min_capacity(&[
            (budget, Binding::PremiumBudget),
            (self.margin_capacity(r), Binding::VenueMargin),
            (exercise_cash / r.exercise_cash_per_premium.max(1e-9), Binding::ExerciseCapacity),
            (self.free_quote_cash(), Binding::FreeQuoteCash),
        ])
    }

    /// `min(put premium budget, long-perp margin capacity, three-path
    /// put exercise capacity, free quote cash)`. The three paths deliver
    /// the underlying: vault-held underlying, base flash, spot buy.
    pub fn put_capacity(&self, r: &FillRatios) -> Capacity {
        let s = self.snap;
        let budget = self.limits.put_premium_max * self.risk_nav
            - s.put_premium_marked
            - s.put_quote_reservations;
        let free_underlying: f64 = s.free_underlying_by_asset.values().sum();
        let underlying = free_underlying + s.base_flash_capacity + s.spot_buy_max()
            - s.put_underlying_value_required;
        min_capacity(&[
            (budget, Binding::PremiumBudget),
            (self.margin_capacity(r), Binding::VenueMargin),
            (underlying / r.exercise_cash_per_premium.max(1e-9), Binding::ExerciseCapacity),
            (self.free_quote_cash(), Binding::FreeQuoteCash),
        ])
    }

    /// `min(per-expiry premium budget, concurrent exercise capacity,
    /// stressed margin top-up capacity)` for `expiry_ms`.
    pub fn expiry_capacity(&self, expiry_ms: u64, r: &FillRatios) -> Capacity {
        let s = self.snap;
        let at = |m: &HashMap<u64, f64>| m.get(&expiry_ms).copied().unwrap_or(0.0);
        let budget = self.limits.per_expiry_max * self.risk_nav - at(&s.premium_by_expiry);
        let free_underlying: f64 = s.free_underlying_by_asset.values().sum();
        let sources = s.free_settlement
            + s.quote_flash_capacity
            + s.spot_sell_max()
            + free_underlying
            + s.base_flash_capacity
            + s.spot_buy_max();
        let concurrent =
            (sources - at(&s.exercise_demand_by_expiry)) / r.exercise_cash_per_premium.max(1e-9);
        let topup = if s.venue_margin_unbounded {
            f64::INFINITY
        } else {
            let demand = s.stress_gap * at(&s.hedge_notional_by_expiry);
            (s.margin_topup_cap - demand)
                / (s.stress_gap * r.hedge_notional_per_premium).max(1e-9)
        };
        min_capacity(&[
            (budget, Binding::PremiumBudget),
            (concurrent, Binding::ConcurrentExercise),
            (topup, Binding::MarginTopUp),
        ])
    }
}

/// Current book exposure, all in settlement raw units except where noted.
#[derive(Clone, Debug, Default)]
pub struct BookExposure {
    /// Budget base from the indexer view (display, hedge bands, kill
    /// switch). The CAPS scale from `capital.risk_nav`, not this.
    pub nav: f64,
    /// The capital picture every dollar cap derives from (SO-444).
    pub capital: CapitalSnapshot,
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
    /// Marked premium held in CALLS (composition sublimit — SO-431).
    pub call_premium: f64,
    /// Marked premium held in PUTS.
    pub put_premium: f64,
    /// Positive-delta inventory (Σ of per-line delta·amount where > 0),
    /// underlying units — composition surface, doc 08 §4.5.
    pub delta_units_positive: f64,
    /// Negative-delta inventory (Σ where < 0; stored negative).
    pub delta_units_negative: f64,
    /// Gamma by option type, underlying units per 1.0 spot move.
    pub gamma_units_calls: f64,
    pub gamma_units_puts: f64,
    /// Kill switch already latched (from [`KillSwitch::check`]).
    pub kill_switch: bool,
}

/// The proposed new buy being tested against the caps.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProposedFill {
    /// Premium the fill would deploy, settlement raw.
    pub premium: f64,
    /// Put or call — drives the per-side premium sublimit.
    pub is_put: bool,
    /// Vega the fill would add, settlement raw per vol pt.
    pub vega_per_volpt: f64,
    /// Theta cost the fill would add, per day.
    pub theta_cost_per_day: f64,
    pub expiry_ms: u64,
    pub strike_bucket: usize,
    /// |delta| × spot × amount — the hedge the fill needs, settlement raw.
    pub hedge_notional: f64,
    /// Exercise demand: strike cash (calls) or underlying value (puts).
    pub exercise_cash: f64,
}

impl ProposedFill {
    /// The fill's own premium-conversion ratios (see [`FillRatios`]).
    pub fn ratios(&self) -> FillRatios {
        let p = self.premium.max(f64::MIN_POSITIVE);
        FillRatios {
            hedge_notional_per_premium: self.hedge_notional / p,
            exercise_cash_per_premium: self.exercise_cash / p,
        }
    }
}

/// Continuous utilizations in [0, ∞) — 1.0 = at the (hard) limit. Feeds
/// the inventory penalty (`vega_utilization` in the bid context).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Utilization {
    pub premium: f64,
    pub vega: f64,
    pub theta: f64,
    /// The fresh risk NAV the caps were scaled from (the bid's size
    /// denominator).
    pub risk_nav: f64,
}

/// Hard-decline reasons — the only cases where the desk refuses to quote
/// the buy side (everything else degrades via the vol discount).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HardDecline {
    PremiumBudget,
    /// The call effective capacity, with the constraint that bound.
    CallCapacity(Binding),
    PutCapacity(Binding),
    VegaCap,
    ThetaGovernor,
    /// The per-expiry effective capacity, with the constraint that bound.
    ExpiryCapacity(Binding),
    StrikeConcentration,
    KillSwitch,
    /// The capital snapshot cannot back new risk (doc 08 §0.4).
    StaleCapital(Stale),
}

impl HardDecline {
    pub fn as_str(self) -> &'static str {
        match self {
            HardDecline::PremiumBudget => "premium budget hard cap",
            HardDecline::CallCapacity(Binding::PremiumBudget) => "call premium sublimit",
            HardDecline::CallCapacity(Binding::VenueMargin) => "call short-perp margin capacity",
            HardDecline::CallCapacity(Binding::ExerciseCapacity) => {
                "call exercise settlement capacity"
            }
            HardDecline::CallCapacity(_) => "call capacity: free quote cash",
            HardDecline::PutCapacity(Binding::PremiumBudget) => "put premium sublimit",
            HardDecline::PutCapacity(Binding::VenueMargin) => "put long-perp margin capacity",
            HardDecline::PutCapacity(Binding::ExerciseCapacity) => "put exercise underlying capacity",
            HardDecline::PutCapacity(_) => "put capacity: free quote cash",
            HardDecline::VegaCap => "net vega cap",
            HardDecline::ThetaGovernor => "theta governor hard cap",
            HardDecline::ExpiryCapacity(Binding::ConcurrentExercise) => {
                "per-expiry concurrent exercise capacity"
            }
            HardDecline::ExpiryCapacity(Binding::MarginTopUp) => {
                "per-expiry stressed margin top-up capacity"
            }
            HardDecline::ExpiryCapacity(_) => "per-expiry concentration cap",
            HardDecline::StrikeConcentration => "per-strike-bucket concentration cap",
            HardDecline::KillSwitch => "kill switch (NAV drawdown)",
            HardDecline::StaleCapital(s) => s.as_str(),
        }
    }
}

/// Evaluate the caps for a proposed buy at `now_ms`. `Ok(Utilization)`
/// reflects the book INCLUDING the fill (so the bid path prices
/// post-fill inventory). Every dollar cap scales from the snapshot's
/// fresh `risk_nav`; the call, put and per-expiry caps are the lesser of
/// their NAV budget and the measured venue capacity.
pub fn evaluate(
    cfg: &LimitsConfig,
    x: &BookExposure,
    fill: &ProposedFill,
    now_ms: u64,
) -> Result<Utilization, HardDecline> {
    if x.kill_switch {
        return Err(HardDecline::KillSwitch);
    }
    let risk_nav = x.capital.risk_nav_at(now_ms).map_err(HardDecline::StaleCapital)?;
    if risk_nav <= 0.0 {
        return Err(HardDecline::PremiumBudget);
    }
    let premium_after = x.premium_deployed + x.reserved + fill.premium;
    if premium_after > cfg.premium_budget_hard * risk_nav {
        return Err(HardDecline::PremiumBudget);
    }
    // Per-side effective capacities (doc 08 §0.4/§4.6): the lesser of
    // the side's NAV budget and the venue/exercise/cash capacity, so one
    // side can never consume the whole book or outrun its hedge.
    let policy = CapitalPolicy { limits: cfg, snap: &x.capital, risk_nav };
    let ratios = fill.ratios();
    if fill.is_put {
        let c = policy.put_capacity(&ratios);
        if fill.premium > c.premium {
            return Err(HardDecline::PutCapacity(c.binding));
        }
    } else {
        let c = policy.call_capacity(&ratios);
        if fill.premium > c.premium {
            return Err(HardDecline::CallCapacity(c.binding));
        }
    }
    let vega_after = x.net_vega_per_volpt + fill.vega_per_volpt;
    let vega_cap = cfg.vega_cap_nav_per_volpt * risk_nav;
    if vega_after.abs() > vega_cap {
        return Err(HardDecline::VegaCap);
    }
    let theta_after = x.theta_cost_per_day + fill.theta_cost_per_day;
    if theta_after > cfg.theta_hard_nav_per_day * risk_nav {
        return Err(HardDecline::ThetaGovernor);
    }
    let c = policy.expiry_capacity(fill.expiry_ms, &ratios);
    if fill.premium > c.premium {
        return Err(HardDecline::ExpiryCapacity(c.binding));
    }
    let bucket = fill.strike_bucket.min(2);
    if x.premium_by_strike_bucket[bucket] + fill.premium > cfg.per_strike_bucket_max * risk_nav {
        return Err(HardDecline::StrikeConcentration);
    }
    Ok(Utilization {
        premium: premium_after / (cfg.premium_budget_soft * risk_nav).max(f64::MIN_POSITIVE),
        vega: vega_after.abs() / vega_cap.max(f64::MIN_POSITIVE),
        theta: (theta_after / (cfg.theta_soft_nav_per_day * risk_nav).max(f64::MIN_POSITIVE))
            .max(0.0),
        risk_nav,
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

    /// A realistic unix-ms "now" (2026-09-01) so age arithmetic never
    /// underflows.
    const NOW: u64 = 1_788_220_800_000;

    fn cfg() -> LimitsConfig {
        LimitsConfig::default()
    }

    /// NAV 1e9 with a fresh, unconstrained capital snapshot; everything
    /// else empty.
    fn base() -> BookExposure {
        BookExposure {
            nav: 1e9,
            capital: CapitalSnapshot::test_fresh(1e9, NOW),
            ..Default::default()
        }
    }

    /// ATM-ish call: hedge notional 6× premium, strike cash 12× premium.
    fn fill() -> ProposedFill {
        ProposedFill {
            premium: 1e6,
            is_put: false,
            vega_per_volpt: 1e5,
            theta_cost_per_day: 1e4,
            expiry_ms: 42,
            strike_bucket: 1,
            hedge_notional: 6e6,
            exercise_cash: 12e6,
        }
    }

    fn ratios() -> FillRatios {
        fill().ratios()
    }

    #[test]
    fn happy_path_reports_utilizations() {
        let u = evaluate(&cfg(), &base(), &fill(), NOW).unwrap();
        assert_eq!(u.risk_nav, 1e9);
        // premium: 1e6 / (0.25 × 1e9) = 0.004
        assert!((u.premium - 1e6 / 2.5e8).abs() < 1e-9);
        // vega: 1e5 / (0.005 × 1e9) = 0.02
        assert!((u.vega - 0.02).abs() < 1e-9);
        // theta: 1e4 / (0.0010 × 1e9) = 0.01
        assert!((u.theta - 0.01).abs() < 1e-9);
    }

    #[test]
    fn premium_hard_cap_trips() {
        let mut x = base();
        x.premium_deployed = 0.29 * 1e9;
        x.reserved = 0.005 * 1e9;
        let mut f = fill();
        f.premium = 0.006 * 1e9; // 29% + 0.5% + 0.6% > 30%
        assert_eq!(evaluate(&cfg(), &x, &f, NOW), Err(HardDecline::PremiumBudget));
        // Reservations count against the budget.
        x.reserved = 0.0;
        assert!(evaluate(&cfg(), &x, &f, NOW).is_ok());
    }

    #[test]
    fn per_side_sublimits_trip_independently(){
        // Call-heavy book: the next call breaches the 20% call cap while
        // an identical put is still welcome (mixed books keep quoting).
        let mut x = base();
        x.call_premium = 0.199 * 1e9;
        x.capital.call_premium_marked = 0.199 * 1e9;
        x.premium_deployed = 0.199 * 1e9;
        let mut f = fill();
        f.premium = 0.002 * 1e9;
        f.hedge_notional = 6.0 * f.premium;
        f.exercise_cash = 12.0 * f.premium;
        assert_eq!(
            evaluate(&cfg(), &x, &f, NOW),
            Err(HardDecline::CallCapacity(Binding::PremiumBudget))
        );
        f.is_put = true;
        assert!(evaluate(&cfg(), &x, &f, NOW).is_ok());
        // Put-heavy book mirrors it.
        let mut x = base();
        x.put_premium = 0.199 * 1e9;
        x.capital.put_premium_marked = 0.199 * 1e9;
        x.premium_deployed = 0.199 * 1e9;
        assert_eq!(
            evaluate(&cfg(), &x, &f, NOW),
            Err(HardDecline::PutCapacity(Binding::PremiumBudget))
        );
        f.is_put = false;
        assert!(evaluate(&cfg(), &x, &f, NOW).is_ok());
    }

    #[test]
    fn vega_cap_trips_on_absolute_value() {
        let mut x = base();
        x.net_vega_per_volpt = 0.0049 * 1e9;
        let mut f = fill();
        f.vega_per_volpt = 0.0002 * 1e9;
        assert_eq!(evaluate(&cfg(), &x, &f, NOW), Err(HardDecline::VegaCap));
        // Short vega breaches the same cap (V2 symmetry is handled by the
        // V2 band; V1's cap is |net|).
        x.net_vega_per_volpt = -0.0052 * 1e9;
        f.vega_per_volpt = 0.0;
        assert_eq!(evaluate(&cfg(), &x, &f, NOW), Err(HardDecline::VegaCap));
    }

    #[test]
    fn theta_governor_trips() {
        let mut x = base();
        x.theta_cost_per_day = 0.0014 * 1e9;
        let mut f = fill();
        f.theta_cost_per_day = 0.0002 * 1e9;
        assert_eq!(evaluate(&cfg(), &x, &f, NOW), Err(HardDecline::ThetaGovernor));
    }

    #[test]
    fn concentration_caps_trip() {
        let mut x = base();
        x.capital.premium_by_expiry.insert(42, 0.099 * 1e9);
        let mut f = fill();
        f.premium = 0.002 * 1e9;
        f.hedge_notional = 6.0 * f.premium;
        f.exercise_cash = 12.0 * f.premium;
        assert_eq!(
            evaluate(&cfg(), &x, &f, NOW),
            Err(HardDecline::ExpiryCapacity(Binding::PremiumBudget))
        );
        // A different expiry is fine.
        f.expiry_ms = 43;
        assert!(evaluate(&cfg(), &x, &f, NOW).is_ok());
        // Strike bucket.
        let mut x = base();
        x.premium_by_strike_bucket[2] = 0.099 * 1e9;
        f.expiry_ms = 42;
        f.strike_bucket = 2;
        assert_eq!(evaluate(&cfg(), &x, &f, NOW), Err(HardDecline::StrikeConcentration));
        f.strike_bucket = 0;
        assert!(evaluate(&cfg(), &x, &f, NOW).is_ok());
    }

    #[test]
    fn kill_switch_blocks_everything() {
        let mut x = base();
        x.kill_switch = true;
        assert_eq!(evaluate(&cfg(), &x, &fill(), NOW), Err(HardDecline::KillSwitch));
    }

    // ── capital snapshot + policy (doc 08 §4.6 gates, SO-444) ──────────

    /// Marked-book inputs for `build_capital_snapshot` at appraised NAV
    /// `nav`, fresh at `NOW`, with free settlement `free`.
    fn inputs<'a>(
        nav: f64,
        free: f64,
        reserved: &'a ReservedSplit,
        empty: &'a HashMap<u64, f64>,
    ) -> CapitalInputs<'a> {
        CapitalInputs {
            now_ms: NOW,
            appraised_nav: Some(nav),
            appraisal_at: Some(NOW - 60_000),
            free_settlement: free,
            free_underlying_by_asset: HashMap::new(),
            premium_deployed: 0.0,
            call_premium_marked: 0.0,
            put_premium_marked: 0.0,
            premium_by_expiry_marked: empty,
            call_strike_cash_marked: 0.0,
            put_underlying_value_marked: 0.0,
            exercise_demand_by_expiry_marked: empty,
            hedge_notional_marked: 0.0,
            hedge_notional_by_expiry_marked: empty,
            reserved,
            queued_withdrawal_value: Some(0.0),
            external: None,
            venue: VenueMarginInputs::default(),
            initial_margin_fraction: 0.10,
            stress_gap: 0.80,
        }
    }

    #[test]
    fn risk_nav_is_the_conservative_formula() {
        let c = CapitalConfig::default();
        let r = ReservedSplit::default();
        let e = HashMap::new();
        // Appraised 1e9, local = free 8e8 + marks 1e8 = 9e8 → min 9e8;
        // reserve 10% of that; queued 5e7.
        let mut i = inputs(1e9, 8e8, &r, &e);
        i.premium_deployed = 1e8;
        i.queued_withdrawal_value = Some(5e7);
        let s = build_capital_snapshot(&c, i);
        assert_eq!(s.locally_reconstructed_nav, Some(9e8));
        assert_eq!(s.liquidity_reserve, 9e7);
        assert_eq!(s.risk_nav_at(NOW), Ok(9e8 - 5e7 - 9e7));
        assert!(s.stale.is_empty());
        // A falling local NAV drags risk NAV down even with the appraisal
        // unchanged (min of the two).
        let mut i = inputs(1e9, 4e8, &r, &e);
        i.premium_deployed = 1e8;
        let s2 = build_capital_snapshot(&c, i);
        assert!(s2.risk_nav.unwrap() < s.risk_nav.unwrap());
    }

    #[test]
    fn every_derived_cap_tracks_fresh_risk_nav() {
        let r = FillRatios { hedge_notional_per_premium: 6.0, exercise_cash_per_premium: 12.0 };
        let l = cfg();
        let at = |nav: f64| {
            let snap = CapitalSnapshot::test_fresh(nav, NOW);
            let p = CapitalPolicy { limits: &l, snap: &snap, risk_nav: nav };
            (
                p.call_capacity(&r).premium,
                p.put_capacity(&r).premium,
                p.expiry_capacity(1, &r).premium,
            )
        };
        let (c1, p1, e1) = at(1e9);
        let (c2, p2, e2) = at(2e9);
        assert!((c1 - 0.20 * 1e9).abs() < 1.0 && (c2 - 2.0 * c1).abs() < 1.0);
        assert!((p1 - 0.20 * 1e9).abs() < 1.0 && (p2 - 2.0 * p1).abs() < 1.0);
        assert!((e1 - 0.10 * 1e9).abs() < 1.0 && (e2 - 2.0 * e1).abs() < 1.0);
        let (c3, _, _) = at(5e8);
        assert!((c3 - 0.5 * c1).abs() < 1.0, "caps fall with NAV too");
    }

    #[test]
    fn fixed_venue_and_flash_capacity_bind_as_nav_grows() {
        let r = FillRatios { hedge_notional_per_premium: 6.0, exercise_cash_per_premium: 12.0 };
        let l = cfg();
        // Exercise cash sources fixed at 1.2e8 (free settlement 2e7 +
        // quote flash 1e8): 1e7 of call premium can be exercised.
        let exercise_bound = |nav: f64| {
            let mut snap = CapitalSnapshot::test_fresh(nav, NOW);
            snap.free_settlement = 2e7;
            snap.quote_flash_capacity = 1e8;
            snap.base_flash_capacity = 0.0;
            let p = CapitalPolicy { limits: &l, snap: &snap, risk_nav: nav };
            p.call_capacity(&r)
        };
        let small = exercise_bound(1e7);
        assert_eq!(small.binding, Binding::PremiumBudget); // 2e6 budget < 1e7
        let big = exercise_bound(1e9);
        assert_eq!(big.binding, Binding::ExerciseCapacity);
        assert!((big.premium - 1e7).abs() < 1.0);
        let huge = exercise_bound(1e12);
        assert_eq!(huge.premium, big.premium, "flash capacity stops the cap growing");
        // Venue margin from chain: 1e6 budget remaining / 10% initial
        // margin / 6× hedge notional ≈ 1.67e6 of premium, whatever NAV.
        let margin_bound = |nav: f64| {
            let mut snap = CapitalSnapshot::test_fresh(nav, NOW);
            snap.venue_margin_unbounded = false;
            snap.external_budget_remaining = 1e6;
            snap.external_daily_release_remaining = 1e6;
            let p = CapitalPolicy { limits: &l, snap: &snap, risk_nav: nav };
            p.put_capacity(&r)
        };
        let m = margin_bound(1e9);
        assert_eq!(m.binding, Binding::VenueMargin);
        assert!((m.premium - 1e6 / 0.10 / 6.0).abs() < 1.0);
        assert_eq!(margin_bound(1e12).premium, m.premium);
        // Per-expiry: the stressed top-up cap (10% risk NAV, or the daily
        // release remaining) binds before the 10% premium budget once
        // the hedge is 6× premium and the gap 80%.
        let mut snap = CapitalSnapshot::test_fresh(1e9, NOW);
        snap.venue_margin_unbounded = false;
        snap.external_budget_remaining = 1e12;
        snap.external_daily_release_remaining = 2e7;
        snap.margin_topup_cap = 2e7;
        let p = CapitalPolicy { limits: &l, snap: &snap, risk_nav: 1e9 };
        let e = p.expiry_capacity(1, &r);
        assert_eq!(e.binding, Binding::MarginTopUp);
        assert!((e.premium - 2e7 / (0.80 * 6.0)).abs() < 1.0);
    }

    #[test]
    fn stale_appraisal_equity_or_withdrawals_block_new_risk() {
        let c = CapitalConfig::default();
        let r = ReservedSplit::default();
        let e = HashMap::new();
        // Fresh: quotes.
        let s = build_capital_snapshot(&c, inputs(1e9, 1e9, &r, &e));
        assert!(s.risk_nav_at(NOW).is_ok());
        // Stale appraisal.
        let mut i = inputs(1e9, 1e9, &r, &e);
        i.appraisal_at = Some(NOW - 2 * 3_600_000);
        let s = build_capital_snapshot(&c, i);
        assert_eq!(s.stale, vec![Stale::Appraisal]);
        assert_eq!(s.risk_nav_at(NOW), Err(Stale::Appraisal));
        // Missing appraisal entirely.
        let mut i = inputs(1e9, 1e9, &r, &e);
        i.appraised_nav = None;
        assert_eq!(build_capital_snapshot(&c, i).risk_nav_at(NOW), Err(Stale::Appraisal));
        // Live external exposure with stale equity.
        let mut i = inputs(1e9, 1e9, &r, &e);
        i.external = Some(ExternalInputs {
            exposure: 1e7,
            equity: Some(1e7),
            equity_at: Some(NOW - 3_600_000),
            nav_for_limits: Some(1e9),
            ..Default::default()
        });
        assert_eq!(build_capital_snapshot(&c, i).risk_nav_at(NOW), Err(Stale::ExternalEquity));
        // Zero exposure needs no equity leg.
        let mut i = inputs(1e9, 1e9, &r, &e);
        i.external = Some(ExternalInputs { nav_for_limits: Some(1e9), ..Default::default() });
        assert!(build_capital_snapshot(&c, i).risk_nav_at(NOW).is_ok());
        // Unvaluable queued withdrawals.
        let mut i = inputs(1e9, 1e9, &r, &e);
        i.queued_withdrawal_value = None;
        assert_eq!(
            build_capital_snapshot(&c, i).risk_nav_at(NOW),
            Err(Stale::QueuedWithdrawals)
        );
        // The snapshot itself ages out at quote time; a default one
        // never backs risk.
        let s = build_capital_snapshot(&c, inputs(1e9, 1e9, &r, &e));
        assert_eq!(s.risk_nav_at(NOW + 301_000), Err(Stale::SnapshotAge));
        assert_eq!(CapitalSnapshot::default().risk_nav_at(NOW), Err(Stale::NoSnapshot));
        // And `evaluate` declines with the reason — a falling option
        // mark never creates capacity against a stale NAV.
        let mut x = base();
        x.capital = s;
        x.premium_deployed = 0.0;
        assert_eq!(
            evaluate(&cfg(), &x, &fill(), NOW + 301_000),
            Err(HardDecline::StaleCapital(Stale::SnapshotAge))
        );
    }

    #[test]
    fn reservations_cannot_double_spend_across_call_put_total_and_expiry() {
        // Ample flash capacity so only cash and budgets can bind.
        let c = CapitalConfig {
            liquidity_reserve_frac: 0.0,
            base_flash_capacity: 1e10,
            quote_flash_capacity: 1e10,
            ..CapitalConfig::default()
        };
        let l = cfg();
        let e = HashMap::new();
        let ratio = ratios();
        // One live CALL reservation of 3e7 at expiry 42 needing 3.6e8 of
        // strike cash and 1.8e8 of hedge notional.
        let reserved = ReservedSplit {
            total: 3e7,
            calls: 3e7,
            puts: 0.0,
            by_expiry: HashMap::from([(42, 3e7)]),
            call_strike_cash: 3.6e8,
            put_underlying_value: 0.0,
            exercise_demand_by_expiry: HashMap::from([(42, 3.6e8)]),
            hedge_notional: 1.8e8,
            hedge_notional_by_expiry: HashMap::from([(42, 1.8e8)]),
        };
        let none = ReservedSplit::default();
        // Free settlement 1e8 (< 20% × 1e9): free quote cash is the
        // binding constraint on both sides, so the same cash shows up
        // in the put capacity too.
        let with = build_capital_snapshot(&c, inputs(1e9, 1e8, &reserved, &e));
        let without = build_capital_snapshot(&c, inputs(1e9, 1e8, &none, &e));
        let pw = CapitalPolicy { limits: &l, snap: &with, risk_nav: 1e9 };
        let po = CapitalPolicy { limits: &l, snap: &without, risk_nav: 1e9 };
        // Call side: the reservation counts against the call budget AND
        // the free cash.
        assert_eq!(with.call_quote_reservations, 3e7);
        let (cw, co) = (pw.call_capacity(&ratio), po.call_capacity(&ratio));
        assert_eq!((co.premium, co.binding), (1e8, Binding::FreeQuoteCash));
        assert_eq!((cw.premium, cw.binding), (7e7, Binding::FreeQuoteCash));
        // Put side: no put premium reserved, yet the cash is gone.
        assert_eq!(with.put_quote_reservations, 0.0);
        assert_eq!(pw.put_capacity(&ratio).premium, 7e7);
        // Expiry 42 carries it; expiry 43 does not.
        let e42 = pw.expiry_capacity(42, &ratio).premium;
        let e43 = pw.expiry_capacity(43, &ratio).premium;
        assert!((e42 - (1e8 - 3e7)).abs() < 1.0, "{e42}");
        assert!((e43 - 1e8).abs() < 1.0, "{e43}");
        // And the exercise-cash side nets the reserved strike cash: with
        // ample free cash the call exercise capacity drops by 3.6e8/12.
        let mut ample = CapitalSnapshot::test_fresh(1e9, NOW);
        ample.quote_flash_capacity = 0.0;
        let free = CapitalPolicy { limits: &l, snap: &ample, risk_nav: 1e9 }
            .call_capacity(&ratio)
            .premium;
        ample.call_strike_cash_required = 3.6e8;
        ample.call_quote_reservations = 3e7;
        ample.total_quote_reservations = 3e7;
        let after =
            CapitalPolicy { limits: &l, snap: &ample, risk_nav: 1e9 }.call_capacity(&ratio);
        assert!(after.premium < free);
        // The total premium hard cap sees the reservation through
        // `BookExposure::reserved` as before.
        let mut x = base();
        x.reserved = 3e7;
        x.premium_deployed = 0.28 * 1e9;
        let mut f = fill();
        f.premium = 0.001 * 1e9;
        assert_eq!(evaluate(&cfg(), &x, &f, NOW), Err(HardDecline::PremiumBudget));
    }

    #[test]
    fn external_limits_follow_the_on_chain_window() {
        let c = CapitalConfig { venue_margin_from_chain: true, ..CapitalConfig::default() };
        let r = ReservedSplit::default();
        let e = HashMap::new();
        let mut i = inputs(1e9, 1e9, &r, &e);
        // 20% budget, 10% daily on a 1e9 appraised NAV; 5e7 exposed, 3e7
        // released in the current window.
        i.external = Some(ExternalInputs {
            exposure: 5e7,
            equity: Some(5e7),
            equity_at: Some(NOW - 1_000),
            budget_bps: 2_000,
            daily_release_bps: 1_000,
            released_in_window: 3e7,
            window_start_ms: NOW - 3_600_000,
            nav_for_limits: Some(1e9),
        });
        let s = build_capital_snapshot(&c, i);
        assert_eq!(s.external_budget_remaining, 2e8 - 5e7);
        assert_eq!(s.external_daily_release_remaining, 1e8 - 3e7);
        assert!(!s.venue_margin_unbounded);
        // The top-up cap is the lesser of 10% risk NAV and the daily
        // release remaining.
        assert_eq!(s.margin_topup_cap, (0.10 * s.risk_nav.unwrap()).min(7e7));
        // A window that rolled over frees the whole daily release.
        let mut i = inputs(1e9, 1e9, &r, &e);
        i.external = Some(ExternalInputs {
            released_in_window: 3e7,
            window_start_ms: NOW - 2 * 86_400_000,
            daily_release_bps: 1_000,
            budget_bps: 2_000,
            nav_for_limits: Some(1e9),
            ..Default::default()
        });
        assert_eq!(build_capital_snapshot(&c, i).external_daily_release_remaining, 1e8);
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
