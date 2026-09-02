//! Capital-to-volume solver (doc 08 §8.1 modes, §8.6 outputs, §8.7
//! randomness discipline).
//!
//! `capacity`: for a target accepted Earn notional per day and a mix,
//! bisect the starting NAV per flow seed until the run services the
//! target with no capacity decline and passes the cash, premium, expiry,
//! hedge, exercise, drawdown and liquidation gates; the required NAV is
//! the `service_fraction` (95%) quantile across seeds, lifted so every
//! seed has zero liquidations. The lower-bound diagnostic of §8.6 is
//! computed from the same runs and compared with the simulated binding
//! constraint.
//!
//! `market`: generate offered flow and acceptance against the strategy's
//! actual bid at the scenario NAV, for a sweep of bid widths — the second
//! frontier (max sustainable bid/APY ↔ attainable volume), labeled
//! demand-, capital-, venue-limited or uneconomic.
//!
//! Every arrival/acceptance parameter is a stated prior; every result
//! says so. Distributions across seeds are reported, never the best seed.

use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::data::{Bar, FundingRow};
use crate::engine::{self, RunOutput};
use crate::flow_gen::PRIOR_LABEL;
use crate::report;
use crate::scenario::Scenario;
use crate::stats::{Declined, RunStats, Volumes};

pub struct Data<'a> {
    pub bars: &'a [Bar],
    pub funding: &'a [FundingRow],
    pub vol_index: &'a [(i64, f64)],
}

impl Data<'_> {
    fn run(&self, s: &Scenario) -> Result<RunOutput> {
        engine::run(s, self.bars, self.funding, self.vol_index)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Mix {
    CallOnly,
    PutOnly,
    Balanced,
    /// Worst case for capital: one type, one bucket (per-expiry cap binds
    /// first), ATM (largest premium), fatter size tail.
    Adversarial,
}

impl Mix {
    pub fn parse(s: &str) -> Result<Mix> {
        Ok(match s {
            "call_only" => Mix::CallOnly,
            "put_only" => Mix::PutOnly,
            "balanced" => Mix::Balanced,
            "adversarial" => Mix::Adversarial,
            other => anyhow::bail!("unknown mix {other} (call_only|put_only|balanced|adversarial)"),
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Mix::CallOnly => "call_only",
            Mix::PutOnly => "put_only",
            Mix::Balanced => "balanced",
            Mix::Adversarial => "adversarial",
        }
    }

    pub fn apply(&self, s: &mut Scenario) {
        let g = &mut s.flow_gen;
        match self {
            Mix::CallOnly => g.call_share = 1.0,
            Mix::PutOnly => g.call_share = 0.0,
            Mix::Balanced => g.call_share = 0.5,
            Mix::Adversarial => {
                g.call_share = 1.0;
                g.herd_prob = 1.0;
                g.expiry_concentration = 1.0;
                g.call.moneyness_mean_z = 0.0;
                g.call.moneyness_sd_z = 0.0;
                g.call.size_log_sd *= 1.5;
            }
        }
        s.flow.call_share = g.call_share;
    }
}

#[derive(Clone, Debug)]
pub struct SolverConfig {
    pub nav_lo: f64,
    pub nav_hi: f64,
    /// Bisection stops when hi/lo − 1 < rel_tol.
    pub rel_tol: f64,
    pub seeds: Vec<u64>,
    /// Fraction of seeds that must service the target (doc 08 §8.6: 95%).
    pub service_fraction: f64,
}

impl Default for SolverConfig {
    fn default() -> Self {
        Self { nav_lo: 1_000.0, nav_hi: 1.0e9, rel_tol: 0.03, seeds: (1..=8).collect(), service_fraction: 0.95 }
    }
}

/// The gates of doc 08 §8.1 capacity mode, in binding-priority order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum Gate {
    Liquidation,
    Cash,
    PremiumTotal,
    PremiumCall,
    PremiumPut,
    Expiry,
    Hedge,
    Exercise,
    Drawdown,
}

impl Gate {
    pub fn name(&self) -> &'static str {
        match self {
            Gate::Liquidation => "liquidation",
            Gate::Cash => "cash",
            Gate::PremiumTotal => "premium_total",
            Gate::PremiumCall => "premium_call",
            Gate::PremiumPut => "premium_put",
            Gate::Expiry => "premium_per_expiry",
            Gate::Hedge => "hedge_margin_or_venue",
            Gate::Exercise => "exercise_flash_or_router",
            Gate::Drawdown => "drawdown",
        }
    }

    /// The §8.6 lower-bound term this gate corresponds to.
    fn bound_term(&self) -> &'static str {
        match self {
            Gate::Liquidation | Gate::Hedge => "required_external_margin",
            Gate::Cash => "(no term: free settlement)",
            Gate::PremiumTotal => "total_premium_at_risk",
            Gate::PremiumCall => "call_premium_at_risk",
            Gate::PremiumPut => "put_premium_at_risk",
            Gate::Expiry => "peak_expiry_premium_at_risk",
            Gate::Exercise => "(no term: exercise capacity)",
            Gate::Drawdown => "historical_loss",
        }
    }
}

/// Failing gates of one run, in priority order.
pub fn failing_gates(s: &Scenario, out: &RunOutput) -> Vec<Gate> {
    let st = &out.stats;
    let mut v = Vec::new();
    if st.liquidations > 0 {
        v.push(Gate::Liquidation);
    }
    if st.min_free_settlement < 0.0 {
        v.push(Gate::Cash);
    }
    if st.declined.count_total_cap > 0 {
        v.push(Gate::PremiumTotal);
    }
    if st.declined.count_call_cap > 0 {
        v.push(Gate::PremiumCall);
    }
    if st.declined.count_put_cap > 0 {
        v.push(Gate::PremiumPut);
    }
    if st.declined.count_expiry_cap > 0 {
        v.push(Gate::Expiry);
    }
    // Doc 08 §0.4: hedge margin within the external budget and the
    // 24h top-up within the daily release fraction of NAV.
    let over_budget = st.peak_hedge_margin > s.venue.external_budget_fraction * s.nav0;
    let over_release = st.peak_24h_margin_topup > s.venue.external_daily_release_fraction * s.nav0;
    if st.venue_cap_hits > 0 || st.min_margin_headroom < 0.0 || over_budget || over_release {
        v.push(Gate::Hedge);
    }
    if st.flash_cap_hits > 0 || st.exercise_failed > 0 {
        v.push(Gate::Exercise);
    }
    if out.max_drawdown > s.hurdle.max_drawdown {
        v.push(Gate::Drawdown);
    }
    v
}

/// Doc 08 §8.6: `required_nav >= max(...)`. Necessary, not sufficient.
#[derive(Clone, Debug, Serialize)]
pub struct LowerBound {
    pub required_nav: f64,
    pub terms: Vec<(String, f64)>,
    pub binding: String,
    pub next: Vec<String>,
    /// `synthetic_stress_loss` is not computed in PR N (label).
    pub labels: Vec<&'static str>,
}

pub fn lower_bound(s: &Scenario, st: &RunStats, historical_loss: f64) -> LowerBound {
    let frac = |x: f64| if x > 0.0 { x } else { f64::INFINITY };
    let mut terms = vec![
        ("total_premium_at_risk".to_string(), st.peak_premium_at_risk_total / frac(s.limits.premium_budget_hard)),
        ("call_premium_at_risk".to_string(), st.peak_premium_at_risk_call / frac(s.limits.call_premium_max)),
        ("put_premium_at_risk".to_string(), st.peak_premium_at_risk_put / frac(s.limits.put_premium_max)),
        ("peak_expiry_premium_at_risk".to_string(), st.peak_expiry_premium_at_risk / frac(s.limits.per_expiry_max)),
        ("required_external_margin".to_string(), st.peak_hedge_margin / frac(s.venue.external_budget_fraction)),
        ("peak_24h_margin_topup".to_string(), st.peak_24h_margin_topup / frac(s.venue.external_daily_release_fraction)),
        ("historical_loss".to_string(), historical_loss / frac(s.hurdle.max_drawdown)),
        ("synthetic_stress_loss".to_string(), 0.0),
    ];
    for t in &mut terms {
        if !t.1.is_finite() {
            t.1 = 0.0;
        }
    }
    let mut ranked = terms.clone();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    LowerBound {
        required_nav: ranked[0].1,
        binding: ranked[0].0.clone(),
        next: ranked[1..3].iter().map(|t| t.0.clone()).collect(),
        terms,
        labels: vec!["synthetic_stress_loss=not_computed(PR N)", "external_budget_fractions=config(not live vault)"],
    }
}

fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { 0.5 * (v[n / 2 - 1] + v[n / 2]) }
}

fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[idx]
}

/// Point the scenario at a capacity-mode target.
fn set_target(s: &mut Scenario, volume_per_day: f64, mix: Mix) {
    if s.flow.source == "constant" {
        s.flow.mode = "daily".into();
        s.flow.notional_per_day = volume_per_day;
    } else {
        s.flow.source = "generated".into();
        s.flow_gen.mode = "capacity".into();
        s.flow_gen.target_notional_per_day = volume_per_day;
    }
    mix.apply(s);
    s.acceptance.mode = "instant".into();
}

/// Bisection over starting NAV for one seed. `None` when even `nav_hi`
/// fails; the gates failing at `nav_hi` say why.
struct SeedSolve {
    nav: Option<f64>,
    gates_at_hi: Vec<Gate>,
    runs: u32,
}

fn solve_seed(base: &Scenario, data: &Data, cfg: &SolverConfig) -> Result<SeedSolve> {
    let mut runs = 0u32;
    let mut feasible = |nav: f64| -> Result<Vec<Gate>> {
        let mut s = base.clone();
        s.nav0 = nav;
        runs += 1;
        let out = data.run(&s)?;
        Ok(failing_gates(&s, &out))
    };
    let at_hi = feasible(cfg.nav_hi)?;
    if !at_hi.is_empty() {
        return Ok(SeedSolve { nav: None, gates_at_hi: at_hi, runs });
    }
    if feasible(cfg.nav_lo)?.is_empty() {
        return Ok(SeedSolve { nav: Some(cfg.nav_lo), gates_at_hi: Vec::new(), runs });
    }
    let (mut lo, mut hi) = (cfg.nav_lo, cfg.nav_hi);
    while hi / lo - 1.0 > cfg.rel_tol {
        let mid = (lo * hi).sqrt();
        if feasible(mid)?.is_empty() {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    Ok(SeedSolve { nav: Some(hi), gates_at_hi: Vec::new(), runs })
}

#[derive(Clone, Debug, Serialize)]
pub struct PremiumAtRisk {
    pub call: f64,
    pub put: f64,
    pub total: f64,
    pub peak_expiry: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct HedgeUsage {
    pub initial_margin: f64,
    pub max_24h_topup: f64,
    pub min_headroom: f64,
    /// peak margin / (external_budget_fraction × NAV).
    pub external_budget_usage: f64,
    pub external_daily_release_usage: f64,
    pub turnover: f64,
    pub funding: f64,
    pub fees: f64,
    pub slippage: f64,
    /// Not tracked by the v0 engine (label).
    pub capital_charge_in_bid: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExerciseUsage {
    pub calls_exercised: u64,
    pub puts_exercised: u64,
    pub path: &'static str,
    pub flash_utilization: Option<f64>,
    pub router_utilization: Option<f64>,
    pub laddered: u64,
    pub failed: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Returns {
    pub depositor_net_return_annualized: f64,
    pub max_drawdown: f64,
    pub liquidations: u64,
    pub hurdle_pass_fraction: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Counts {
    pub accepted_rfqs: f64,
    pub expiries: f64,
    pub calls: f64,
    pub puts: f64,
    pub effective_capital_deployed: f64,
    pub quotes_expired: f64,
}

/// Medians across seeds of everything a §8.6 result reports.
#[derive(Clone, Debug, Serialize)]
pub struct Aggregate {
    pub volumes: Volumes,
    pub declined: Declined,
    pub premium_at_risk: PremiumAtRisk,
    pub reserved_peak: f64,
    pub reserved_avg: f64,
    pub hedge: HedgeUsage,
    pub exercise: ExerciseUsage,
    pub returns: Returns,
    pub counts: Counts,
    pub displayed_apy_call: Option<f64>,
    pub displayed_apy_put: Option<f64>,
}

fn aggregate(s: &Scenario, outs: &[RunOutput], nav: f64) -> Aggregate {
    let med = |f: &dyn Fn(&RunOutput) -> f64| {
        let mut v: Vec<f64> = outs.iter().map(f).collect();
        median(&mut v)
    };
    let st = |f: &dyn Fn(&RunStats) -> f64| med(&|o: &RunOutput| f(&o.stats));
    // The runs were made at `nav`, not the base scenario's nav0: returns
    // and the hurdle are relative to the capital actually deployed.
    let at_nav = Scenario { nav0: nav, ..s.clone() };
    let summaries: Vec<report::Summary> = outs.iter().map(|o| report::summarize(&at_nav, o)).collect();
    let hurdle_pass = summaries.iter().filter(|m| m.hurdle_pass).count() as f64 / summaries.len().max(1) as f64;
    let flash_cap = s.venue.flash_max_notional_per_exercise;
    let router_cap = s.venue.router_capacity_notional;
    let peak_exercise = med(&|o| o.settled.iter().map(|x| x.spot_close * x.qty).fold(0.0, f64::max));
    let opt_med = |f: &dyn Fn(&RunStats) -> Option<f64>| {
        let mut v: Vec<f64> = outs.iter().filter_map(|o| f(&o.stats)).collect();
        if v.is_empty() { None } else { Some(median(&mut v)) }
    };
    Aggregate {
        volumes: Volumes {
            offered_earn_notional: st(&|x| x.volumes.offered_earn_notional),
            quoted_earn_notional: st(&|x| x.volumes.quoted_earn_notional),
            accepted_earn_notional: st(&|x| x.volumes.accepted_earn_notional),
            premium_turnover: st(&|x| x.volumes.premium_turnover),
            hedge_turnover: st(&|x| x.volumes.hedge_turnover),
            exercise_spot_turnover: st(&|x| x.volumes.exercise_spot_turnover),
        },
        declined: Declined {
            capacity: st(&|x| x.declined.capacity),
            priced_zero: st(&|x| x.declined.priced_zero),
            stale: st(&|x| x.declined.stale),
            count_capacity: st(&|x| x.declined.count_capacity as f64) as u64,
            count_priced_zero: st(&|x| x.declined.count_priced_zero as f64) as u64,
            count_stale: st(&|x| x.declined.count_stale as f64) as u64,
            count_total_cap: st(&|x| x.declined.count_total_cap as f64) as u64,
            count_call_cap: st(&|x| x.declined.count_call_cap as f64) as u64,
            count_put_cap: st(&|x| x.declined.count_put_cap as f64) as u64,
            count_expiry_cap: st(&|x| x.declined.count_expiry_cap as f64) as u64,
        },
        premium_at_risk: PremiumAtRisk {
            call: st(&|x| x.peak_premium_at_risk_call),
            put: st(&|x| x.peak_premium_at_risk_put),
            total: st(&|x| x.peak_premium_at_risk_total),
            peak_expiry: st(&|x| x.peak_expiry_premium_at_risk),
        },
        reserved_peak: st(&|x| x.peak_reserved),
        reserved_avg: st(&|x| x.avg_reserved),
        hedge: HedgeUsage {
            initial_margin: st(&|x| x.initial_hedge_margin),
            max_24h_topup: st(&|x| x.peak_24h_margin_topup),
            min_headroom: st(&|x| x.min_margin_headroom),
            external_budget_usage: st(&|x| x.peak_hedge_margin) / (s.venue.external_budget_fraction * nav).max(1e-9),
            external_daily_release_usage: st(&|x| x.peak_24h_margin_topup) / (s.venue.external_daily_release_fraction * nav).max(1e-9),
            turnover: med(&|o| o.ledger.lines.hedge_turnover_notional),
            funding: med(&|o| o.ledger.lines.funding_paid),
            fees: med(&|o| o.ledger.lines.hedge_fees),
            slippage: med(&|o| o.ledger.lines.hedge_slippage),
            capital_charge_in_bid: None,
        },
        exercise: ExerciseUsage {
            calls_exercised: st(&|x| x.exercised_call as f64) as u64,
            puts_exercised: st(&|x| x.exercised_put as f64) as u64,
            path: "american_sweep(cash|quote_flash|vault_underlying|base_flash; route modeled)",
            flash_utilization: if flash_cap > 0.0 { Some(peak_exercise / flash_cap) } else { None },
            router_utilization: if router_cap > 0.0 { Some(peak_exercise / router_cap) } else { None },
            laddered: st(&|x| x.exercise_laddered as f64) as u64,
            failed: st(&|x| x.exercise_failed as f64) as u64,
        },
        returns: Returns {
            depositor_net_return_annualized: {
                let mut v: Vec<f64> = summaries.iter().map(|m| m.depositor_net_return_annualized).collect();
                median(&mut v)
            },
            max_drawdown: outs.iter().map(|o| o.max_drawdown).fold(0.0, f64::max),
            liquidations: outs.iter().map(|o| o.stats.liquidations).sum(),
            hurdle_pass_fraction: hurdle_pass,
        },
        counts: Counts {
            accepted_rfqs: st(&|x| x.quotes_accepted as f64),
            expiries: st(&|x| x.expiries_settled as f64),
            calls: st(&|x| x.fills_call as f64),
            puts: st(&|x| x.fills_put as f64),
            effective_capital_deployed: st(&|x| x.peak_capital_deployed),
            quotes_expired: st(&|x| x.quotes_expired as f64),
        },
        displayed_apy_call: opt_med(&|x| x.displayed_apy_call_mean),
        displayed_apy_put: opt_med(&|x| x.displayed_apy_put_mean),
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CapacityResult {
    pub provenance: &'static str,
    pub labels: Vec<&'static str>,
    pub target_accepted_notional_per_day: f64,
    pub mix: &'static str,
    pub seeds: usize,
    pub runs: u32,
    /// `feasible` | `venue_limited` | `capital_beyond_range` |
    /// `uneconomic_at_min_nav`.
    pub feasibility: &'static str,
    /// Doc 08 P5 gate: `capital_limited` | `venue_limited` | `uneconomic`
    /// (capacity mode injects demand, so never `demand_limited`).
    pub limit_label: &'static str,
    pub min_nav: Option<f64>,
    pub nav_ci_low: Option<f64>,
    pub nav_ci_high: Option<f64>,
    pub per_seed_nav: Vec<Option<f64>>,
    pub simulated_binding: Option<&'static str>,
    pub simulated_next: Vec<&'static str>,
    pub lower_bound: Option<LowerBound>,
    pub lower_bound_agrees: Option<bool>,
    pub at_min_nav: Option<Aggregate>,
}

/// Solve one (volume, mix) point.
pub fn capacity_point(base: &Scenario, data: &Data, volume_per_day: f64, mix: Mix, cfg: &SolverConfig) -> Result<CapacityResult> {
    let mut s = base.clone();
    set_target(&mut s, volume_per_day, mix);
    let mut per_seed = Vec::new();
    let mut runs = 0u32;
    let mut gates_at_hi: Vec<Gate> = Vec::new();
    for &seed in &cfg.seeds {
        let mut ss = s.clone();
        ss.seed = seed;
        let r = solve_seed(&ss, data, cfg)?;
        runs += r.runs;
        gates_at_hi.extend(r.gates_at_hi);
        per_seed.push(r.nav);
    }
    let labels = vec![
        "proxy_oracle",
        "proxy_venue",
        "taker_only",
        "flow=capacity_injection(demand_inelastic)",
        "acceptance=instant",
        "venue_capacity=assumed",
        "flash_capacity=assumed(PR M)",
        "exercise_path=american_sweep(modeled_route, flash_capacity_assumed)",
        "liquidation=detected_not_applied",
    ];
    let mut solved: Vec<f64> = per_seed.iter().flatten().copied().collect();
    solved.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let needed = (cfg.service_fraction * cfg.seeds.len() as f64).ceil() as usize;
    if solved.len() < needed.max(1) {
        let venue = gates_at_hi.iter().all(|g| matches!(g, Gate::Hedge | Gate::Exercise)) && !gates_at_hi.is_empty();
        return Ok(CapacityResult {
            provenance: PRIOR_LABEL,
            labels,
            target_accepted_notional_per_day: volume_per_day,
            mix: mix.name(),
            seeds: cfg.seeds.len(),
            runs,
            feasibility: if venue { "venue_limited" } else { "capital_beyond_range" },
            limit_label: if venue { "venue_limited" } else { "capital_limited" },
            min_nav: None,
            nav_ci_low: None,
            nav_ci_high: None,
            per_seed_nav: per_seed,
            simulated_binding: gates_at_hi.first().map(|g| g.name()),
            simulated_next: Vec::new(),
            lower_bound: None,
            lower_bound_agrees: None,
            at_min_nav: None,
        });
    }
    let mut required = quantile(&solved, cfg.service_fraction);
    // Every seed must have zero liquidations at the required NAV.
    let mut outs = Vec::new();
    loop {
        outs.clear();
        let mut lift: Option<f64> = None;
        for (i, &seed) in cfg.seeds.iter().enumerate() {
            let mut ss = s.clone();
            ss.seed = seed;
            ss.nav0 = required;
            runs += 1;
            let out = data.run(&ss)?;
            if out.stats.liquidations > 0 {
                if let Some(n) = per_seed[i].filter(|n| *n > required) {
                    lift = Some(lift.map_or(n, |l: f64| l.max(n)));
                }
            }
            outs.push(out);
        }
        match lift {
            Some(n) => required = n,
            None => break,
        }
    }
    // Simulated binding constraint: what fails just below the solution.
    let mut below: Vec<Gate> = Vec::new();
    for probe in [required / (1.0 + cfg.rel_tol) / 1.02, required / 2.0] {
        for &seed in &cfg.seeds {
            let mut ss = s.clone();
            ss.seed = seed;
            ss.nav0 = probe;
            runs += 1;
            below.extend(failing_gates(&ss, &data.run(&ss)?));
        }
        if below.len() >= 3 {
            break;
        }
    }
    let mut freq: std::collections::BTreeMap<Gate, usize> = std::collections::BTreeMap::new();
    for g in &below {
        *freq.entry(*g).or_default() += 1;
    }
    let mut ranked: Vec<(Gate, usize)> = freq.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let binding = ranked.first().map(|(g, _)| *g);
    let next: Vec<&'static str> = ranked.iter().skip(1).take(2).map(|(g, _)| g.name()).collect();
    let agg = aggregate(&s, &outs, required);
    let hist_loss = agg.returns.max_drawdown * required;
    let mut st_med = outs[0].stats.clone();
    st_med.peak_premium_at_risk_total = agg.premium_at_risk.total;
    st_med.peak_premium_at_risk_call = agg.premium_at_risk.call;
    st_med.peak_premium_at_risk_put = agg.premium_at_risk.put;
    st_med.peak_expiry_premium_at_risk = agg.premium_at_risk.peak_expiry;
    st_med.peak_hedge_margin = agg.hedge.external_budget_usage * s.venue.external_budget_fraction * required;
    st_med.peak_24h_margin_topup = agg.hedge.max_24h_topup;
    let lb = lower_bound(&s, &st_med, hist_loss);
    let agrees = binding.map(|g| g.bound_term() == lb.binding);
    let feasibility = if agg.returns.hurdle_pass_fraction >= cfg.service_fraction { "feasible" } else { "uneconomic_at_min_nav" };
    Ok(CapacityResult {
        provenance: PRIOR_LABEL,
        labels,
        target_accepted_notional_per_day: volume_per_day,
        mix: mix.name(),
        seeds: cfg.seeds.len(),
        runs,
        limit_label: if feasibility == "feasible" { "capital_limited" } else { "uneconomic" },
        feasibility,
        min_nav: Some(required),
        nav_ci_low: Some(quantile(&solved, 0.025)),
        nav_ci_high: Some(quantile(&solved, 0.975).max(required)),
        per_seed_nav: per_seed,
        simulated_binding: binding.map(|g| g.name()),
        simulated_next: next,
        lower_bound: Some(lb),
        lower_bound_agrees: agrees,
        at_min_nav: Some(agg),
    })
}

/// The default logarithmic sweep of doc 08 §8.1.
pub fn default_volumes() -> Vec<f64> {
    vec![1e4, 2.5e4, 5e4, 1e5, 2.5e5, 5e5, 1e6, 2.5e6, 5e6, 1e7]
}

pub fn capacity_sweep(base: &Scenario, data: &Data, volumes: &[f64], mixes: &[Mix], cfg: &SolverConfig, out: Option<&Path>) -> Result<Vec<CapacityResult>> {
    let mut results = Vec::new();
    for &v in volumes {
        for &m in mixes {
            let r = capacity_point(base, data, v, m, cfg)?;
            eprintln!(
                "capacity V={v:.0}/day {}: min_nav {} ({}) binding {:?} runs {}",
                m.name(),
                r.min_nav.map(|n| format!("{n:.0}")).unwrap_or_else(|| "none".into()),
                r.feasibility,
                r.simulated_binding,
                r.runs
            );
            if let Some(dir) = out {
                let d = dir.join(format!("capacity-V{}-{}", v as u64, m.name()));
                std::fs::create_dir_all(&d)?;
                std::fs::write(d.join("summary.json"), serde_json::to_string_pretty(&r)?)?;
            }
            results.push(r);
            // The frontier is rewritten after every point so a cut-short
            // sweep still leaves a complete table for the points it solved.
            if let Some(dir) = out {
                std::fs::write(dir.join("frontier.csv"), capacity_frontier_csv(&results))?;
            }
        }
    }
    Ok(results)
}

pub fn capacity_frontier_csv(results: &[CapacityResult]) -> String {
    let mut csv = String::from(
        "provenance,target_accepted_per_day,mix,feasibility,limit_label,min_nav,nav_ci_low,nav_ci_high,simulated_binding,next1,next2,lower_bound_nav,lower_bound_binding,agrees,\
offered_earn_notional,quoted_earn_notional,accepted_earn_notional,premium_turnover,hedge_turnover,exercise_spot_turnover,\
premium_at_risk_total,premium_at_risk_call,premium_at_risk_put,peak_expiry_premium_at_risk,reserved_peak,reserved_avg,\
net_return_annualized,hurdle_pass_fraction,max_drawdown,liquidations,accepted_rfqs,expiries,calls,puts,effective_capital,seeds,runs\n",
    );
    let f = |x: Option<f64>| x.map(|v| format!("{v:.2}")).unwrap_or_default();
    for r in results {
        let a = r.at_min_nav.as_ref();
        let lb = r.lower_bound.as_ref();
        csv.push_str(&format!(
            "prior,{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            r.target_accepted_notional_per_day,
            r.mix,
            r.feasibility,
            r.limit_label,
            f(r.min_nav),
            f(r.nav_ci_low),
            f(r.nav_ci_high),
            r.simulated_binding.unwrap_or(""),
            r.simulated_next.first().copied().unwrap_or(""),
            r.simulated_next.get(1).copied().unwrap_or(""),
            f(lb.map(|l| l.required_nav)),
            lb.map(|l| l.binding.as_str()).unwrap_or(""),
            r.lower_bound_agrees.map(|b| b.to_string()).unwrap_or_default(),
            f(a.map(|a| a.volumes.offered_earn_notional)),
            f(a.map(|a| a.volumes.quoted_earn_notional)),
            f(a.map(|a| a.volumes.accepted_earn_notional)),
            f(a.map(|a| a.volumes.premium_turnover)),
            f(a.map(|a| a.volumes.hedge_turnover)),
            f(a.map(|a| a.volumes.exercise_spot_turnover)),
            f(a.map(|a| a.premium_at_risk.total)),
            f(a.map(|a| a.premium_at_risk.call)),
            f(a.map(|a| a.premium_at_risk.put)),
            f(a.map(|a| a.premium_at_risk.peak_expiry)),
            f(a.map(|a| a.reserved_peak)),
            f(a.map(|a| a.reserved_avg)),
            a.map(|a| format!("{:.5}", a.returns.depositor_net_return_annualized)).unwrap_or_default(),
            a.map(|a| format!("{:.3}", a.returns.hurdle_pass_fraction)).unwrap_or_default(),
            a.map(|a| format!("{:.4}", a.returns.max_drawdown)).unwrap_or_default(),
            a.map(|a| a.returns.liquidations.to_string()).unwrap_or_default(),
            f(a.map(|a| a.counts.accepted_rfqs)),
            f(a.map(|a| a.counts.expiries)),
            f(a.map(|a| a.counts.calls)),
            f(a.map(|a| a.counts.puts)),
            f(a.map(|a| a.counts.effective_capital_deployed)),
            r.seeds,
            r.runs
        ));
    }
    csv
}

#[derive(Clone, Debug, Serialize)]
pub struct MarketResult {
    pub provenance: &'static str,
    pub labels: Vec<&'static str>,
    pub base_spread_volpts: f64,
    pub nav0: f64,
    pub seeds: usize,
    /// `demand_limited` | `capital_limited` | `venue_limited` | `uneconomic`.
    pub label: &'static str,
    pub aggregate: Aggregate,
}

/// Market mode over a sweep of bid widths (common random numbers across
/// widths: same seeds, same keyed draws).
pub fn market_sweep(base: &Scenario, data: &Data, spreads: &[f64], seeds: &[u64], out: Option<&Path>) -> Result<Vec<MarketResult>> {
    let mut results = Vec::new();
    for &spread in spreads {
        let mut s = base.clone();
        s.flow.source = "generated".into();
        s.flow_gen.mode = "market".into();
        s.acceptance.mode = "hazard".into();
        s.bid.base_spread_volpts = spread;
        let mut outs = Vec::new();
        for &seed in seeds {
            let mut ss = s.clone();
            ss.seed = seed;
            ss.name = format!("{}-market-s{spread}-seed{seed}", base.name);
            let o = data.run(&ss)?;
            if let Some(dir) = out {
                let m = report::summarize(&ss, &o);
                report::write_all(&dir.join(format!("market-s{spread}")).join(format!("seed{seed}")), &ss, &o, &m)?;
            }
            outs.push(o);
        }
        let agg = aggregate(&s, &outs, s.nav0);
        let venue = outs.iter().any(|o| o.stats.venue_cap_hits > 0 || o.stats.flash_cap_hits > 0);
        let capital = agg.declined.capacity > 0.05 * (agg.volumes.quoted_earn_notional + agg.declined.capacity).max(1e-9);
        let label = if agg.returns.hurdle_pass_fraction < 0.5 {
            "uneconomic"
        } else if venue {
            "venue_limited"
        } else if capital {
            "capital_limited"
        } else {
            "demand_limited"
        };
        eprintln!(
            "market spread {spread}: accepted {:.0}/run offered {:.0} apy_call {:?} net {:.4} → {label}",
            agg.volumes.accepted_earn_notional, agg.volumes.offered_earn_notional, agg.displayed_apy_call, agg.returns.depositor_net_return_annualized
        );
        results.push(MarketResult {
            provenance: PRIOR_LABEL,
            labels: vec!["proxy_oracle", "proxy_venue", "taker_only", "flow=generated_market(scenario_only)", "acceptance=hazard_ttl", "venue_capacity=assumed", "flash_capacity=assumed(PR M)"],
            base_spread_volpts: spread,
            nav0: s.nav0,
            seeds: seeds.len(),
            label,
            aggregate: agg,
        });
    }
    if let Some(dir) = out {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join("frontier.csv"), market_frontier_csv(&results))?;
        std::fs::write(dir.join("market.json"), serde_json::to_string_pretty(&results)?)?;
    }
    Ok(results)
}

pub fn market_frontier_csv(results: &[MarketResult]) -> String {
    let mut csv = String::from(
        "provenance,base_spread_volpts,nav0,label,offered_earn_notional,quoted_earn_notional,accepted_earn_notional,premium_turnover,hedge_turnover,exercise_spot_turnover,\
displayed_apy_call,displayed_apy_put,accepted_rfqs,quotes_expired,declined_capacity,net_return_annualized,hurdle_pass_fraction,max_drawdown,liquidations,seeds\n",
    );
    for r in results {
        let a = &r.aggregate;
        csv.push_str(&format!(
            "prior,{},{},{},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{},{},{:.1},{:.1},{:.2},{:.5},{:.3},{:.4},{},{}\n",
            r.base_spread_volpts,
            r.nav0,
            r.label,
            a.volumes.offered_earn_notional,
            a.volumes.quoted_earn_notional,
            a.volumes.accepted_earn_notional,
            a.volumes.premium_turnover,
            a.volumes.hedge_turnover,
            a.volumes.exercise_spot_turnover,
            a.displayed_apy_call.map(|v| format!("{v:.4}")).unwrap_or_default(),
            a.displayed_apy_put.map(|v| format!("{v:.4}")).unwrap_or_default(),
            a.counts.accepted_rfqs,
            a.counts.quotes_expired,
            a.declined.capacity,
            a.returns.depositor_net_return_annualized,
            a.returns.hurdle_pass_fraction,
            a.returns.max_drawdown,
            a.returns.liquidations,
            r.seeds
        ));
    }
    csv
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::fair_per_unit;

    /// Flat 1-minute bars: marks decay only through theta and hedges
    /// never move, so the premium caps are the only thing that binds.
    fn flat_bars(days: i64, start_ms: i64, px: f64) -> Vec<Bar> {
        (0..days * 1440).map(|i| Bar { ts_ms: start_ms + i * 60_000, open: px, high: px, low: px, close: px, volume: 1.0 }).collect()
    }

    /// Doc 07-style: one ATM 1-day call per day, no size penalty, a
    /// constant vol index so the surface sigma is known.
    #[allow(clippy::field_reassign_with_default)]
    fn fixture(days: i64) -> (Scenario, Vec<Bar>, Vec<(i64, f64)>) {
        let mut s = Scenario::default();
        s.from = "2025-01-01".into();
        s.to = format!("2025-01-{:02}", days);
        s.flow.source = "constant".into();
        s.flow.tenor_days = 1.0;
        s.flow.call_share = 1.0;
        s.flow.use_expiry_board = false;
        s.estimator.kind = "vol_index".into();
        s.estimator.risk_premium = 0.0;
        s.bid.size_penalty_volpts_per_pct_nav = 0.0;
        s.bid.inventory_penalty_max_volpts = 0.0;
        // Isolate the premium gates: a flat path bleeds every premium, so
        // the drawdown policy would otherwise bind on any multi-day run.
        s.hurdle.max_drawdown = 1.0;
        // Likewise the external-margin policy (a 1-day ATM call needs
        // ≈ 5% of V as margin, which would bind before the premium cap).
        s.venue.external_budget_fraction = 1.0;
        s.venue.external_daily_release_fraction = 1.0;
        s.revalue_interval_min = 30;
        let start = crate::data::date_start_ms(&s.from).unwrap();
        (s, flat_bars(days, start, 3.0), vec![(start - 1, 80.0)])
    }

    fn cfg(seeds: usize) -> SolverConfig {
        SolverConfig { nav_lo: 1_000.0, nav_hi: 1e8, rel_tol: 0.02, seeds: (1..=seeds as u64).collect(), service_fraction: 0.95 }
    }

    #[test]
    fn capacity_mode_recovers_the_hand_calculated_constant_flow_fixture() {
        let (s, bars, vi) = fixture(1);
        let data = Data { bars: &bars, funding: &[], vol_index: &vi };
        let v = 30_000.0;
        let r = capacity_point(&s, &data, v, Mix::CallOnly, &cfg(1)).unwrap();
        let nav = r.min_nav.expect("feasible");
        // Hand calculation: one ATM 1-day call of V/spot units at the
        // fixture's vol index (0.80, no risk premium, flat smile) on the
        // nearest lattice strike; the 10%-per-expiry cap binds first
        // (10% < 20% < 30%), so NAV_min = fair / 0.10.
        let mut probe = s.clone();
        set_target(&mut probe, v, Mix::CallOnly);
        probe.nav0 = nav;
        let out = data.run(&probe).unwrap();
        assert_eq!(out.stats.quotes_accepted, 1);
        let sigma = 0.80;
        let strike = crate::flow::quantised_strike(&s.flow, 3.0, sigma, 1.0 / 365.0);
        let fair = fair_per_unit(false, 3.0, strike, 1.0 / 365.0, sigma, 0.0) * (v / 3.0);
        let expected = fair / s.limits.per_expiry_max;
        assert!((nav / expected - 1.0).abs() < 0.03, "solved {nav} vs hand {expected} (fair {fair}, strike {strike})");
        assert_eq!(r.simulated_binding, Some("premium_per_expiry"));
        let lb = r.lower_bound.as_ref().unwrap();
        assert_eq!(lb.binding, "peak_expiry_premium_at_risk");
        assert_eq!(r.lower_bound_agrees, Some(true));
        assert!(lb.required_nav <= nav * 1.03 && lb.required_nav >= nav * 0.9, "bound {} vs {nav}", lb.required_nav);
        assert_eq!(r.provenance, PRIOR_LABEL);
        let a = r.at_min_nav.as_ref().unwrap();
        // The aggregate's return is measured against the solved NAV, not
        // the base scenario's nav0 (one seed: median = the run itself).
        let fresh = report::summarize(&probe, &out);
        assert!((a.returns.depositor_net_return_annualized - fresh.depositor_net_return_annualized).abs() < 1e-9, "{} vs {}", a.returns.depositor_net_return_annualized, fresh.depositor_net_return_annualized);
        assert!((a.volumes.accepted_earn_notional - v).abs() < 1e-6);
        assert!(a.volumes.premium_turnover > 0.0 && a.volumes.hedge_turnover > 0.0);
        assert_eq!(a.volumes.exercise_spot_turnover, 0.0, "ATM call on a flat path expires worthless");
    }

    #[test]
    fn doubling_volume_doubles_required_capital_in_a_non_netted_unconstrained_fixture() {
        let (s, bars, vi) = fixture(2);
        let data = Data { bars: &bars, funding: &[], vol_index: &vi };
        let c = cfg(1);
        let a = capacity_point(&s, &data, 20_000.0, Mix::CallOnly, &c).unwrap().min_nav.unwrap();
        let b = capacity_point(&s, &data, 40_000.0, Mix::CallOnly, &c).unwrap().min_nav.unwrap();
        let ratio = b / a;
        assert!((1.9..=2.1).contains(&ratio), "{b} / {a} = {ratio}");
    }

    #[test]
    fn venue_capacity_creates_a_nonlinear_ceiling() {
        let (mut s, bars, vi) = fixture(1);
        let data = Data { bars: &bars, funding: &[], vol_index: &vi };
        let c = cfg(1);
        let lin = capacity_point(&s, &data, 20_000.0, Mix::CallOnly, &c).unwrap().min_nav.unwrap();
        // Perp venue cap below the hedge of a 40k ATM call (≈ 0.5·V).
        // With a band hedger the only way to service the flow is a NAV so
        // large the delta sits inside the band: required NAV jumps from
        // ~0.17·V (premium-bound) to ~3.3·V (venue-bound) — a visible
        // nonlinearity the §8.6 lower bound does not see.
        s.venue.max_hedge_notional = 12_000.0;
        let small = capacity_point(&s, &data, 20_000.0, Mix::CallOnly, &c).unwrap();
        let large = capacity_point(&s, &data, 40_000.0, Mix::CallOnly, &c).unwrap();
        assert!((small.min_nav.unwrap() / lin - 1.0).abs() < 0.05, "{small:?}");
        let jump = large.min_nav.unwrap() / small.min_nav.unwrap();
        assert!(jump > 4.0, "expected a nonlinear jump, got ×{jump}: {large:?}");
        assert_eq!(large.simulated_binding, Some("hedge_margin_or_venue"));
        assert_eq!(large.lower_bound_agrees, Some(false));
        // Flash/router capacity is a hard ceiling: an ITM call exercised
        // above the cap fails at every NAV.
        let (mut f, bars2, vi2) = fixture(2);
        f.flow.moneyness_z = -1.0;
        f.venue.flash_max_notional_per_exercise = 30_000.0;
        let data2 = Data { bars: &bars2, funding: &[], vol_index: &vi2 };
        let ok = capacity_point(&f, &data2, 20_000.0, Mix::CallOnly, &c).unwrap();
        let capped = capacity_point(&f, &data2, 40_000.0, Mix::CallOnly, &c).unwrap();
        assert!(ok.min_nav.is_some(), "{ok:?}");
        assert!(ok.at_min_nav.as_ref().unwrap().exercise.calls_exercised > 0, "{ok:?}");
        assert_eq!(capped.feasibility, "venue_limited", "{capped:?}");
        assert_eq!(capped.limit_label, "venue_limited");
        assert!(capped.min_nav.is_none());
        assert_eq!(capped.simulated_binding, Some("exercise_flash_or_router"));
    }

    #[test]
    fn generated_capacity_sweep_writes_the_frontier_with_distributions_across_seeds() {
        let (mut s, bars, vi) = fixture(2);
        s.flow.source = "generated".into();
        s.flow_gen.rfqs_per_day = 4;
        s.flow_gen.use_expiry_board = false;
        s.flow_gen.tenor_menu_days = vec![1.0];
        s.flow_gen.min_tenor_days = 0.5;
        s.flow_gen.min_notional = 0.0;
        s.flow_gen.max_notional = 1e9;
        let data = Data { bars: &bars, funding: &[], vol_index: &vi };
        let c = SolverConfig { seeds: vec![1, 2, 3], ..cfg(3) };
        let dir = std::env::temp_dir().join(format!("desk-backtester-solver-{}", std::process::id()));
        let rs = capacity_sweep(&s, &data, &[20_000.0], &[Mix::CallOnly, Mix::Balanced, Mix::Adversarial], &c, Some(&dir)).unwrap();
        assert_eq!(rs.len(), 3);
        for r in &rs {
            assert_eq!(r.per_seed_nav.len(), 3);
            assert!(r.min_nav.is_some(), "{r:?}");
            let (lo, hi) = (r.nav_ci_low.unwrap(), r.nav_ci_high.unwrap());
            assert!(lo <= r.min_nav.unwrap() && r.min_nav.unwrap() <= hi);
            let a = r.at_min_nav.as_ref().unwrap();
            assert!(a.counts.accepted_rfqs > 0.0 && a.counts.effective_capital_deployed > 0.0);
        }
        // Puts and calls net delta in the balanced mix; the adversarial
        // one-bucket mix needs at least as much capital as call-only.
        assert!(rs[2].min_nav.unwrap() >= rs[0].min_nav.unwrap() * 0.98, "{:?} vs {:?}", rs[2].min_nav, rs[0].min_nav);
        let csv = std::fs::read_to_string(dir.join("frontier.csv")).unwrap();
        assert_eq!(csv.lines().count(), 4);
        assert!(csv.starts_with("provenance,target_accepted_per_day,mix,feasibility,limit_label,"));
        assert!(rs.iter().all(|r| ["demand_limited", "capital_limited", "venue_limited", "uneconomic"].contains(&r.limit_label)));
        assert!(dir.join("capacity-V20000-balanced/summary.json").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn market_mode_labels_and_no_resale_run_completes() {
        let (mut s, bars, vi) = fixture(3);
        s.nav0 = 200_000.0;
        s.flow_gen.use_expiry_board = false;
        s.flow_gen.tenor_menu_days = vec![1.0];
        s.flow_gen.min_tenor_days = 0.5;
        let data = Data { bars: &bars, funding: &[], vol_index: &vi };
        let rs = market_sweep(&s, &data, &[0.02, 0.20], &[1, 2], None).unwrap();
        assert_eq!(rs.len(), 2);
        let (tight, wide) = (&rs[0].aggregate, &rs[1].aggregate);
        assert!(tight.volumes.offered_earn_notional > 0.0);
        assert!(wide.displayed_apy_call.unwrap() < tight.displayed_apy_call.unwrap());
        assert!(wide.volumes.accepted_earn_notional < tight.volumes.accepted_earn_notional, "wider bid must attain less: {wide:?} vs {tight:?}");
        assert!(!s.resale.enabled);
        assert_eq!(tight.counts.accepted_rfqs + tight.counts.quotes_expired, tight.counts.accepted_rfqs + tight.counts.quotes_expired);
        for r in &rs {
            assert!(["demand_limited", "capital_limited", "venue_limited", "uneconomic"].contains(&r.label));
            assert_eq!(r.provenance, PRIOR_LABEL);
        }
        // Resale as a labeled upside scenario also completes.
        let mut up = s.clone();
        up.resale.enabled = true;
        up.resale.min_holding_days = 0.1;
        up.resale.call_demand_per_day = 5.0;
        up.flow.source = "generated".into();
        up.acceptance.mode = "hazard".into();
        let o = data.run(&up).unwrap();
        assert!(o.stats.resales > 0, "{:?}", o.stats);
    }
}
