//! Synthetic stress suite (doc 08 §9.5). Each case is a transformation of
//! a real path (or a synthetic path) plus scenario overrides, run through
//! the same engine, and judged against the doc 08 §0.4 limits: 15% of
//! risk NAV drawdown on the historical replay, 25% on a synthetic stress,
//! zero liquidations everywhere. Nothing here is a probability statement:
//! a stress either stays inside the limits or it does not.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::data::{Bar, FundingRow};
use crate::engine;
use crate::report;
use crate::scenario::{BasisPoint, Scenario};
use crate::study::{self, Metric};
use crate::MS_PER_DAY;

pub const HISTORICAL_DRAWDOWN_LIMIT: f64 = 0.15;
pub const STRESS_DRAWDOWN_LIMIT: f64 = 0.25;

pub struct StressCase {
    pub name: String,
    pub description: String,
    pub labels: Vec<String>,
    pub scenario: Scenario,
    pub bars: Vec<Bar>,
    pub funding: Vec<FundingRow>,
    pub limit_drawdown: f64,
}

fn scale_prices(bars: &mut [Bar], from_ms: i64, f: impl Fn(i64) -> f64) {
    for b in bars.iter_mut().filter(|b| b.ts_ms >= from_ms) {
        let k = f(b.ts_ms);
        b.open *= k;
        b.high *= k;
        b.low *= k;
        b.close *= k;
    }
}

/// Instant gap: every bar from `at` scaled by `1 + pct`.
pub fn gap(bars: &[Bar], at_ms: i64, pct: f64) -> Vec<Bar> {
    let mut v = bars.to_vec();
    scale_prices(&mut v, at_ms, |_| 1.0 + pct);
    v
}

/// Multi-step move: `step` per day for `days` days (compounded), then held.
pub fn multi_step(bars: &[Bar], at_ms: i64, days: i64, step: f64) -> Vec<Bar> {
    let mut v = bars.to_vec();
    scale_prices(&mut v, at_ms, |ts| {
        let d = ((ts - at_ms) / MS_PER_DAY + 1).min(days).max(0);
        (1.0 + step).powi(d as i32)
    });
    v
}

/// Flat market from `at` for `days`: the price pins at the last close
/// with ±0.1% deterministic wobble (so realized vol is small, not zero).
pub fn flat(bars: &[Bar], at_ms: i64, days: i64) -> Vec<Bar> {
    let px = bars.iter().rev().find(|b| b.ts_ms < at_ms).or(bars.first()).map(|b| b.close).unwrap_or(1.0);
    let end = at_ms + days * MS_PER_DAY;
    bars.iter()
        .map(|b| {
            if b.ts_ms >= at_ms && b.ts_ms < end {
                let w = 1.0 + 0.001 * ((b.ts_ms - at_ms) as f64 / 3_600_000.0).sin();
                Bar { ts_ms: b.ts_ms, open: px * w, high: px * w * 1.0005, low: px * w * 0.9995, close: px * w, volume: b.volume }
            } else if b.ts_ms >= end {
                // Re-anchor the tail so the path is continuous.
                let anchor = bars.iter().find(|x| x.ts_ms >= end).map(|x| x.close).unwrap_or(px);
                let k = px / anchor;
                Bar { ts_ms: b.ts_ms, open: b.open * k, high: b.high * k, low: b.low * k, close: b.close * k, volume: b.volume }
            } else {
                *b
            }
        })
        .collect()
}

/// Volatility collapse: log returns after `at` compressed by `factor`.
pub fn compress_vol(bars: &[Bar], at_ms: i64, factor: f64) -> Vec<Bar> {
    let mut out = Vec::with_capacity(bars.len());
    let mut prev_close: Option<f64> = None;
    let mut acc = 0.0;
    for b in bars {
        if b.ts_ms < at_ms {
            prev_close = Some(b.close);
            out.push(*b);
            continue;
        }
        let base = prev_close.unwrap_or(b.close);
        let r = (b.close / base).ln();
        acc += r * factor;
        prev_close = Some(b.close);
        let anchor = bars.iter().rev().find(|x| x.ts_ms < at_ms).map(|x| x.close).unwrap_or(b.close);
        let close = anchor * acc.exp();
        let k = close / b.close;
        out.push(Bar { ts_ms: b.ts_ms, open: b.open * k, high: b.high * k, low: b.low * k, close, volume: b.volume });
    }
    out
}

/// Funding pinned at `annual` (per year) inside `[from, to)`.
pub fn funding_pinned(funding: &[FundingRow], from_ms: i64, to_ms: i64, annual: f64) -> Vec<FundingRow> {
    funding
        .iter()
        .map(|r| {
            if r.ts_ms >= from_ms && r.ts_ms < to_ms {
                let hours = if r.interval_hours > 0.0 { r.interval_hours } else { 8.0 };
                FundingRow { ts_ms: r.ts_ms, rate: annual * hours / 8760.0, interval_hours: hours }
            } else {
                *r
            }
        })
        .collect()
}

/// The doc 08 §9.5 suite around instant `at_ms` (a date the caller picks
/// inside the window, e.g. 2025-10-10 for SUI) and the expiry the outage
/// case straddles.
pub fn suite(base: &Scenario, bars: &[Bar], funding: &[FundingRow], at_ms: i64, expiry_ms: Option<i64>) -> Result<Vec<StressCase>> {
    let expiry = expiry_ms.unwrap_or(at_ms + 7 * MS_PER_DAY);
    let mk = |name: &str, desc: &str, labels: &[&str], s: Scenario, b: Vec<Bar>, f: Vec<FundingRow>, limit: f64| StressCase {
        name: name.into(),
        description: desc.into(),
        labels: labels.iter().map(|l| l.to_string()).collect(),
        scenario: Scenario { name: format!("{}-{name}", base.name), ..s },
        bars: b,
        funding: f,
        limit_drawdown: limit,
    };
    let ov = |pairs: &[(&str, toml::Value)]| -> Result<Scenario> { base.with_overrides(&pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect::<Vec<_>>()) };
    let f = |x: f64| toml::Value::Float(x);
    let i = |x: i64| toml::Value::Integer(x);
    let day = MS_PER_DAY;
    let mut v = vec![
        mk("historical", "the untouched replay (15% drawdown limit)", &["transform=none"], base.clone(), bars.to_vec(), funding.to_vec(), HISTORICAL_DRAWDOWN_LIMIT),
        mk("gap_down_60", "instant −60% gap at the stress instant", &["transform=price×0.40"], base.clone(), gap(bars, at_ms, -0.60), funding.to_vec(), STRESS_DRAWDOWN_LIMIT),
        mk("gap_up_80", "instant +80% gap at the stress instant", &["transform=price×1.80"], base.clone(), gap(bars, at_ms, 0.80), funding.to_vec(), STRESS_DRAWDOWN_LIMIT),
        mk(
            "crash_multistep_delayed_oracle",
            "−12%/day for 5 days with the oracle proxy updating every 5 min at 60 s latency",
            &["transform=−12%/day×5", "oracle.update_ms=300000", "oracle.latency_ms=60000"],
            ov(&[("oracle.update_ms", i(300_000)), ("oracle.latency_ms", i(60_000)), ("oracle.max_age_ms", i(600_000))])?,
            multi_step(bars, at_ms, 5, -0.12),
            funding.to_vec(),
            STRESS_DRAWDOWN_LIMIT,
        ),
        mk(
            "rally_multistep_delayed_oracle",
            "+15%/day for 5 days with the oracle proxy updating every 5 min at 60 s latency",
            &["transform=+15%/day×5", "oracle.update_ms=300000", "oracle.latency_ms=60000"],
            ov(&[("oracle.update_ms", i(300_000)), ("oracle.latency_ms", i(60_000)), ("oracle.max_age_ms", i(600_000))])?,
            multi_step(bars, at_ms, 5, 0.15),
            funding.to_vec(),
            STRESS_DRAWDOWN_LIMIT,
        ),
        mk(
            "flat_six_months",
            "price pinned (±0.1%) for 183 days from the stress instant; funding zero",
            &["transform=flat×183d", "funding=0"],
            base.clone(),
            flat(bars, at_ms, 183),
            funding_pinned(funding, at_ms, at_ms + 183 * day, 0.0),
            STRESS_DRAWDOWN_LIMIT,
        ),
        mk(
            "vol_collapse_after_purchase",
            "log returns compressed ×0.25 from one day after the stress instant",
            &["transform=returns×0.25"],
            base.clone(),
            compress_vol(bars, at_ms + day, 0.25),
            funding.to_vec(),
            STRESS_DRAWDOWN_LIMIT,
        ),
        mk("funding_plus_50", "+50% annualized funding for 30 days (shorts receive, longs pay)", &["funding=+0.50/yr×30d"], base.clone(), bars.to_vec(), funding_pinned(funding, at_ms, at_ms + 30 * day, 0.50), STRESS_DRAWDOWN_LIMIT),
        mk("funding_minus_50", "−50% annualized funding for 30 days (shorts pay, longs receive)", &["funding=−0.50/yr×30d"], base.clone(), bars.to_vec(), funding_pinned(funding, at_ms, at_ms + 30 * day, -0.50), STRESS_DRAWDOWN_LIMIT),
        mk(
            "venue_outage_exercise_margin",
            "Bluefin outage 12 h before to 36 h after the straddled expiry while the path gaps −25% at the outage start",
            &["margin.outages=[expiry−12h, expiry+36h]", "transform=price×0.75@outage"],
            {
                let mut s = base.clone();
                s.margin.outages = vec![[expiry - 12 * 3_600_000, expiry + 36 * 3_600_000]];
                s
            },
            gap(bars, expiry - 12 * 3_600_000, -0.25),
            funding.to_vec(),
            STRESS_DRAWDOWN_LIMIT,
        ),
        mk(
            "sui_congestion_near_expiry",
            "Sui inclusion 10 min ± 5 min, detection 2 min, 20% PTB failure — applied to the whole run (a conservative superset of 'near expiry')",
            &["latency.sui_inclusion=600000±300000", "latency.indexer_detection=120000", "exercise.ptb_failure_prob=0.2", "scope=whole_run(conservative)"],
            {
                let mut s = base.clone();
                s.latency.sui_inclusion = crate::latency::LatencyDist { mean_ms: 600_000, jitter_ms: 300_000, assumed: true };
                s.latency.indexer_detection = crate::latency::LatencyDist { mean_ms: 120_000, jitter_ms: 60_000, assumed: true };
                s.exercise.ptb_failure_prob = 0.2;
                s
            },
            bars.to_vec(),
            funding.to_vec(),
            STRESS_DRAWDOWN_LIMIT,
        ),
        mk("no_resale", "resale disabled (hold to exercise/expiry)", &["resale.enabled=false"], ov(&[("resale.enabled", toml::Value::Boolean(false))])?, bars.to_vec(), funding.to_vec(), STRESS_DRAWDOWN_LIMIT),
        mk("no_base_flash", "DeepBook pool holds no base: put exercise falls through to the quote flash or fails", &["exercise.pool_base_balance_units=0"], ov(&[("exercise.pool_base_balance_units", f(0.0))])?, bars.to_vec(), funding.to_vec(), STRESS_DRAWDOWN_LIMIT),
        mk("no_quote_flash", "DeepBook pool holds no quote: call exercise is cash-only, puts lose the last fallback", &["exercise.pool_quote_balance=0"], ov(&[("exercise.pool_quote_balance", f(0.0))])?, bars.to_vec(), funding.to_vec(), STRESS_DRAWDOWN_LIMIT),
        mk(
            "router_depth_collapse",
            "route depth ÷ 20 (each bp of impact absorbs 1/20 of the units)",
            &["exercise.route_depth_units_per_bps=÷20"],
            ov(&[("exercise.route_depth_units_per_bps", f(base.exercise.route_depth_units_per_bps / 20.0))])?,
            bars.to_vec(),
            funding.to_vec(),
            STRESS_DRAWDOWN_LIMIT,
        ),
        mk(
            "concentrated_expiry",
            "every writer herds into the nearest listed expiry and the per-expiry cap is lifted to the total budget",
            &["flow_gen.herd_prob=1", "flow_gen.expiry_concentration=1", "limits.per_expiry_max=premium_budget_hard"],
            {
                let mut s = base.clone();
                s.flow_gen.herd_prob = 1.0;
                s.flow_gen.expiry_concentration = 1.0;
                s.flow.use_expiry_board = true;
                s.limits.per_expiry_max = s.limits.premium_budget_hard;
                s
            },
            bars.to_vec(),
            funding.to_vec(),
            STRESS_DRAWDOWN_LIMIT,
        ),
        mk(
            "settlement_depeg",
            "settlement stablecoin −3% against the perp quote for 7 days, modeled as a −300 bp mark basis (doc 08 §7.4 basis series)",
            &["venue.basis=−300bps×7d", "depeg=basis_series_proxy"],
            {
                let mut s = base.clone();
                s.venue.basis = vec![BasisPoint { from_ms: at_ms, bps: -300.0 }, BasisPoint { from_ms: at_ms + 7 * day, bps: 0.0 }];
                s
            },
            bars.to_vec(),
            funding.to_vec(),
            STRESS_DRAWDOWN_LIMIT,
        ),
    ];
    for c in &mut v {
        c.labels.push("proxy_oracle".into());
        c.labels.push("proxy_venue".into());
    }
    Ok(v)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StressResult {
    pub name: String,
    pub description: String,
    pub labels: Vec<String>,
    pub limit_drawdown: f64,
    pub metric: Metric,
    pub drawdown_pass: bool,
    pub liquidation_pass: bool,
    pub pass: bool,
    /// Change of NAV end against the historical case.
    pub nav_end_vs_historical: f64,
    /// Minimum margin headroom `(MR − MMR)/MMR` seen (None = no perp).
    pub closest_margin_headroom: Option<f64>,
}

pub fn run_suite(cases: Vec<StressCase>, vol_index: &[(i64, f64)], out: Option<&std::path::Path>, threads: usize) -> Result<Vec<StressResult>> {
    let runs = study::par_map(cases, threads, |c| -> Result<(StressCase, engine::RunOutput)> {
        let o = engine::run(&c.scenario, &c.bars, &c.funding, vol_index)?;
        Ok((c, o))
    });
    let mut results = Vec::new();
    let mut hist_nav = None;
    for r in runs {
        let (c, o) = r?;
        let m = Metric::from_run(&c.scenario, &o);
        if let Some(dir) = out {
            let s = report::summarize(&c.scenario, &o);
            report::write_all(&dir.join(&c.name), &c.scenario, &o, &s)?;
        }
        if c.name == "historical" {
            hist_nav = Some(m.nav_end);
        }
        let dd_pass = m.max_drawdown <= c.limit_drawdown && !m.bankrupt;
        let liq_pass = m.liquidations == 0;
        eprintln!("stress {:32} nav_end {:>12.0} dd {:.3} (limit {:.2}) liq {} {}", c.name, m.nav_end, m.max_drawdown, c.limit_drawdown, m.liquidations, if dd_pass && liq_pass { "PASS" } else { "FAIL" });
        results.push(StressResult {
            name: c.name,
            description: c.description,
            labels: c.labels,
            limit_drawdown: c.limit_drawdown,
            closest_margin_headroom: m.closest_margin_headroom,
            drawdown_pass: dd_pass,
            liquidation_pass: liq_pass,
            pass: dd_pass && liq_pass,
            nav_end_vs_historical: 0.0,
            metric: m,
        });
    }
    if let Some(h) = hist_nav {
        for r in &mut results {
            r.nav_end_vs_historical = r.metric.nav_end - h;
        }
    }
    if let Some(dir) = out {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join("stress.json"), serde_json::to_string_pretty(&results)?)?;
        std::fs::write(dir.join("stress.csv"), csv(&results))?;
    }
    Ok(results)
}

pub fn csv(results: &[StressResult]) -> String {
    let mut s = String::from("case,limit_drawdown,nav_end,nav_end_vs_historical,depositor_net_return_annualized,max_drawdown,drawdown_pass,liquidations,liquidation_pass,closest_margin_headroom,bankrupt,fills,exercise_cost,option_payoff,hedge_realized,funding_paid,pass,labels\n");
    for r in results {
        let m = &r.metric;
        s.push_str(&format!(
            "{},{},{:.2},{:.2},{:.5},{:.4},{},{},{},{},{},{},{:.2},{:.2},{:.2},{:.2},{},{}\n",
            r.name,
            r.limit_drawdown,
            m.nav_end,
            r.nav_end_vs_historical,
            m.depositor_net_return_annualized,
            m.max_drawdown,
            r.drawdown_pass,
            m.liquidations,
            r.liquidation_pass,
            r.closest_margin_headroom.map(|h| format!("{h:.4}")).unwrap_or_default(),
            m.bankrupt,
            m.fills,
            m.exercise_cost,
            m.option_payoff,
            m.hedge_realized,
            m.funding_paid,
            r.pass,
            r.labels.join("|")
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::synthetic_bars;

    #[test]
    fn transforms_do_what_they_say() {
        let start = crate::data::date_start_ms("2025-01-01").unwrap();
        let bars = synthetic_bars(10, start);
        let at = start + 3 * MS_PER_DAY;
        let g = gap(&bars, at, -0.60);
        let i = bars.iter().position(|b| b.ts_ms == at).unwrap();
        assert!((g[i].close / bars[i].close - 0.4).abs() < 1e-12);
        assert_eq!(g[i - 1].close, bars[i - 1].close);
        let m = multi_step(&bars, at, 5, -0.12);
        let j = bars.iter().position(|b| b.ts_ms == at + 6 * MS_PER_DAY).unwrap();
        assert!((m[j].close / bars[j].close - 0.88f64.powi(5)).abs() < 1e-9);
        let fl = flat(&bars, at, 4);
        let k = bars.iter().position(|b| b.ts_ms == at + MS_PER_DAY).unwrap();
        let pinned = bars[i - 1].close;
        assert!((fl[k].close / pinned - 1.0).abs() < 0.002);
        let c = compress_vol(&bars, at, 0.25);
        let rv = |v: &[Bar]| v[i..].windows(2).map(|w| (w[1].close / w[0].close).ln().powi(2)).sum::<f64>().sqrt();
        assert!((rv(&c) / rv(&bars) - 0.25).abs() < 0.05, "{}", rv(&c) / rv(&bars));
        let f: Vec<FundingRow> = (0..30).map(|n| FundingRow { ts_ms: start + n * 8 * 3_600_000, rate: 0.0001, interval_hours: 8.0 }).collect();
        let p = funding_pinned(&f, at, at + 2 * MS_PER_DAY, 0.5);
        let inside = p.iter().filter(|r| r.ts_ms >= at && r.ts_ms < at + 2 * MS_PER_DAY).count();
        assert_eq!(inside, 6);
        assert!(p.iter().filter(|r| r.ts_ms >= at && r.ts_ms < at + 2 * MS_PER_DAY).all(|r| (r.rate - 0.5 * 8.0 / 8760.0).abs() < 1e-12));
    }

    /// Every doc 08 §9.5 case builds, runs through the engine and is
    /// judged against the stated limits; the −60% gap on a put book is a
    /// liquidation (fails), the flat market is not.
    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn suite_runs_every_case_and_judges_the_limits() {
        let mut s = Scenario::default();
        s.from = "2025-01-01".into();
        s.to = "2025-02-05".into();
        s.flow.tenor_days = 14.0;
        s.flow.call_share = 0.0;
        s.limits.put_premium_max = 0.30;
        s.limits.per_expiry_max = 0.30;
        s.bid.size_penalty_volpts_per_pct_nav = 0.0;
        s.latency = crate::latency::LatencyConfig::zero();
        s.revalue_interval_min = 30;
        let start = crate::data::date_start_ms(&s.from).unwrap();
        let bars = synthetic_bars(36, start);
        let funding: Vec<FundingRow> = (0..108).map(|n| FundingRow { ts_ms: start + n * 8 * 3_600_000, rate: 0.0001, interval_hours: 8.0 }).collect();
        let cases = suite(&s, &bars, &funding, start + 7 * MS_PER_DAY, Some(start + 14 * MS_PER_DAY)).unwrap();
        assert_eq!(cases.len(), 17);
        let dir = std::env::temp_dir().join(format!("desk-backtester-stress-{}", std::process::id()));
        let rs = run_suite(cases, &[], Some(&dir), 4).unwrap();
        assert_eq!(rs.len(), 17);
        let by = |n: &str| rs.iter().find(|r| r.name == n).unwrap();
        assert_eq!(by("historical").limit_drawdown, HISTORICAL_DRAWDOWN_LIMIT);
        assert!(by("gap_down_60").metric.liquidations > 0 && !by("gap_down_60").pass, "{:?}", by("gap_down_60").metric);
        assert!(by("flat_six_months").liquidation_pass);
        assert!(rs.iter().all(|r| r.metric.reconciliation_gap.abs() < 1e-6 && r.metric.perp_identity_gap.abs() < 1e-6 && r.metric.option_identity_gap.abs() < 1e-6), "{:?}", rs.iter().map(|r| (r.name.clone(), r.metric.reconciliation_gap, r.metric.perp_identity_gap)).collect::<Vec<_>>());
        assert!(rs.iter().all(|r| r.labels.contains(&"proxy_venue".to_string())));
        assert!(rs.iter().all(|r| r.metric.labels.iter().any(|l| l.starts_with("execution="))));
        let csv = std::fs::read_to_string(dir.join("stress.csv")).unwrap();
        assert_eq!(csv.lines().count(), 18);
        assert!(dir.join("stress.json").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
