//! Report writers (doc 06 §10): per-round CSV, summary JSON, frozen
//! scenario copy, and the markdown comparison table that picks launch
//! parameters.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use vault_sim::types::RoundRecord;

use crate::run::{rank_value, CellSummary};

pub fn write_rounds_csv(path: &Path, records: &[RoundRecord]) -> Result<()> {
    let mut w = csv::Writer::from_path(path)
        .with_context(|| format!("creating {}", path.display()))?;
    for r in records {
        w.serialize(r)?;
    }
    w.flush()?;
    Ok(())
}

pub fn write_summary_json(path: &Path, summaries: &[CellSummary]) -> Result<()> {
    fs::write(path, serde_json::to_string_pretty(summaries)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn read_summary_json(path: &Path) -> Result<Vec<CellSummary>> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(serde_json::from_str(&raw)?)
}

fn fmt_pct(x: f64) -> String {
    format!("{:.2}%", x * 100.0)
}

/// The launch-parameter comparison table: top `top` cells by `rank_key`,
/// descending.
pub fn markdown_table(summaries: &[CellSummary], rank_key: &str, top: usize) -> Result<String> {
    let mut rows: Vec<&CellSummary> = summaries.iter().collect();
    let mut err = None;
    rows.sort_by(|a, b| {
        let va = rank_value(a, rank_key).unwrap_or_else(|e| {
            err = Some(e);
            f64::MIN
        });
        let vb = rank_value(b, rank_key).unwrap_or(f64::MIN);
        vb.partial_cmp(&va).unwrap_or(std::cmp::Ordering::Equal)
    });
    if let Some(e) = err {
        return Err(e);
    }

    let mut md = String::new();
    md.push_str(&format!("| # | Δ | slices | π | iv | mech | haircut | fees (m/p) | slip | {rank_key} | APY(USD) | APY p5 | call-away | maxDD p95 | vs HODL | P(<HODL) | unsold |\n"));
    md.push_str("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n");
    for (i, s) in rows.iter().take(top).enumerate() {
        let c = &s.cell;
        let rank_cell = format!("{:.4}", rank_value(s, rank_key)?);
        md.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {}/{} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            i + 1,
            c.delta_target,
            c.slices,
            c.premium_policy,
            c.iv_provider,
            c.mechanism,
            c.haircut_bps,
            c.mgmt_bps_annual,
            c.perf_bps,
            c.slippage_bps,
            rank_cell,
            fmt_pct(s.apy_usd_mean),
            fmt_pct(s.apy_usd_p5),
            fmt_pct(s.callaway_freq_mean),
            fmt_pct(s.max_drawdown_usd_p95),
            fmt_pct(s.vs_hodl_mean),
            fmt_pct(s.p_underperform_hodl),
            fmt_pct(s.unsold_rate_mean),
        ));
    }
    Ok(md)
}
