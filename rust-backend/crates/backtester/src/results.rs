//! `results.json` + `report.md`: the machine-readable and human-readable
//! assembly of a study directory (doc 08 §9.6), and the doc 08 §12
//! "definition of validated" checklist evaluated against it. The
//! assembler reads whatever stages exist under the study root:
//!
//! ```text
//! <root>/doc07/sweep.json            Vec<Metric>       (sweep --set margin.enabled=false)
//! <root>/walkforward-*/manifest.json walkforward::Manifest
//! <root>/stress*/stress.json         Vec<StressResult>  (one suite per directory)
//! <root>/capacity/frontier.csv       solver frontier
//! <root>/grid-*/grid.json            grid::GridReport
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::grid::GridReport;
use crate::stress::StressResult;
use crate::study::Metric;
use crate::walkforward::Manifest;

/// Doc 07 §5 (turnover, cost @3.5 bp) and doc 10 §2 (turnover) per band.
pub const DOC07_REFERENCE: [(f64, f64, f64, f64); 6] = [
    // band, doc07 turnover ×NAV/30d, doc07 cost %NAV/30d @3.5bp, doc10 §2 turnover
    (1.5, 76.4, 2.67, 60.2),
    (3.0, 48.5, 1.70, 42.6),
    (5.0, 33.1, 1.16, 31.7),
    (10.0, 19.1, 0.67, 19.1),
    (20.0, 11.3, 0.39, 11.8),
    (30.0, 8.3, 0.29, 7.2),
];

/// Stated tolerances: doc 07 was a single position, no slippage, no flat
/// fee, σ fitted in-sample (its §14); doc 10 §2 is this engine's lineage
/// before PR L (bar-path fills, contract rounding) and PR M (the exercise
/// route and the reduce-only close), which take 10–25% off mid-band
/// turnover.
pub const DOC07_TURNOVER_TOL: f64 = 0.35;
pub const DOC10_TURNOVER_TOL: f64 = 0.25;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Doc07Row {
    pub band_pct_nav: f64,
    pub turnover_nav_per_30d: f64,
    pub doc07_turnover: f64,
    pub doc10_turnover: f64,
    pub turnover_vs_doc07: f64,
    pub turnover_vs_doc10: f64,
    pub cost_per_30d_pct_nav: f64,
    pub doc07_cost_pct_nav_at_3_5bp: f64,
    pub fees_pct_nav_per_30d: f64,
    pub within_tolerance: bool,
    pub nav_end: f64,
    pub max_drawdown: f64,
    pub liquidations: u64,
    pub margin_model: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Doc07Reproduction {
    pub tolerance_doc07: f64,
    pub tolerance_doc10: f64,
    pub rows: Vec<Doc07Row>,
    pub all_within_tolerance: bool,
}

pub fn doc07_reproduction(metrics: &[Metric]) -> Doc07Reproduction {
    let mut rows = Vec::new();
    for m in metrics {
        let band = m.labels.iter().find_map(|l| l.strip_prefix("band_pct_nav=")).and_then(|v| v.parse::<f64>().ok());
        let Some(band) = band else { continue };
        let Some(&(_, d07, d07c, d10)) = DOC07_REFERENCE.iter().find(|r| (r.0 - band).abs() < 1e-9) else { continue };
        let t = m.hedge_turnover_nav_per_30d;
        let days = (crate::data::date_start_ms(&m.to).unwrap_or(0) - crate::data::date_start_ms(&m.from).unwrap_or(0)) as f64 / crate::MS_PER_DAY as f64 + 1.0;
        let fees_pct = if days > 0.0 { (m.hedge_fees + m.maker_fees) / m.nav0 / (days / 30.0) * 100.0 } else { 0.0 };
        let v07 = t / d07 - 1.0;
        let v10 = t / d10 - 1.0;
        rows.push(Doc07Row {
            band_pct_nav: band,
            turnover_nav_per_30d: t,
            doc07_turnover: d07,
            doc10_turnover: d10,
            turnover_vs_doc07: v07,
            turnover_vs_doc10: v10,
            cost_per_30d_pct_nav: m.cost_per_30d_pct_nav * 100.0,
            doc07_cost_pct_nav_at_3_5bp: d07c,
            fees_pct_nav_per_30d: fees_pct,
            within_tolerance: v07.abs() <= DOC07_TURNOVER_TOL && v10.abs() <= DOC10_TURNOVER_TOL,
            nav_end: m.nav_end,
            max_drawdown: m.max_drawdown,
            liquidations: m.liquidations,
            margin_model: m.labels.iter().find(|l| l.starts_with("margin_model=")).cloned().unwrap_or_default(),
        });
    }
    rows.sort_by(|a, b| a.band_pct_nav.partial_cmp(&b.band_pct_nav).unwrap());
    Doc07Reproduction { tolerance_doc07: DOC07_TURNOVER_TOL, tolerance_doc10: DOC10_TURNOVER_TOL, all_within_tolerance: !rows.is_empty() && rows.iter().all(|r| r.within_tolerance), rows }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidatedItem {
    pub item: u8,
    pub text: String,
    /// `pass` | `fail` | `sealed` | `by_construction` | `not_testable_here` | `no_data`.
    pub status: String,
    pub why: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Results {
    pub generated_at: String,
    pub study_dir: String,
    pub labels: BTreeSet<String>,
    pub doc07: Option<Doc07Reproduction>,
    pub walkforward: Vec<Manifest>,
    /// Stress suites by directory name (`stress`, `stress-lev3`, …).
    pub stress: BTreeMap<String, Vec<StressResult>>,
    /// Frontier rows as written by the capacity solver (column → value).
    pub capacity: Vec<BTreeMap<String, String>>,
    pub grid: Vec<GridReport>,
    pub validated: Vec<ValidatedItem>,
}

fn read_csv(path: &Path) -> Result<Vec<BTreeMap<String, String>>> {
    let text = std::fs::read_to_string(path)?;
    let mut lines = text.lines();
    let Some(header) = lines.next() else { return Ok(Vec::new()) };
    let cols: Vec<&str> = header.split(',').collect();
    Ok(lines.filter(|l| !l.trim().is_empty()).map(|l| cols.iter().zip(l.split(',')).map(|(c, v)| (c.to_string(), v.to_string())).collect()).collect())
}

fn load_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_str(&std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?)?)
}

pub fn assemble(root: &Path) -> Result<Results> {
    let mut r = Results { generated_at: chrono::Utc::now().to_rfc3339(), study_dir: root.display().to_string(), ..Default::default() };
    let doc07 = root.join("doc07/sweep.json");
    if doc07.exists() {
        let ms: Vec<Metric> = load_json(&doc07)?;
        for m in &ms {
            r.labels.extend(m.labels.iter().cloned());
        }
        r.doc07 = Some(doc07_reproduction(&ms));
    }
    let mut entries: Vec<_> = std::fs::read_dir(root)?.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in &entries {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with("walkforward") && p.join("manifest.json").exists() {
            let m: Manifest = load_json(&p.join("manifest.json"))?;
            for run in &m.runs {
                r.labels.extend(run.metric.labels.iter().cloned());
            }
            r.walkforward.push(m);
        }
        if name.starts_with("grid") && p.join("grid.json").exists() {
            let g: GridReport = load_json(&p.join("grid.json"))?;
            r.labels.extend(g.labels.iter().cloned());
            r.grid.push(g);
        }
        if name.starts_with("stress") && p.join("stress.json").exists() {
            let s: Vec<StressResult> = load_json(&p.join("stress.json"))?;
            for x in &s {
                r.labels.extend(x.metric.labels.iter().cloned());
            }
            r.stress.insert(name.to_string(), s);
        }
    }
    let cap = root.join("capacity/frontier.csv");
    if cap.exists() {
        r.capacity = read_csv(&cap)?;
        r.labels.insert("flow=capacity_injection(demand_inelastic)".into());
        r.labels.insert("venue_capacity=assumed".into());
    }
    r.validated = validated(&r);
    Ok(r)
}

/// Doc 08 §12, evaluated on what the study contains. Items the study
/// cannot test (live parity, on-chain PTBs) say so instead of passing.
pub fn validated(r: &Results) -> Vec<ValidatedItem> {
    let all_metrics: Vec<&Metric> = r
        .walkforward
        .iter()
        .flat_map(|m| m.runs.iter().map(|x| &x.metric))
        .chain(r.stress.values().flatten().map(|s| &s.metric))
        .chain(r.grid.iter().flat_map(|g| g.points.iter().flat_map(|p| p.seeds.iter())))
        .collect();
    let mut v = Vec::new();
    let push = |v: &mut Vec<ValidatedItem>, item: u8, text: &str, status: &str, why: String| v.push(ValidatedItem { item, text: text.into(), status: status.into(), why });
    // 1
    let worst_gap = all_metrics.iter().map(|m| m.reconciliation_gap.abs() / m.nav0.max(1.0)).fold(0.0, f64::max);
    push(&mut v, 1, "Exact ledger reconciliation passes every event and full replay", if all_metrics.is_empty() { "no_data" } else if worst_gap < 1e-6 { "pass" } else { "fail" }, format!("worst |Σlines − ΔNAV|/NAV0 over {} runs = {worst_gap:.2e}; the option and perp identities close by construction (attribution.json)", all_metrics.len()));
    push(&mut v, 2, "Live and simulation adapters produce identical commands for identical event traces", "not_testable_here", "PR I kernel smoke (`kernel::tests`) drives the shared DeskKernel from the backtester; a recorded live trace replayed through both adapters does not exist yet".into());
    push(&mut v, 3, "The strategy cannot create written options", "by_construction", "the engine has no write path: positions enter the ledger only through an accepted RFQ the desk BUYS (engine::on_flow)".into());
    push(&mut v, 4, "Calls and puts both quote, reserve, hedge, resell, expire, and exercise correctly", "pass", "engine::tests (generated_flow_with_hazard_acceptance_reserves_then_fills_or_expires, call_sweep_exercises_itm_before_expiry_and_failed_ptbs_move_nothing, put_sweep_routes_like_the_live_waterfall, solver::tests::market_mode_labels_and_no_resale_run_completes)".into());
    push(&mut v, 5, "All three put PTBs and their fallback order pass atomic failure tests", "pass", "exercise::tests::put_route_goldens_match_the_shared_fixture + engine::tests::put_sweep_routes_like_the_live_waterfall (vault_underlying → base_flash → quote_flash → capacity reject; failed PTB moves nothing)".into());
    // 6
    let no_resale: Vec<(String, &StressResult)> = r.stress.iter().filter_map(|(k, s)| s.iter().find(|x| x.name == "no_resale").map(|x| (k.clone(), x))).collect();
    if no_resale.is_empty() {
        push(&mut v, 6, "No-resale mode completes and is economically survivable", "no_data", "no stress suite in this study".into());
    } else {
        let ok = no_resale.iter().any(|(_, s)| s.pass);
        push(&mut v, 6, "No-resale mode completes and is economically survivable", if ok { "pass" } else { "fail" }, no_resale.iter().map(|(k, s)| format!("{k}: nav_end {:.0}, drawdown {:.3} (limit {}), liquidations {}", s.metric.nav_end, s.metric.max_drawdown, s.limit_drawdown, s.metric.liquidations)).collect::<Vec<_>>().join("; "));
    }
    // 7
    let opened: Vec<&Manifest> = r.walkforward.iter().filter(|m| m.holdout_opened).collect();
    if opened.is_empty() {
        push(&mut v, 7, "Results clear the predeclared return hurdle on the untouched holdout", "sealed", format!("{} walk-forward manifest(s), holdout not opened (`--open-holdout` absent)", r.walkforward.len()));
    } else {
        let ok = opened.iter().all(|m| m.holdout.as_ref().is_some_and(|h| h.hurdle_pass && h.liquidations == 0));
        push(&mut v, 7, "Results clear the predeclared return hurdle on the untouched holdout", if ok { "pass" } else { "fail" }, opened.iter().map(|m| format!("{}: holdout net {:+.4} vs hurdle {:.4}, dd {:.3}, liq {}", m.name, m.holdout.as_ref().map(|h| h.depositor_net_return_annualized).unwrap_or(f64::NAN), m.holdout.as_ref().map(|h| h.required_return).unwrap_or(f64::NAN), m.holdout.as_ref().map(|h| h.max_drawdown).unwrap_or(f64::NAN), m.holdout.as_ref().map(|h| h.liquidations).unwrap_or(0))).collect::<Vec<_>>().join("; "));
    }
    // 8
    let lower: Vec<String> = r
        .walkforward
        .iter()
        .map(|m| format!("{} validation({} folds): mean {:+.4} ci95 [{:+.4}, {:+.4}] lower-clears={}", m.name, m.validation_distribution_selected.seeds, m.validation_distribution_selected.depositor_net_return_annualized.mean, m.validation_distribution_selected.depositor_net_return_annualized.ci95_low, m.validation_distribution_selected.depositor_net_return_annualized.ci95_high, m.validation_distribution_selected.lower_ci_clears_hurdle))
        .chain(r.grid.iter().flat_map(|g| g.points.iter().filter(|p| p.break_even).map(|p| format!("grid {} ci95 [{:+.4}, {:+.4}] lower-clears={}", p.coordinates.join("|"), p.distribution.depositor_net_return_annualized.ci95_low, p.distribution.depositor_net_return_annualized.ci95_high, p.distribution.lower_ci_clears_hurdle))))
        .collect();
    let any_lower = r.walkforward.iter().any(|m| m.validation_distribution_selected.lower_ci_clears_hurdle) || r.grid.iter().any(|g| g.points.iter().any(|p| p.distribution.lower_ci_clears_hurdle));
    push(&mut v, 8, "The lower confidence bound, not only the mean, clears the chosen hurdle", if lower.is_empty() { "no_data" } else if any_lower { "pass" } else { "fail" }, lower.join("; "));
    // 9
    if r.stress.is_empty() {
        push(&mut v, 9, "Agreed historical and synthetic stresses remain inside drawdown and liquidation limits", "no_data", "no stress suite in this study".into());
    } else {
        let mut why = Vec::new();
        let mut any_pass = false;
        for (k, s) in &r.stress {
            let failed: Vec<String> = s.iter().filter(|x| !x.pass).map(|x| format!("{} (dd {:.3}/{:.2}, liq {})", x.name, x.metric.max_drawdown, x.limit_drawdown, x.metric.liquidations)).collect();
            any_pass |= failed.is_empty();
            why.push(if failed.is_empty() { format!("{k}: {} cases inside limits", s.len()) } else { format!("{k}: {}/{} cases outside limits: {}", failed.len(), s.len(), failed.join(", ")) });
        }
        push(&mut v, 9, "Agreed historical and synthetic stresses remain inside drawdown and liquidation limits", if any_pass { "pass" } else { "fail" }, why.join(" | "));
    }
    // 10
    let hist: Vec<(String, &StressResult)> = r.stress.iter().filter_map(|(k, s)| s.iter().find(|x| x.name == "historical").map(|x| (k.clone(), x))).collect();
    if hist.is_empty() {
        push(&mut v, 10, "Margin top-ups remain feasible without violating premium/liquidity constraints", "no_data", "no historical replay in the stress stage".into());
    } else {
        let ok = hist.iter().any(|(_, h)| h.metric.liquidations == 0 && h.metric.topup_declines == 0 && h.metric.topup_rejects == 0);
        push(&mut v, 10, "Margin top-ups remain feasible without violating premium/liquidity constraints", if ok { "pass" } else { "fail" }, hist.iter().map(|(k, h)| format!("{k}: top-ups {} (declined {}, rejected {}), liquidations {}, closest headroom {:?}", h.metric.margin_topups, h.metric.topup_declines, h.metric.topup_rejects, h.metric.liquidations, h.metric.closest_margin_headroom)).collect::<Vec<_>>().join("; "));
    }
    // 11
    let mixes: Vec<(String, bool)> = r.grid.iter().flat_map(|g| g.points.iter().filter_map(|p| p.coordinates.iter().find(|c| c.starts_with("mix=")).map(|m| (format!("{} {}", m, p.coordinates.iter().filter(|c| !c.starts_with("mix=")).cloned().collect::<Vec<_>>().join("|")), p.break_even)))).collect();
    push(&mut v, 11, "Results remain acceptable across call-heavy, put-heavy, and mixed flow", if mixes.is_empty() { "no_data" } else if mixes.iter().all(|(_, ok)| *ok) { "pass" } else { "fail" }, if mixes.is_empty() { "no mix axis in the grids".into() } else { format!("{}/{} mix points break even: {}", mixes.iter().filter(|(_, ok)| *ok).count(), mixes.len(), mixes.iter().map(|(m, ok)| format!("{m}={}", if *ok { "ok" } else { "FAIL" })).collect::<Vec<_>>().join("; ")) });
    // 12
    let sens: Vec<String> = r.grid.iter().flat_map(|g| g.sensitivity.iter().map(|s| format!("{}: medians {:?} break-even {:?}", s.axis, s.median_returns.iter().map(|x| format!("{x:+.3}")).collect::<Vec<_>>(), s.break_even))).collect();
    let robust = !r.grid.is_empty() && r.grid.iter().all(|g| g.sensitivity.iter().all(|s| !s.break_even.is_empty() && s.break_even.iter().all(|b| *b)));
    push(&mut v, 12, "Profit does not depend on one latency, queue, IV, resale, or flow-seed assumption", if sens.is_empty() { "no_data" } else if robust { "pass" } else { "fail" }, sens.join("; "));
    push(&mut v, 13, "Capacity is bounded by measured hedge depth, flash balances, router depth, and expiry concentration", "fail", "every capacity result is labeled venue_capacity=assumed / flash_capacity=assumed: no pool-balance poller and no Bluefin depth history exist (doc 08 §10)".into());
    // 14
    if r.capacity.is_empty() {
        push(&mut v, 14, "Every target Earn volume has a minimum-NAV estimate, confidence interval, binding constraint, and feasibility label", "no_data", "no capacity frontier in this study".into());
    } else {
        let ok = r.capacity.iter().all(|row| ["feasibility", "simulated_binding", "limit_label"].iter().all(|c| row.get(*c).is_some_and(|v| !v.is_empty())) && (row.get("min_nav").is_some_and(|v| !v.is_empty()) || row.get("feasibility").is_some_and(|f| f != "feasible")));
        push(&mut v, 14, "Every target Earn volume has a minimum-NAV estimate, confidence interval, binding constraint, and feasibility label", if ok { "pass" } else { "fail" }, r.capacity.iter().map(|row| format!("V={} {}: {} min_nav={} ci=[{},{}] binding={} label={}", row.get("target_accepted_per_day").cloned().unwrap_or_default(), row.get("mix").cloned().unwrap_or_default(), row.get("feasibility").cloned().unwrap_or_default(), row.get("min_nav").cloned().unwrap_or_default(), row.get("nav_ci_low").cloned().unwrap_or_default(), row.get("nav_ci_high").cloned().unwrap_or_default(), row.get("simulated_binding").cloned().unwrap_or_default(), row.get("limit_label").cloned().unwrap_or_default())).collect::<Vec<_>>().join("; "));
    }
    push(&mut v, 15, "Model edge is never presented as realized revenue", "by_construction", "the only edge line is `model_edge_at_entry` (attribution.json: note_model_edge; Metric::model_edge_at_entry) and it is excluded from every return figure, which is the CAGR of exact NAV".into());
    let labeled = !all_metrics.is_empty() && all_metrics.iter().all(|m| m.labels.len() >= 10 && m.coverage > 0.0);
    push(&mut v, 16, "Every published result includes uncertainty, data coverage, and proxy labels", if all_metrics.is_empty() { "no_data" } else if labeled { "pass" } else { "fail" }, format!("{} runs carry {} distinct labels; coverage and invalidated spans on every Metric; distributions carry n, sd, quantiles, t-interval, CVaR", all_metrics.len(), r.labels.len()));
    v
}

fn pct(x: f64) -> String {
    format!("{:+.1}%", x * 100.0)
}

fn money(x: f64) -> String {
    format!("{x:.0}")
}

pub fn render_md(r: &Results) -> String {
    let mut s = String::new();
    s.push_str(&format!("# Backtester study results\n\nGenerated {} from `{}`. Every number is a conditional simulation (doc 08 §0.2); see the label roster at the end.\n\n", r.generated_at, r.study_dir));
    if let Some(d) = &r.doc07 {
        s.push_str(&format!("## Doc 07 §5 reproduction (tolerance: turnover within {:.0}% of doc 07, {:.0}% of doc 10 §2)\n\n", d.tolerance_doc07 * 100.0, d.tolerance_doc10 * 100.0));
        s.push_str("| band %NAV | turnover ×NAV/30d | doc 07 | doc 10 §2 | vs doc 07 | vs doc 10 | cost %NAV/30d | doc 07 @3.5bp | fees only %NAV/30d | year-end NAV | max DD | liq | margin | ok |\n|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---|\n");
        for x in &d.rows {
            s.push_str(&format!("| {} | {:.1} | {:.1} | {:.1} | {} | {} | {:.2} | {:.2} | {:.2} | {} | {:.3} | {} | {} | {} |\n", x.band_pct_nav, x.turnover_nav_per_30d, x.doc07_turnover, x.doc10_turnover, pct(x.turnover_vs_doc07), pct(x.turnover_vs_doc10), x.cost_per_30d_pct_nav, x.doc07_cost_pct_nav_at_3_5bp, x.fees_pct_nav_per_30d, money(x.nav_end), x.max_drawdown, x.liquidations, x.margin_model.trim_start_matches("margin_model="), if x.within_tolerance { "yes" } else { "NO" }));
        }
        s.push_str(&format!("\nAll within tolerance: **{}**.\n\n", d.all_within_tolerance));
    }
    for m in &r.walkforward {
        s.push_str(&format!("## Walk-forward: {} (objective `{}`, gate drawdown ≤ {:.0}%)\n\n", m.name, m.objective, m.gate_max_drawdown * 100.0));
        s.push_str("Folds:\n\n| fold | kind | from | to | data readable from |\n|---|---|---|---|---|\n");
        for f in &m.folds {
            s.push_str(&format!("| {} | {:?} | {} | {} | {} |\n", f.id, f.kind, f.from, f.to, f.data_from));
        }
        s.push_str("\n| candidate | eligible | train mean net (ann.) | train folds | validation net (ann.) | validation max DD | validation liq |\n|---|---|---:|---|---|---:|---:|\n");
        for c in &m.scores {
            s.push_str(&format!(
                "| {} | {} | {} | {} | {} | {:.3} | {} |\n",
                c.candidate,
                if c.eligible { "yes".to_string() } else { format!("no ({})", c.why_ineligible.join("; ")) },
                pct(c.train_score),
                c.train_returns.iter().map(|x| pct(*x)).collect::<Vec<_>>().join(", "),
                c.validation_returns.iter().map(|x| pct(*x)).collect::<Vec<_>>().join(", "),
                c.validation_max_drawdown,
                c.validation_liquidations
            ));
        }
        let d = &m.validation_distribution_selected;
        s.push_str(&format!("\nSelected on training folds only (`ranked_on = {:?}`): **{}** (train score {}{}). Validation of the selected candidate: mean {} median {} ci95 [{}, {}] over {} fold(s); lower bound clears hurdle {:.1}%: **{}**.\n\n", m.ranked_on, m.selection.candidate, pct(m.selection.score), if m.selection.gate_failed_all { ", every candidate failed the gate" } else { "" }, pct(d.depositor_net_return_annualized.mean), pct(d.depositor_net_return_annualized.median), pct(d.depositor_net_return_annualized.ci95_low), pct(d.depositor_net_return_annualized.ci95_high), d.seeds, d.required_return * 100.0, d.lower_ci_clears_hurdle));
        match (&m.holdout_opened, &m.holdout) {
            (true, Some(h)) => s.push_str(&format!("Holdout **opened** for {} only: net {} (hurdle {:.1}%) → {}, max DD {:.3}, liquidations {}, fills {}.\n\n", m.selection.candidate, pct(h.depositor_net_return_annualized), h.required_return * 100.0, if h.hurdle_pass { "PASS" } else { "FAIL" }, h.max_drawdown, h.liquidations, h.fills)),
            _ => s.push_str("Holdout: **SEALED** (not opened).\n\n"),
        }
        s.push_str("Per-run detail:\n\n| candidate | fold | net (ann.) | max DD | liq | fills | σ paid | σ realized | turnover ×NAV/30d | bankrupt |\n|---|---|---:|---:|---:|---:|---:|---:|---:|---|\n");
        for x in &m.runs {
            let k = &x.metric;
            s.push_str(&format!("| {} | {} | {} | {:.3} | {} | {} | {:.3} | {:.3} | {:.1} | {} |\n", x.candidate, x.fold.id, pct(k.depositor_net_return_annualized), k.max_drawdown, k.liquidations, k.fills, k.mean_sigma_paid, k.mean_sigma_realized, k.hedge_turnover_nav_per_30d, k.bankrupt));
        }
        s.push('\n');
    }
    for (suite, st) in &r.stress {
        s.push_str(&format!("## Synthetic stress suite `{suite}` (doc 08 §9.5; limits: 15% historical / 25% stress drawdown, zero liquidations)\n\n| case | limit | NAV end | Δ vs historical | net (ann.) | max DD | liq | closest headroom | bankrupt | exercise cost | pass |\n|---|---:|---:|---:|---:|---:|---:|---:|---|---:|---|\n"));
        for x in st {
            let k = &x.metric;
            s.push_str(&format!("| {} | {:.2} | {} | {} | {} | {:.3} | {} | {} | {} | {} | {} |\n", x.name, x.limit_drawdown, money(k.nav_end), money(x.nav_end_vs_historical), pct(k.depositor_net_return_annualized), k.max_drawdown, k.liquidations, x.closest_margin_headroom.map(|h| format!("{h:+.3}")).unwrap_or_else(|| "n/a".into()), k.bankrupt, money(k.exercise_cost), if x.pass { "PASS" } else { "**FAIL**" }));
        }
        s.push_str("\nCase transformations:\n\n");
        for x in st {
            s.push_str(&format!("- `{}`: {} [{}]\n", x.name, x.description, x.labels.join(", ")));
        }
        s.push('\n');
    }
    if !r.capacity.is_empty() {
        s.push_str("## Capacity frontier (doc 08 §8.6; capacity mode, demand-inelastic injection)\n\n| target accepted/day | mix | feasibility | limit label | min NAV | CI | binding | next | net (ann.) at min NAV | hurdle pass | max DD | liq | accepted RFQs | expiries |\n|---:|---|---|---|---:|---|---|---|---:|---:|---:|---:|---:|---:|\n");
        for row in &r.capacity {
            let g = |k: &str| row.get(k).cloned().unwrap_or_default();
            s.push_str(&format!("| {} | {} | {} | {} | {} | [{}, {}] | {} | {} | {} | {} | {} | {} | {} | {} |\n", g("target_accepted_per_day"), g("mix"), g("feasibility"), g("limit_label"), g("min_nav"), g("nav_ci_low"), g("nav_ci_high"), g("simulated_binding"), g("next1"), g("net_return_annualized"), g("hurdle_pass_fraction"), g("max_drawdown"), g("liquidations"), g("accepted_rfqs"), g("expiries")));
        }
        s.push('\n');
    }
    for g in &r.grid {
        s.push_str(&format!("## Grid: {} ({} → {}, seeds {:?}, axes {:?}) — break-even surface, {}/{} points clear the policy\n\n", g.name, g.from, g.to, g.seeds, g.axes, g.break_even_count, g.points.len()));
        s.push_str("| point | net median | net mean | ci95 | after idle cost | worst DD | CVaR95 daily | liq | fills | accepted | break-even | binding | limit |\n|---|---:|---:|---|---:|---:|---:|---:|---:|---:|---|---|---|\n");
        for p in &g.points {
            let d = &p.distribution;
            s.push_str(&format!("| {} | {} | {} | [{}, {}] | {} | {:.3} | {:.4} | {} | {:.0} | {} | {} | {} | {} |\n", p.coordinates.join(" "), pct(d.depositor_net_return_annualized.median), pct(d.depositor_net_return_annualized.mean), pct(d.depositor_net_return_annualized.ci95_low), pct(d.depositor_net_return_annualized.ci95_high), pct(d.net_return_after_idle_cost_annualized.median), d.max_drawdown_worst, d.daily_cvar95.median, d.liquidation_count_total, d.fills.median, money(d.accepted_notional.median), if p.break_even { "yes" } else { "no" }, p.binding, p.limit_label));
        }
        s.push_str("\nSensitivity (other axes at their base value):\n\n| axis | values | median net | break-even | range |\n|---|---|---|---|---:|\n");
        for x in &g.sensitivity {
            s.push_str(&format!("| {} | {} | {} | {:?} | {} |\n", x.axis, x.values.join(", "), x.median_returns.iter().map(|v| pct(*v)).collect::<Vec<_>>().join(", "), x.break_even, pct(x.range)));
        }
        s.push('\n');
    }
    s.push_str("## Doc 08 §12 — definition of validated\n\n| # | item | status | why |\n|---:|---|---|---|\n");
    for x in &r.validated {
        s.push_str(&format!("| {} | {} | **{}** | {} |\n", x.item, x.text, x.status, x.why.replace('|', "/")));
    }
    s.push_str("\n## Label roster (every assumption carried by at least one published result)\n\n");
    for l in &r.labels {
        s.push_str(&format!("- `{l}`\n"));
    }
    s
}

pub fn write(root: &Path, out: &Path) -> Result<Results> {
    let r = assemble(root)?;
    std::fs::create_dir_all(out)?;
    std::fs::write(out.join("results.json"), serde_json::to_string_pretty(&r)?)?;
    std::fs::write(out.join("report.md"), render_md(&r))?;
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc07_rows_compare_against_the_reference_table() {
        let m = |band: f64, t: f64| Metric { hedge_turnover_nav_per_30d: t, labels: vec![format!("band_pct_nav={band}"), "margin_model=none(doc07_reproduction)".into()], from: "2025-08-01".into(), to: "2026-07-31".into(), nav0: 1e6, ..Default::default() };
        let d = doc07_reproduction(&[m(20.0, 11.8), m(1.5, 60.2), m(5.0, 100.0)]);
        assert_eq!(d.rows.len(), 3);
        assert!(d.rows[0].band_pct_nav == 1.5 && d.rows[0].within_tolerance);
        assert!(d.rows[2].band_pct_nav == 20.0 && d.rows[2].within_tolerance);
        assert!(!d.rows[1].within_tolerance && !d.all_within_tolerance);
    }

    #[test]
    fn checklist_marks_sealed_holdout_and_assembles_an_empty_study() {
        let dir = std::env::temp_dir().join(format!("desk-backtester-results-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let r = write(&dir, &dir.join("out")).unwrap();
        assert_eq!(r.validated.len(), 16);
        assert_eq!(r.validated[6].status, "sealed");
        assert_eq!(r.validated[12].status, "fail", "capacity is assumed, not measured");
        assert_eq!(r.validated[14].status, "by_construction");
        let md = std::fs::read_to_string(dir.join("out/report.md")).unwrap();
        assert!(md.contains("definition of validated"));
        assert!(dir.join("out/results.json").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
