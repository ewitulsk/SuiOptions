//! Declared-grid sweeps (doc 08 §9.3 portfolio variants, §9.4 sweep
//! axes) with common random numbers: the same flow seeds at every point,
//! so a difference between two points is the parameter, not the draw.
//! The output is a break-even surface — every point with its across-seed
//! distribution and whether it clears the predeclared policy — never a
//! single optimum.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::data::{Bar, FundingRow};
use crate::engine;
use crate::scenario::Scenario;
use crate::study::{self, Distribution, Metric};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Axis {
    /// Dotted scenario path (any `[section].field`), e.g. the doc 08 §9.4
    /// axes: `flow_gen.target_notional_per_day`, `flow_gen.call_share`,
    /// `flow_gen.call.size_log_sd`, `flow_gen.expiry_concentration`,
    /// `acceptance.call.apy_elasticity`, `bid.base_spread_volpts`,
    /// `estimator.skew`, `hedge.band_pct_nav`, `venue.execution_assumption`,
    /// `flow_gen.tenor_menu_days`, `resale.enabled`, `limits.premium_budget_hard`,
    /// `latency.sui_inclusion`, `estimator.q_bid`, `margin.topup_trigger_mr`.
    pub path: String,
    pub values: Vec<toml::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct GridConfig {
    pub name: String,
    pub scenario: String,
    pub seeds: Vec<u64>,
    pub axes: Vec<Axis>,
    /// Named portfolio variants (doc 08 §9.3) applied as an extra axis.
    pub mixes: Vec<String>,
}

impl Default for GridConfig {
    fn default() -> Self {
        Self { name: "grid".into(), scenario: String::new(), seeds: vec![1, 2], axes: Vec::new(), mixes: Vec::new() }
    }
}

impl GridConfig {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let c: Self = toml::from_str(&text)?;
        anyhow::ensure!(!c.axes.is_empty() || !c.mixes.is_empty(), "grid has no axes");
        anyhow::ensure!(!c.seeds.is_empty(), "grid has no seeds");
        Ok(c)
    }
}

/// Doc 08 §9.3: the required portfolio variants as flow overrides. The
/// share drives the constant injector and capacity mode directly; in
/// market mode arrivals come from the per-type base rates, so those are
/// rescaled to the share at the same total intensity.
pub fn apply_mix(s: &mut Scenario, mix: &str) -> Result<()> {
    let g = &mut s.flow_gen;
    let prior = g.call.base_rate_per_day / (g.call.base_rate_per_day + g.put.base_rate_per_day).max(1e-9);
    let share = match mix {
        "call_only" => 1.0,
        "put_only" => 0.0,
        "balanced" => 0.5,
        // No RFQ history exists (doc 08 §3.1): the "historically
        // calibrated" mix is the stated prior's base rates.
        "prior_calibrated" => prior,
        "call_heavy" => 0.85,
        "put_heavy" => 0.15,
        other => anyhow::bail!("unknown mix {other} (call_only|put_only|balanced|prior_calibrated|call_heavy|put_heavy)"),
    };
    let total = g.call.base_rate_per_day + g.put.base_rate_per_day;
    g.call_share = share;
    g.call.base_rate_per_day = total * share;
    g.put.base_rate_per_day = total * (1.0 - share);
    s.flow.call_share = share;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GridPoint {
    pub index: usize,
    /// `path=value` for every axis (and `mix=…`).
    pub coordinates: Vec<String>,
    pub seeds: Vec<Metric>,
    pub distribution: Distribution,
    /// Median depositor-net return ≥ hurdle, worst drawdown ≤ policy,
    /// zero liquidations, nobody bankrupt.
    pub break_even: bool,
    /// Which policy line fails first (empty when break-even).
    pub binding: String,
    pub limit_label: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AxisSensitivity {
    pub axis: String,
    pub values: Vec<String>,
    /// Median depositor-net return at each value, other axes at their
    /// first (base) value.
    pub median_returns: Vec<f64>,
    pub break_even: Vec<bool>,
    pub range: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GridReport {
    pub name: String,
    pub scenario: String,
    pub from: String,
    pub to: String,
    pub seeds: Vec<u64>,
    pub axes: Vec<String>,
    pub points: Vec<GridPoint>,
    pub sensitivity: Vec<AxisSensitivity>,
    pub break_even_count: usize,
    pub labels: Vec<String>,
}

fn value_str(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// One grid point: `(dotted path, value)` per axis, `mix` last.
pub type Coord = Vec<(String, toml::Value)>;

/// Cartesian product of the axes (mix last).
fn coordinates(cfg: &GridConfig) -> Vec<Coord> {
    let mut pts: Vec<Coord> = vec![Vec::new()];
    for ax in &cfg.axes {
        pts = pts.into_iter().flat_map(|p| ax.values.iter().map(move |v| { let mut q = p.clone(); q.push((ax.path.clone(), v.clone())); q })).collect();
    }
    if !cfg.mixes.is_empty() {
        pts = pts.into_iter().flat_map(|p| cfg.mixes.iter().map(move |m| { let mut q = p.clone(); q.push(("mix".into(), toml::Value::String(m.clone()))); q })).collect();
    }
    pts
}

pub fn scenario_at(base: &Scenario, coord: &[(String, toml::Value)], seed: u64) -> Result<Scenario> {
    let overrides: Vec<(String, toml::Value)> = coord.iter().filter(|(k, _)| k != "mix").cloned().collect();
    let mut s = base.with_overrides(&overrides)?;
    if let Some((_, m)) = coord.iter().find(|(k, _)| k == "mix") {
        apply_mix(&mut s, m.as_str().unwrap_or(""))?;
    }
    s.seed = seed;
    s.name = format!("{}-{}-seed{seed}", base.name, coord.iter().map(|(k, v)| format!("{}={}", k.rsplit('.').next().unwrap_or(k), value_str(v))).collect::<Vec<_>>().join("-"));
    Ok(s)
}

pub fn run(cfg: &GridConfig, base: &Scenario, bars: &[Bar], funding: &[FundingRow], vol_index: &[(i64, f64)], threads: usize, out: Option<&std::path::Path>) -> Result<GridReport> {
    let coords = coordinates(cfg);
    let jobs: Vec<(usize, Coord, u64)> = coords.iter().enumerate().flat_map(|(i, c)| cfg.seeds.iter().map(move |&s| (i, c.clone(), s))).collect();
    let outs = study::par_map(jobs, threads, |(i, c, seed)| -> Result<(usize, Metric, bool)> {
        let s = scenario_at(base, &c, seed)?;
        let o = engine::run(&s, bars, funding, vol_index)?;
        let m = Metric::from_run(&s, &o);
        eprintln!("grid {} net {:+.4} dd {:.3} liq {} fills {}", s.name, m.depositor_net_return_annualized, m.max_drawdown, m.liquidations, m.fills);
        Ok((i, m, o.stats.venue_cap_hits > 0 || o.stats.flash_cap_hits > 0))
    });
    let mut per_point: Vec<(Vec<Metric>, bool)> = coords.iter().map(|_| (Vec::new(), false)).collect();
    for o in outs {
        let (i, m, venue) = o?;
        per_point[i].0.push(m);
        per_point[i].1 |= venue;
    }
    let policy_dd = base.hurdle.max_drawdown;
    let mut points = Vec::new();
    for (i, c) in coords.iter().enumerate() {
        let (seeds, venue) = &per_point[i];
        let d = study::distribution(seeds);
        let mut binding = String::new();
        if d.liquidation_count_total > 0 {
            binding = "liquidation".into();
        } else if d.bankrupt_fraction > 0.0 {
            binding = "bankrupt".into();
        } else if d.max_drawdown_worst > policy_dd {
            binding = format!("drawdown {:.3} > {policy_dd}", d.max_drawdown_worst);
        } else if !d.median_clears_hurdle {
            binding = format!("median net {:.4} < hurdle {:.4}", d.depositor_net_return_annualized.median, d.required_return);
        }
        let break_even = binding.is_empty();
        let rep = seeds.first().cloned().unwrap_or_default();
        points.push(GridPoint {
            index: i,
            coordinates: c.iter().map(|(k, v)| format!("{k}={}", value_str(v))).collect(),
            limit_label: study::limit_label(&rep, d.median_clears_hurdle, *venue).to_string(),
            seeds: seeds.clone(),
            distribution: d,
            break_even,
            binding,
        });
    }
    // Sensitivity: walk each axis with the others at their base value.
    let mut sensitivity = Vec::new();
    let mut axes: Vec<(String, Vec<String>)> = cfg.axes.iter().map(|a| (a.path.clone(), a.values.iter().map(value_str).collect())).collect();
    if !cfg.mixes.is_empty() {
        axes.push(("mix".into(), cfg.mixes.clone()));
    }
    for (ai, (path, values)) in axes.iter().enumerate() {
        let mut med = Vec::new();
        let mut be = Vec::new();
        for v in values {
            let target: Vec<String> = axes.iter().enumerate().map(|(j, (p, vs))| format!("{p}={}", if j == ai { v.clone() } else { vs[0].clone() })).collect();
            if let Some(pt) = points.iter().find(|p| p.coordinates == target) {
                med.push(pt.distribution.depositor_net_return_annualized.median);
                be.push(pt.break_even);
            }
        }
        let range = med.iter().cloned().fold(f64::NEG_INFINITY, f64::max) - med.iter().cloned().fold(f64::INFINITY, f64::min);
        sensitivity.push(AxisSensitivity { axis: path.clone(), values: values.clone(), median_returns: med, break_even: be, range: if range.is_finite() { range } else { 0.0 } });
    }
    let report = GridReport {
        name: cfg.name.clone(),
        scenario: base.name.clone(),
        from: base.from.clone(),
        to: base.to.clone(),
        seeds: cfg.seeds.clone(),
        axes: axes.iter().map(|(p, _)| p.clone()).collect(),
        break_even_count: points.iter().filter(|p| p.break_even).count(),
        labels: points.first().and_then(|p| p.seeds.first()).map(|m| m.labels.clone()).unwrap_or_default(),
        points,
        sensitivity,
    };
    if let Some(dir) = out {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join("grid.json"), serde_json::to_string_pretty(&report)?)?;
        std::fs::write(dir.join("surface.csv"), csv(&report))?;
    }
    Ok(report)
}

pub fn csv(r: &GridReport) -> String {
    let mut s = String::from("point,coordinates,seeds,net_median,net_mean,net_ci95_low,net_ci95_high,net_after_idle_median,max_drawdown_worst,daily_cvar95_median,liquidations,bankrupt_fraction,fills_median,accepted_notional_median,hurdle,break_even,binding,limit_label\n");
    for p in &r.points {
        let d = &p.distribution;
        s.push_str(&format!(
            "{},{},{},{:.5},{:.5},{:.5},{:.5},{:.5},{:.4},{:.5},{},{:.2},{:.1},{:.0},{:.4},{},{},{}\n",
            p.index,
            p.coordinates.join("|"),
            d.seeds,
            d.depositor_net_return_annualized.median,
            d.depositor_net_return_annualized.mean,
            d.depositor_net_return_annualized.ci95_low,
            d.depositor_net_return_annualized.ci95_high,
            d.net_return_after_idle_cost_annualized.median,
            d.max_drawdown_worst,
            d.daily_cvar95.median,
            d.liquidation_count_total,
            d.bankrupt_fraction,
            d.fills.median,
            d.accepted_notional.median,
            d.required_return,
            p.break_even,
            p.binding,
            p.limit_label
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::synthetic_bars;

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn grid_runs_the_product_with_common_seeds_and_reports_sensitivity() {
        let mut s = Scenario::default();
        s.from = "2025-01-01".into();
        s.to = "2025-01-06".into();
        s.flow.source = "generated".into();
        s.flow_gen.use_expiry_board = false;
        s.flow_gen.tenor_menu_days = vec![2.0];
        s.flow_gen.min_tenor_days = 0.5;
        s.acceptance.mode = "hazard".into();
        s.latency = crate::latency::LatencyConfig::zero();
        s.revalue_interval_min = 30;
        let start = crate::data::date_start_ms(&s.from).unwrap();
        let bars = synthetic_bars(6, start);
        let cfg = GridConfig {
            name: "t".into(),
            seeds: vec![1, 2],
            axes: vec![
                Axis { path: "bid.base_spread_volpts".into(), values: vec![toml::Value::Float(0.02), toml::Value::Float(0.30)] },
                Axis { path: "venue.execution_assumption".into(), values: vec![toml::Value::String("taker_only".into()), toml::Value::String("conservative".into())] },
            ],
            mixes: vec!["balanced".into(), "put_heavy".into()],
            ..Default::default()
        };
        let r = run(&cfg, &s, &bars, &[], &[], 4, None).unwrap();
        assert_eq!(r.points.len(), 8);
        assert!(r.points.iter().all(|p| p.seeds.len() == 2 && p.seeds[0].seed == 1 && p.seeds[1].seed == 2));
        assert_eq!(r.sensitivity.len(), 3);
        assert_eq!(r.sensitivity[0].values, vec!["0.02", "0.3"]);
        // Common random numbers: the wider bid accepts less at every seed.
        let tight = &r.points[0].seeds;
        let wide = r.points.iter().find(|p| p.coordinates[0] == "bid.base_spread_volpts=0.3" && p.coordinates[1] == "venue.execution_assumption=taker_only" && p.coordinates[2] == "mix=balanced").unwrap();
        for (a, b) in tight.iter().zip(&wide.seeds) {
            assert!(b.fills <= a.fills, "{} vs {}", b.fills, a.fills);
        }
        // Central and conservative execution are both published.
        assert!(r.points.iter().any(|p| p.coordinates.contains(&"venue.execution_assumption=taker_only".to_string())));
        assert!(r.points.iter().any(|p| p.coordinates.contains(&"venue.execution_assumption=conservative".to_string())));
        assert!(r.points.iter().all(|p| ["demand_limited", "capital_limited", "venue_limited", "uneconomic"].contains(&p.limit_label.as_str())));
        let csv = csv(&r);
        assert_eq!(csv.lines().count(), 9);
    }

    #[test]
    fn mixes_cover_the_required_variants() {
        let total = Scenario::default().flow_gen.call.base_rate_per_day + Scenario::default().flow_gen.put.base_rate_per_day;
        for m in ["call_only", "put_only", "balanced", "prior_calibrated", "call_heavy", "put_heavy"] {
            let mut s = Scenario::default();
            apply_mix(&mut s, m).unwrap();
            let g = &s.flow_gen;
            assert!((0.0..=1.0).contains(&g.call_share));
            assert!((g.call.base_rate_per_day + g.put.base_rate_per_day - total).abs() < 1e-9, "{m}: total intensity preserved");
            assert!((g.call.base_rate_per_day / total - g.call_share).abs() < 1e-9);
        }
        let mut s = Scenario::default();
        apply_mix(&mut s, "put_heavy").unwrap();
        assert!(s.flow_gen.put.base_rate_per_day > s.flow_gen.call.base_rate_per_day);
        assert!(apply_mix(&mut Scenario::default(), "nope").is_err());
    }
}
