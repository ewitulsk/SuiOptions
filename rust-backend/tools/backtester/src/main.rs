//! Covered-call vault backtester (doc 06 §10).
//!
//! ```text
//! backtester run    --scenario scenarios/sui_launch.toml --out out/sui_launch/
//! backtester sweep  --scenario scenarios/sweep.toml --out out/sweep/ --top 20 --rank apy_p5
//! backtester report --in out/sweep/ --format md --top 20 --rank apy_p5
//! ```
//!
//! `run` executes every cell sequentially and dumps per-round CSVs (meant
//! for single-cell scenarios); `sweep` runs the cartesian product in
//! parallel and writes the ranked comparison table; `report` re-renders
//! the table from an existing `summary.json`.

mod data;
mod report;
mod run;
mod scenario;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rayon::prelude::*;

use run::{run_cell, CellSummary};

#[derive(Parser)]
#[command(name = "backtester", about = "Covered-call vault backtesting engine")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run every cell of a scenario sequentially, dumping per-round CSVs.
    Run {
        #[arg(long)]
        scenario: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Run the full cartesian sweep in parallel; write summary + ranking.
    Sweep {
        #[arg(long)]
        scenario: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// Rows in the markdown table.
        #[arg(long, default_value_t = 20)]
        top: usize,
        /// apy_p5 | apy | apy_underlying | sharpe | vs_hodl | es95
        #[arg(long, default_value = "apy_p5")]
        rank: String,
    },
    /// Re-render the comparison table from an existing run directory.
    Report {
        #[arg(long = "in")]
        in_dir: PathBuf,
        #[arg(long, default_value = "md")]
        format: String,
        #[arg(long, default_value_t = 20)]
        top: usize,
        #[arg(long, default_value = "apy_p5")]
        rank: String,
    },
    /// Calibrate the IV/RV ratio (the keeper's iv_ratio and the
    /// vrp_transfer provider's vrp_ratio): vol-index / trailing realized
    /// vol over the overlap of the two series (doc 06 §5).
    Calibrate {
        #[arg(long)]
        candles: PathBuf,
        #[arg(long)]
        dvol: PathBuf,
        /// Realized-vol window, bars.
        #[arg(long, default_value_t = 30)]
        window: usize,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Run { scenario, out } => cmd_run(&scenario, &out, false, 20, "apy_p5"),
        Command::Sweep { scenario, out, top, rank } => {
            cmd_run(&scenario, &out, true, top, &rank)
        }
        Command::Report { in_dir, format, top, rank } => cmd_report(&in_dir, &format, top, &rank),
        Command::Calibrate { candles, dvol, window } => cmd_calibrate(&candles, &dvol, window),
    }
}

fn cmd_calibrate(candles: &Path, dvol: &Path, window: usize) -> Result<()> {
    let bars = data::load_candles(candles)?;
    let iv = data::load_vol_index_aligned(dvol, &bars)?;
    // Only the true overlap: bars before the index's first print would
    // otherwise compare backfilled IV against real RV.
    let first_iv_ts = data::load_vol_index(dvol)?[0].0;
    let mut ratios: Vec<f64> = Vec::new();
    for idx in window..bars.len() {
        if bars[idx].ts_ms < first_iv_ts {
            continue;
        }
        let rv = vault_sim::iv::realized_vol(&bars, idx, window, 365.0);
        if rv > 0.0 {
            ratios.push(iv[idx] / rv);
        }
    }
    anyhow::ensure!(!ratios.is_empty(), "no overlapping IV/RV samples");
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| ratios[((p * (ratios.len() - 1) as f64).round() as usize).min(ratios.len() - 1)];
    println!(
        "IV/RV{} over {} samples: p25 {:.3}  median {:.3}  p75 {:.3}",
        window,
        ratios.len(),
        pct(0.25),
        pct(0.50),
        pct(0.75),
    );
    println!(
        "→ keeper iv_ratio / scenario vrp_ratio candidate: {:.2}",
        pct(0.50)
    );
    Ok(())
}

fn cmd_run(
    scenario_path: &Path,
    out: &Path,
    parallel: bool,
    top: usize,
    rank: &str,
) -> Result<()> {
    let toml_str = fs::read_to_string(scenario_path)
        .with_context(|| format!("reading {}", scenario_path.display()))?;
    let sc = scenario::parse(&toml_str)?;
    fs::create_dir_all(out)?;
    // Freeze the resolved scenario alongside the results.
    fs::write(out.join("config.toml"), &toml_str)?;

    eprintln!(
        "{} cells × {} path source ({}), asset {}",
        sc.cells.len(),
        sc.meta.paths,
        sc.meta.candles,
        sc.meta.asset
    );
    let paths = run::build_paths(&sc.meta)?;
    eprintln!("{} path(s), {} bars each", paths.len(), paths[0].bars.len());
    let seeds: Vec<u64> = paths.iter().map(|p| p.seed).collect();
    fs::write(out.join("seeds.json"), serde_json::to_string(&seeds)?)?;

    let run_one = |(idx, cell): (usize, &scenario::Cell)| -> Result<(usize, run::CellRun)> {
        let r = run_cell(&sc.meta, idx, cell, &paths)?;
        Ok((idx, r))
    };

    let mut results: Vec<(usize, run::CellRun)> = if parallel {
        sc.cells
            .par_iter()
            .enumerate()
            .map(run_one)
            .collect::<Result<Vec<_>>>()?
    } else {
        sc.cells.iter().enumerate().map(run_one).collect::<Result<Vec<_>>>()?
    };
    results.sort_by_key(|(idx, _)| *idx);

    let mut summaries: Vec<CellSummary> = Vec::with_capacity(results.len());
    for (idx, r) in results {
        // Per-round dump: every cell for `run`, skipped for sweeps (the
        // table is the product there).
        if !parallel {
            report::write_rounds_csv(
                &out.join(format!("rounds_cell{idx:04}.csv")),
                &r.first_path_records,
            )?;
        }
        summaries.push(r.summary);
    }
    report::write_summary_json(&out.join("summary.json"), &summaries)?;

    let md = report::markdown_table(&summaries, rank, top)?;
    fs::write(out.join("report.md"), &md)?;
    println!("{md}");
    eprintln!("wrote {}", out.display());
    Ok(())
}

fn cmd_report(in_dir: &Path, format: &str, top: usize, rank: &str) -> Result<()> {
    anyhow::ensure!(format == "md", "only --format md is supported");
    let summaries = report::read_summary_json(&in_dir.join("summary.json"))?;
    println!("{}", report::markdown_table(&summaries, rank, top)?);
    Ok(())
}
