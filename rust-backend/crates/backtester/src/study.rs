//! Shared statistical output (doc 08 §9.6): the per-run metric row every
//! runner (walk-forward, grid, stress, capacity) reports, distributions
//! across flow seeds (mean, median, quantiles, a t-interval, CVaR), and a
//! small work-stealing parallel map for independent runs.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::engine::RunOutput;
use crate::report::{self, Summary};
use crate::scenario::Scenario;

/// One run, flattened. Every field a table in doc 11 needs.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Metric {
    pub name: String,
    pub seed: u64,
    pub from: String,
    pub to: String,
    pub nav0: f64,
    pub nav_end: f64,
    pub desk_gross_return: f64,
    pub desk_gross_return_annualized: f64,
    pub depositor_net_return_annualized: f64,
    pub net_return_after_idle_cost_annualized: f64,
    pub required_return: f64,
    pub hurdle_pass: bool,
    pub max_drawdown: f64,
    pub drawdown_pass: bool,
    /// CVaR-95 of daily NAV returns (mean of the worst 5% days).
    pub daily_cvar95: f64,
    pub liquidations: u64,
    pub liquidation_loss: f64,
    pub closest_margin_headroom: Option<f64>,
    pub bankrupt: bool,
    pub fills: u64,
    pub fills_call: u64,
    pub fills_put: u64,
    pub expiries: u64,
    pub independent_expiries: u64,
    pub declines_capacity: u64,
    pub offered_notional: f64,
    pub quoted_notional: f64,
    pub accepted_notional: f64,
    pub premium_turnover: f64,
    pub hedge_turnover: f64,
    pub exercise_spot_turnover: f64,
    pub premium_paid: f64,
    pub option_payoff: f64,
    pub hedge_realized: f64,
    pub funding_paid: f64,
    pub funding_paid_long: f64,
    pub funding_paid_short: f64,
    pub hedge_fees: f64,
    pub maker_fees: f64,
    pub hedge_slippage: f64,
    pub gas: f64,
    pub exercise_cost: f64,
    /// Non-realized (doc 08 §2.3).
    pub model_edge_at_entry: f64,
    pub option_mtm: f64,
    pub exit_vs_mark: f64,
    pub delta_explained: f64,
    pub gamma_explained: f64,
    pub theta_explained: f64,
    pub vega_explained: f64,
    pub basis_explained: f64,
    pub explanation_residual: f64,
    pub idle_cash_cost: f64,
    pub hedge_turnover_nav_per_30d: f64,
    pub cost_per_30d_pct_nav: f64,
    pub mean_sigma_paid: f64,
    pub mean_sigma_realized: f64,
    pub mean_vol_bias: f64,
    pub coverage: f64,
    pub invalidated_spans: usize,
    pub net_profit_per_accepted_notional: Option<f64>,
    pub return_on_peak_capital: Option<f64>,
    pub peak_capital_deployed: f64,
    pub margin_topups: u64,
    pub topup_declines: u64,
    pub topup_rejects: u64,
    /// `Σ ledger lines − ΔNAV` over the run (attribution cumulative window).
    pub reconciliation_gap: f64,
    pub option_identity_gap: f64,
    pub perp_identity_gap: f64,
    pub labels: Vec<String>,
    pub determinism_hash: String,
}

/// Every proxy / queue / latency / resale / flow assumption as a label.
pub fn labels(m: &Summary) -> Vec<String> {
    let l = &m.labels;
    let lat = &m.latency_assumptions;
    let mut v = vec![
        format!("proxy_oracle={}", l.proxy_oracle),
        format!("proxy_venue={}", l.proxy_venue),
        format!("execution={}", m.execution_assumption),
        format!("resale={}", l.resale),
        format!("flow={}", l.flow_source),
        format!("acceptance={}", l.acceptance),
        format!("flow_provenance={}", l.flow_provenance),
        format!("estimator={}", l.estimator),
        format!("exercise={}", l.exercise),
        format!("margin_model={}", m.margin_model),
        format!("flash_capacity_assumed={}", m.flash_capacity_assumed),
        format!("gap_policy={}", m.gap_policy),
        format!(
            "latency_assumed={}",
            [lat.observation.assumed, lat.strategy.assumed, lat.venue_submit.assumed, lat.venue_fill_report.assumed, lat.sui_inclusion.assumed, lat.indexer_detection.assumed]
                .iter()
                .any(|a| *a)
        ),
        format!("sui_inclusion_ms={}", lat.sui_inclusion.mean_ms),
        format!("basis_configured={}", m.venue_labels.basis_configured),
    ];
    if m.bankrupt_ms.is_some() {
        v.push("bankrupt=true".into());
    }
    v
}

/// CVaR-95 of daily returns along the NAV path.
pub fn daily_cvar95(out: &RunOutput) -> f64 {
    let mut r: Vec<f64> = out.nav_path.windows(2).filter(|w| w[0].nav > 0.0).map(|w| w[1].nav / w[0].nav - 1.0).collect();
    if r.is_empty() {
        return 0.0;
    }
    r.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let k = ((r.len() as f64) * 0.05).ceil().max(1.0) as usize;
    r[..k].iter().sum::<f64>() / k as f64
}

impl Metric {
    pub fn from_run(s: &Scenario, out: &RunOutput) -> Metric {
        let m = report::summarize(s, out);
        let a = &m.attribution;
        let days = out.minutes_total as f64 / 1440.0;
        let cost = m.hedge_fees + m.maker_fees + m.hedge_slippage + m.gas;
        let expiries: std::collections::BTreeSet<i64> = out.settled.iter().map(|o| o.expiry_ms).collect();
        let attr = crate::attribution::report(s, out);
        Metric {
            name: s.name.clone(),
            seed: s.seed,
            from: s.from.clone(),
            to: s.to.clone(),
            nav0: s.nav0,
            nav_end: m.nav_end,
            desk_gross_return: m.desk_gross_return,
            desk_gross_return_annualized: m.desk_gross_return_annualized,
            depositor_net_return_annualized: m.depositor_net_return_annualized,
            net_return_after_idle_cost_annualized: m.net_return_after_idle_cost_annualized,
            required_return: m.required_return,
            hurdle_pass: m.hurdle_pass,
            max_drawdown: m.max_drawdown,
            drawdown_pass: m.drawdown_pass,
            daily_cvar95: daily_cvar95(out),
            liquidations: m.liquidations,
            liquidation_loss: m.liquidation_loss,
            closest_margin_headroom: m.closest_margin_headroom,
            bankrupt: m.bankrupt_ms.is_some(),
            fills: m.fills,
            fills_call: out.stats.fills_call,
            fills_put: out.stats.fills_put,
            expiries: out.stats.expiries_settled,
            independent_expiries: expiries.len() as u64,
            declines_capacity: m.declines_capacity,
            offered_notional: out.stats.volumes.offered_earn_notional,
            quoted_notional: out.stats.volumes.quoted_earn_notional,
            accepted_notional: out.stats.volumes.accepted_earn_notional,
            premium_turnover: out.stats.volumes.premium_turnover,
            hedge_turnover: out.stats.volumes.hedge_turnover,
            exercise_spot_turnover: out.stats.volumes.exercise_spot_turnover,
            premium_paid: m.premium_paid,
            option_payoff: m.option_payoff,
            hedge_realized: m.hedge_realized,
            funding_paid: m.funding_paid,
            funding_paid_long: a.funding_paid_long,
            funding_paid_short: a.funding_paid_short,
            hedge_fees: m.hedge_fees,
            maker_fees: m.maker_fees,
            hedge_slippage: m.hedge_slippage,
            gas: m.gas,
            exercise_cost: a.exercise_cost,
            model_edge_at_entry: a.model_edge(),
            option_mtm: a.option_mtm(),
            exit_vs_mark: a.exit_vs_mark(),
            delta_explained: a.option_delta + a.perp_delta,
            gamma_explained: a.option_gamma,
            theta_explained: a.option_theta,
            vega_explained: a.option_vega,
            basis_explained: a.perp_basis,
            explanation_residual: a.option_residual + a.perp_residual,
            idle_cash_cost: a.idle_cash_cost,
            hedge_turnover_nav_per_30d: m.hedge_turnover_nav_per_30d,
            cost_per_30d_pct_nav: if days > 0.0 { cost / s.nav0 / (days / 30.0) } else { 0.0 },
            mean_sigma_paid: m.mean_sigma_paid,
            mean_sigma_realized: m.mean_sigma_realized,
            mean_vol_bias: m.mean_vol_bias,
            coverage: m.coverage,
            invalidated_spans: m.invalidated_spans.len(),
            net_profit_per_accepted_notional: m.net_profit_per_accepted_notional,
            return_on_peak_capital: m.return_on_peak_capital,
            peak_capital_deployed: out.stats.peak_capital_deployed,
            margin_topups: m.margin_topups,
            topup_declines: m.topup_declines,
            topup_rejects: m.topup_rejects,
            reconciliation_gap: attr.as_ref().map(|a| a.cumulative.reconciliation_gap).unwrap_or(0.0),
            option_identity_gap: attr.as_ref().map(|a| a.option_identity_gap).unwrap_or(0.0),
            perp_identity_gap: attr.as_ref().map(|a| a.perp_identity_gap).unwrap_or(0.0),
            labels: labels(&m),
            determinism_hash: m.determinism_hash.clone(),
        }
    }
}

/// Distribution of one quantity across seeds (or folds).
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SeedStats {
    pub n: usize,
    pub mean: f64,
    pub median: f64,
    pub sd: f64,
    pub min: f64,
    pub max: f64,
    pub q05: f64,
    pub q25: f64,
    pub q75: f64,
    pub q95: f64,
    /// Student-t interval on the mean (small n ⇒ wide; None when n < 2).
    pub ci95_low: Option<f64>,
    pub ci95_high: Option<f64>,
    /// Mean of the values at or below the 5% quantile (lower tail).
    pub cvar05: f64,
}

fn t975(n: usize) -> f64 {
    match n {
        0 | 1 => f64::NAN,
        2 => 12.706,
        3 => 4.303,
        4 => 3.182,
        5 => 2.776,
        6 => 2.571,
        7 => 2.447,
        8 => 2.365,
        9 => 2.306,
        10 => 2.262,
        11..=15 => 2.145,
        16..=20 => 2.093,
        21..=30 => 2.045,
        _ => 1.96,
    }
}

pub fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let pos = q * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
}

pub fn seed_stats(values: &[f64]) -> SeedStats {
    let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return SeedStats::default();
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    let mean = v.iter().sum::<f64>() / n as f64;
    let sd = if n > 1 { (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt() } else { 0.0 };
    let half = if n >= 2 { Some(t975(n) * sd / (n as f64).sqrt()) } else { None };
    let k = ((n as f64) * 0.05).ceil().max(1.0) as usize;
    SeedStats {
        n,
        mean,
        median: quantile(&v, 0.5),
        sd,
        min: v[0],
        max: v[n - 1],
        q05: quantile(&v, 0.05),
        q25: quantile(&v, 0.25),
        q75: quantile(&v, 0.75),
        q95: quantile(&v, 0.95),
        ci95_low: half.map(|h| mean - h),
        ci95_high: half.map(|h| mean + h),
        cvar05: v[..k].iter().sum::<f64>() / k as f64,
    }
}

/// Across-seed summary of the doc 08 §9.6 lines.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Distribution {
    pub seeds: usize,
    pub depositor_net_return_annualized: SeedStats,
    pub net_return_after_idle_cost_annualized: SeedStats,
    pub max_drawdown: SeedStats,
    pub daily_cvar95: SeedStats,
    pub fills: SeedStats,
    pub independent_expiries: SeedStats,
    pub accepted_notional: SeedStats,
    pub liquidation_count_total: u64,
    pub liquidation_probability: f64,
    pub closest_margin_headroom: Option<f64>,
    pub bankrupt_fraction: f64,
    pub hurdle_pass_fraction: f64,
    pub required_return: f64,
    /// Doc 08 §12 item 8: the LOWER confidence bound clears the hurdle.
    pub lower_ci_clears_hurdle: bool,
    pub median_clears_hurdle: bool,
    pub max_drawdown_worst: f64,
}

pub fn distribution(ms: &[Metric]) -> Distribution {
    let f = |g: &dyn Fn(&Metric) -> f64| seed_stats(&ms.iter().map(g).collect::<Vec<_>>());
    let ret = f(&|m| m.depositor_net_return_annualized);
    let required = ms.first().map(|m| m.required_return).unwrap_or(0.0);
    let n = ms.len().max(1) as f64;
    Distribution {
        seeds: ms.len(),
        median_clears_hurdle: ret.median >= required,
        lower_ci_clears_hurdle: ret.ci95_low.is_some_and(|l| l >= required),
        depositor_net_return_annualized: ret,
        net_return_after_idle_cost_annualized: f(&|m| m.net_return_after_idle_cost_annualized),
        max_drawdown: f(&|m| m.max_drawdown),
        daily_cvar95: f(&|m| m.daily_cvar95),
        fills: f(&|m| m.fills as f64),
        independent_expiries: f(&|m| m.independent_expiries as f64),
        accepted_notional: f(&|m| m.accepted_notional),
        liquidation_count_total: ms.iter().map(|m| m.liquidations).sum(),
        liquidation_probability: ms.iter().filter(|m| m.liquidations > 0).count() as f64 / n,
        closest_margin_headroom: ms.iter().filter_map(|m| m.closest_margin_headroom).fold(None, |a: Option<f64>, h| Some(a.map_or(h, |x| x.min(h)))),
        bankrupt_fraction: ms.iter().filter(|m| m.bankrupt).count() as f64 / n,
        hurdle_pass_fraction: ms.iter().filter(|m| m.hurdle_pass).count() as f64 / n,
        required_return: required,
        max_drawdown_worst: ms.iter().map(|m| m.max_drawdown).fold(0.0, f64::max),
    }
}

/// Doc 08 §9.6 / P5 gate: which limit binds a result.
/// `demand_limited | capital_limited | venue_limited | uneconomic`.
pub fn limit_label(m: &Metric, hurdle_pass: bool, venue_hit: bool) -> &'static str {
    if !hurdle_pass || m.bankrupt {
        "uneconomic"
    } else if venue_hit {
        "venue_limited"
    } else if m.declines_capacity > 0 && m.declines_capacity as f64 > 0.05 * m.fills.max(1) as f64 {
        "capital_limited"
    } else {
        "demand_limited"
    }
}

/// Run independent jobs on every core; results in input order.
pub fn par_map<T: Send, R: Send>(items: Vec<T>, threads: usize, f: impl Fn(T) -> R + Sync) -> Vec<R> {
    let n = items.len();
    let queue: Mutex<std::collections::VecDeque<(usize, T)>> = Mutex::new(items.into_iter().enumerate().collect());
    let results: Mutex<Vec<Option<R>>> = Mutex::new((0..n).map(|_| None).collect());
    let workers = threads.max(1).min(n.max(1));
    std::thread::scope(|sc| {
        for _ in 0..workers {
            sc.spawn(|| loop {
                let next = queue.lock().expect("queue").pop_front();
                let Some((i, item)) = next else { break };
                let r = f(item);
                results.lock().expect("results")[i] = Some(r);
            });
        }
    });
    results.into_inner().expect("results").into_iter().map(|r| r.expect("every job ran")).collect()
}

pub fn default_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_stats_quantiles_ci_and_cvar_by_hand() {
        let s = seed_stats(&[0.10, 0.20, 0.30, 0.40]);
        assert_eq!(s.n, 4);
        assert!((s.mean - 0.25).abs() < 1e-12);
        assert!((s.median - 0.25).abs() < 1e-12);
        assert!((s.q25 - 0.175).abs() < 1e-12);
        assert!((s.cvar05 - 0.10).abs() < 1e-12);
        let sd = (((0.15f64).powi(2) * 2.0 + (0.05f64).powi(2) * 2.0) / 3.0).sqrt();
        assert!((s.sd - sd).abs() < 1e-12);
        assert!((s.ci95_low.unwrap() - (0.25 - 3.182 * sd / 2.0)).abs() < 1e-9);
        assert!(seed_stats(&[0.1]).ci95_low.is_none());
        assert_eq!(seed_stats(&[]).n, 0);
    }

    #[test]
    fn distribution_reports_lower_bound_and_liquidation_probability() {
        let mut ms: Vec<Metric> = (0..4)
            .map(|i| Metric { depositor_net_return_annualized: 0.30 + 0.01 * i as f64, required_return: 0.12, hurdle_pass: true, ..Default::default() })
            .collect();
        ms[3].liquidations = 2;
        let d = distribution(&ms);
        assert!(d.lower_ci_clears_hurdle && d.median_clears_hurdle);
        assert_eq!(d.liquidation_count_total, 2);
        assert!((d.liquidation_probability - 0.25).abs() < 1e-12);
        let wide: Vec<Metric> = [0.5, -0.4].iter().map(|r| Metric { depositor_net_return_annualized: *r, required_return: 0.12, ..Default::default() }).collect();
        assert!(!distribution(&wide).lower_ci_clears_hurdle);
    }

    #[test]
    fn par_map_preserves_order() {
        let out = par_map((0..50).collect(), 4, |i: i32| i * i);
        assert_eq!(out, (0..50).map(|i| i * i).collect::<Vec<_>>());
    }

    #[test]
    fn limit_labels_cover_the_four_cases() {
        let m = Metric::default();
        assert_eq!(limit_label(&m, false, false), "uneconomic");
        assert_eq!(limit_label(&m, true, true), "venue_limited");
        assert_eq!(limit_label(&Metric { declines_capacity: 5, fills: 10, ..Default::default() }, true, false), "capital_limited");
        assert_eq!(limit_label(&m, true, false), "demand_limited");
    }
}
