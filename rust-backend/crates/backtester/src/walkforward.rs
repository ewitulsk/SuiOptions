//! Walk-forward protocol (doc 08 §9.2): chronological calibration /
//! training / validation / final-holdout splits from config; parameters
//! chosen on training folds only; validation reported for every
//! candidate; the holdout opened once, by an explicit flag, for the
//! selected candidate only — the runner never ranks on it.
//!
//! The runner is generic over the run function so a test can feed it a
//! trace and prove that the selection reads nothing but past folds.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::scenario::Scenario;
use crate::study::{self, Metric};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Candidate {
    pub name: String,
    /// Dotted scenario paths → values (`"estimator.q_bid" = 0.35`).
    #[serde(default)]
    pub overrides: BTreeMap<String, toml::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct WalkForwardConfig {
    pub name: String,
    /// Base scenario path (relative to the config file).
    pub scenario: String,
    /// Earliest date the estimator warm-up / HAR calibration may read.
    pub calibration_from: String,
    /// `[[from, to]]` inclusive dates, chronological.
    pub train: Vec<[String; 2]>,
    pub validation: Vec<[String; 2]>,
    pub holdout: Option<[String; 2]>,
    /// `depositor_net_return_annualized` (the predeclared criterion).
    pub objective: String,
    /// Selection gate (predeclared): a candidate with a liquidation, a
    /// bankruptcy, or a training-fold drawdown above this is not eligible.
    pub gate_max_drawdown: f64,
    pub candidates: Vec<Candidate>,
}

impl Default for WalkForwardConfig {
    fn default() -> Self {
        Self {
            name: "walkforward".into(),
            scenario: String::new(),
            calibration_from: String::new(),
            train: Vec::new(),
            validation: Vec::new(),
            holdout: None,
            objective: "depositor_net_return_annualized".into(),
            gate_max_drawdown: 0.15,
            candidates: Vec::new(),
        }
    }
}

impl WalkForwardConfig {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let c: Self = toml::from_str(&text)?;
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.candidates.is_empty(), "no candidates");
        anyhow::ensure!(!self.train.is_empty(), "no training folds");
        anyhow::ensure!(!self.validation.is_empty(), "no validation folds");
        let mut last = self.calibration_from.clone();
        for f in self.train.iter().chain(self.validation.iter()).chain(self.holdout.iter()) {
            anyhow::ensure!(f[0] <= f[1], "fold {f:?} is reversed");
            anyhow::ensure!(f[0] > last || last.is_empty(), "fold {f:?} is not after {last}: splits must be chronological");
            last = f[1].clone();
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum FoldKind {
    Train,
    Validation,
    Holdout,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fold {
    pub id: String,
    pub kind: FoldKind,
    pub from: String,
    pub to: String,
    /// The earliest data the run may read (warm-up / calibration).
    pub data_from: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunRecord {
    pub candidate: String,
    pub fold: Fold,
    /// Order in which the runner issued it (selection happens after the
    /// last training run and before any validation run).
    pub sequence: usize,
    pub metric: Metric,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CandidateScore {
    pub candidate: String,
    pub eligible: bool,
    pub why_ineligible: Vec<String>,
    /// Mean objective over the training folds.
    pub train_score: f64,
    pub train_folds: Vec<String>,
    pub train_returns: Vec<f64>,
    pub validation_returns: Vec<f64>,
    pub validation_max_drawdown: f64,
    pub validation_liquidations: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Selection {
    pub candidate: String,
    pub score: f64,
    pub based_on_folds: Vec<String>,
    pub selection_sequence: usize,
    pub gate_failed_all: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub objective: String,
    pub gate_max_drawdown: f64,
    pub calibration_from: String,
    pub folds: Vec<Fold>,
    pub candidates: Vec<Candidate>,
    pub runs: Vec<RunRecord>,
    pub scores: Vec<CandidateScore>,
    pub selection: Selection,
    /// Never anything but `["train"]`: the holdout is not a ranking input.
    pub ranked_on: Vec<String>,
    pub holdout_opened: bool,
    pub holdout: Option<Metric>,
    pub validation_distribution_selected: study::Distribution,
}

pub fn warm_days(s: &Scenario) -> i64 {
    let mut d = (s.estimator.long_window_hours / 24.0).ceil() as i64 + 1;
    if s.estimator.kind == "har" {
        d = d.max(s.estimator.calibration_days as i64 + 2);
    }
    d
}

fn shift_date(date: &str, days: i64) -> Result<String> {
    Ok((chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")? - chrono::Duration::days(days)).format("%Y-%m-%d").to_string())
}

fn folds(cfg: &WalkForwardConfig, base: &Scenario) -> Result<Vec<Fold>> {
    let mut v = Vec::new();
    let mut push = |kind: FoldKind, idx: usize, f: &[String; 2]| -> Result<()> {
        let mut s = base.clone();
        s.from = f[0].clone();
        let mut data_from = shift_date(&f[0], warm_days(&s))?;
        if !cfg.calibration_from.is_empty() && data_from < cfg.calibration_from {
            data_from = cfg.calibration_from.clone();
        }
        let id = match kind {
            FoldKind::Train => format!("train-{}", idx + 1),
            FoldKind::Validation => format!("validation-{}", idx + 1),
            FoldKind::Holdout => "holdout".into(),
        };
        v.push(Fold { id, kind, from: f[0].clone(), to: f[1].clone(), data_from });
        Ok(())
    };
    for (i, f) in cfg.train.iter().enumerate() {
        push(FoldKind::Train, i, f)?;
    }
    for (i, f) in cfg.validation.iter().enumerate() {
        push(FoldKind::Validation, i, f)?;
    }
    if let Some(h) = &cfg.holdout {
        push(FoldKind::Holdout, 0, h)?;
    }
    Ok(v)
}

fn scenario_for(base: &Scenario, c: &Candidate, fold: &Fold) -> Result<Scenario> {
    let mut s = base.with_overrides(&c.overrides.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>())?;
    s.from = fold.from.clone();
    s.to = fold.to.clone();
    s.name = format!("{}-{}-{}", base.name, c.name, fold.id);
    Ok(s)
}

fn objective(cfg: &WalkForwardConfig, m: &Metric) -> f64 {
    match cfg.objective.as_str() {
        "net_return_after_idle_cost_annualized" => m.net_return_after_idle_cost_annualized,
        "desk_gross_return_annualized" => m.desk_gross_return_annualized,
        _ => m.depositor_net_return_annualized,
    }
}

/// Run the protocol. `run_fn` receives the fold's scenario (whose
/// `from`/`to` bound the data it may read) and the fold; calls for one
/// phase are independent and run in parallel.
pub fn run(cfg: &WalkForwardConfig, base: &Scenario, open_holdout: bool, threads: usize, run_fn: &(dyn Fn(&Scenario, &Fold) -> Result<Metric> + Sync)) -> Result<Manifest> {
    cfg.validate()?;
    let all = folds(cfg, base)?;
    let mut runs: Vec<RunRecord> = Vec::new();
    let mut seq = 0usize;
    let phase = |kind: FoldKind, cands: &[Candidate], runs: &mut Vec<RunRecord>, seq: &mut usize| -> Result<()> {
        let jobs: Vec<(Candidate, Fold)> = cands.iter().flat_map(|c| all.iter().filter(|f| f.kind == kind).map(move |f| (c.clone(), f.clone()))).collect();
        let outs = study::par_map(jobs, threads, |(c, f)| -> Result<(Candidate, Fold, Metric)> {
            let s = scenario_for(base, &c, &f)?;
            let m = run_fn(&s, &f)?;
            eprintln!("walkforward {} {:14} {:16} net {:+.4} dd {:.3} liq {} fills {}", cfg.name, c.name, f.id, m.depositor_net_return_annualized, m.max_drawdown, m.liquidations, m.fills);
            Ok((c, f, m))
        });
        for o in outs {
            let (c, f, m) = o?;
            runs.push(RunRecord { candidate: c.name, fold: f, sequence: *seq, metric: m });
            *seq += 1;
        }
        Ok(())
    };
    // 1. Training: every candidate, every training fold.
    phase(FoldKind::Train, &cfg.candidates, &mut runs, &mut seq)?;
    // 2. Selection on training folds only.
    let mut scores = Vec::new();
    for c in &cfg.candidates {
        let tr: Vec<&RunRecord> = runs.iter().filter(|r| r.candidate == c.name && r.fold.kind == FoldKind::Train).collect();
        let rets: Vec<f64> = tr.iter().map(|r| objective(cfg, &r.metric)).collect();
        let mut why = Vec::new();
        if tr.iter().any(|r| r.metric.liquidations > 0) {
            why.push("liquidation in a training fold".to_string());
        }
        if tr.iter().any(|r| r.metric.bankrupt) {
            why.push("bankrupt in a training fold".to_string());
        }
        let dd = tr.iter().map(|r| r.metric.max_drawdown).fold(0.0, f64::max);
        if dd > cfg.gate_max_drawdown {
            why.push(format!("training drawdown {dd:.3} > gate {}", cfg.gate_max_drawdown));
        }
        scores.push(CandidateScore {
            candidate: c.name.clone(),
            eligible: why.is_empty(),
            why_ineligible: why,
            train_score: rets.iter().sum::<f64>() / rets.len().max(1) as f64,
            train_folds: tr.iter().map(|r| r.fold.id.clone()).collect(),
            train_returns: rets,
            validation_returns: Vec::new(),
            validation_max_drawdown: 0.0,
            validation_liquidations: 0,
        });
    }
    let gate_failed_all = scores.iter().all(|s| !s.eligible);
    let pool: Vec<&CandidateScore> = if gate_failed_all { scores.iter().collect() } else { scores.iter().filter(|s| s.eligible).collect() };
    let best = pool.iter().max_by(|a, b| a.train_score.partial_cmp(&b.train_score).unwrap()).expect("candidates");
    let selection = Selection {
        candidate: best.candidate.clone(),
        score: best.train_score,
        based_on_folds: best.train_folds.clone(),
        selection_sequence: seq,
        gate_failed_all,
    };
    // 3. Validation: every candidate (reported, never re-ranked).
    phase(FoldKind::Validation, &cfg.candidates, &mut runs, &mut seq)?;
    for s in &mut scores {
        let va: Vec<&RunRecord> = runs.iter().filter(|r| r.candidate == s.candidate && r.fold.kind == FoldKind::Validation).collect();
        s.validation_returns = va.iter().map(|r| objective(cfg, &r.metric)).collect();
        s.validation_max_drawdown = va.iter().map(|r| r.metric.max_drawdown).fold(0.0, f64::max);
        s.validation_liquidations = va.iter().map(|r| r.metric.liquidations).sum();
    }
    let selected_validation: Vec<Metric> = runs.iter().filter(|r| r.candidate == selection.candidate && r.fold.kind == FoldKind::Validation).map(|r| r.metric.clone()).collect();
    // 4. Holdout: the selected candidate only, only when opened.
    let mut holdout = None;
    if open_holdout {
        if let Some(c) = cfg.candidates.iter().find(|c| c.name == selection.candidate) {
            let before = runs.len();
            phase(FoldKind::Holdout, std::slice::from_ref(c), &mut runs, &mut seq)?;
            holdout = runs[before..].first().map(|r| r.metric.clone());
        }
    }
    Ok(Manifest {
        name: cfg.name.clone(),
        objective: cfg.objective.clone(),
        gate_max_drawdown: cfg.gate_max_drawdown,
        calibration_from: cfg.calibration_from.clone(),
        folds: all,
        candidates: cfg.candidates.clone(),
        runs,
        scores,
        selection,
        ranked_on: vec!["train".to_string()],
        holdout_opened: open_holdout,
        holdout,
        validation_distribution_selected: study::distribution(&selected_validation),
    })
}

pub fn write(dir: &std::path::Path, m: &Manifest) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("manifest.json"), serde_json::to_string_pretty(m)?)?;
    let mut csv = String::from("candidate,fold,kind,from,to,data_from,sequence,depositor_net_return_annualized,max_drawdown,liquidations,fills,nav_end,mean_vol_bias,hedge_turnover_nav_per_30d,bankrupt\n");
    for r in &m.runs {
        let x = &r.metric;
        csv.push_str(&format!(
            "{},{},{:?},{},{},{},{},{:.5},{:.4},{},{},{:.2},{:.4},{:.3},{}\n",
            r.candidate, r.fold.id, r.fold.kind, r.fold.from, r.fold.to, r.fold.data_from, r.sequence, x.depositor_net_return_annualized, x.max_drawdown, x.liquidations, x.fills, x.nav_end, x.mean_vol_bias, x.hedge_turnover_nav_per_30d, x.bankrupt
        ));
    }
    std::fs::write(dir.join("runs.csv"), csv)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn cfg() -> WalkForwardConfig {
        WalkForwardConfig {
            name: "t".into(),
            calibration_from: "2024-01-01".into(),
            train: vec![["2024-06-01".into(), "2024-11-30".into()], ["2024-12-01".into(), "2025-05-31".into()]],
            validation: vec![["2025-06-01".into(), "2025-11-30".into()]],
            holdout: Some(["2025-12-01".into(), "2026-06-30".into()]),
            candidates: vec![
                Candidate { name: "a".into(), overrides: [("estimator.q_bid".to_string(), toml::Value::Float(0.25))].into_iter().collect() },
                Candidate { name: "b".into(), overrides: [("estimator.q_bid".to_string(), toml::Value::Float(0.45))].into_iter().collect() },
            ],
            ..Default::default()
        }
    }

    /// P5 gate: no parameter is selected using future or holdout data.
    /// The fake run function records every call; the selection must
    /// happen after only training runs, every training fold must end
    /// before validation begins, and the holdout stays sealed without
    /// the flag.
    #[test]
    fn selection_reads_only_past_folds_and_the_holdout_stays_sealed() {
        let trace: Mutex<Vec<(String, String, String, String)>> = Mutex::new(Vec::new());
        let fake = |s: &Scenario, f: &Fold| -> Result<Metric> {
            trace.lock().unwrap().push((s.name.clone(), f.id.clone(), s.from.clone(), s.to.clone()));
            // Candidate b looks better on the holdout ONLY; a wins on train.
            let ret = match (s.estimator.q_bid, &f.kind) {
                (q, FoldKind::Holdout) if q > 0.4 => 9.0,
                (q, _) if q < 0.3 => 0.30,
                _ => 0.20,
            };
            Ok(Metric { depositor_net_return_annualized: ret, required_return: 0.12, hurdle_pass: ret >= 0.12, ..Default::default() })
        };
        let c = cfg();
        let base = Scenario::default();
        let m = run(&c, &base, false, 2, &fake).unwrap();
        assert_eq!(m.selection.candidate, "a");
        assert_eq!(m.ranked_on, vec!["train"]);
        assert!(!m.holdout_opened && m.holdout.is_none());
        // Selection happened after exactly the training runs.
        let n_train = c.candidates.len() * c.train.len();
        assert_eq!(m.selection.selection_sequence, n_train);
        let t = trace.lock().unwrap();
        assert_eq!(t.len(), n_train + c.candidates.len() * c.validation.len());
        for (i, (_, id, _, to)) in t.iter().enumerate() {
            if i < n_train {
                assert!(id.starts_with("train-"), "{id} ran before selection");
                assert!(to.as_str() < c.validation[0][0].as_str(), "training fold {id} ends {to}, inside validation");
            } else {
                assert!(id.starts_with("validation-"));
            }
            assert!(!id.starts_with("holdout"), "holdout ran without the flag");
        }
        assert!(m.selection.based_on_folds.iter().all(|f| f.starts_with("train-")));
        // Every fold's data window starts at or after calibration_from and
        // ends at the fold end: nothing after `to` is readable.
        for f in &m.folds {
            assert!(f.data_from >= c.calibration_from && f.data_from <= f.from && f.from <= f.to);
        }
        drop(t);
        // Opened: only the SELECTED candidate runs on the holdout, once,
        // and the selection is unchanged even though b "wins" there.
        let m2 = run(&c, &base, true, 2, &fake).unwrap();
        assert!(m2.holdout_opened);
        assert_eq!(m2.selection.candidate, "a");
        let hold: Vec<&RunRecord> = m2.runs.iter().filter(|r| r.fold.kind == FoldKind::Holdout).collect();
        assert_eq!(hold.len(), 1);
        assert_eq!(hold[0].candidate, "a");
        assert!(m2.holdout.is_some());
    }

    #[test]
    fn gate_disqualifies_liquidations_and_non_chronological_splits_are_rejected() {
        let fake = |s: &Scenario, _f: &Fold| -> Result<Metric> {
            let liq = if s.estimator.q_bid < 0.3 { 1 } else { 0 };
            Ok(Metric { depositor_net_return_annualized: if liq > 0 { 0.9 } else { 0.1 }, liquidations: liq, required_return: 0.12, ..Default::default() })
        };
        let m = run(&cfg(), &Scenario::default(), false, 1, &fake).unwrap();
        assert_eq!(m.selection.candidate, "b", "the liquidating candidate is ineligible however good its return");
        assert!(!m.selection.gate_failed_all);
        assert!(m.scores.iter().find(|s| s.candidate == "a").unwrap().why_ineligible[0].contains("liquidation"));
        let mut bad = cfg();
        bad.validation = vec![["2024-01-15".into(), "2024-03-01".into()]];
        assert!(bad.validate().is_err());
    }
}
