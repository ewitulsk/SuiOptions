//! Wires one scenario cell to the vault-sim engine and aggregates
//! per-path summaries into the per-cell row the sweep report ranks.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use vault_sim::engine::{run_path, EngineParams, FlowSchedule, PremiumPolicy};
use vault_sim::exercise::{EarlyIntrinsic, ExercisePolicy, Partial, RationalExpiry};
use vault_sim::iv::{BetaScaled, DeribitDvol, IvProvider, MarketCtx, RealizedOnly, VrpTransfer};
use vault_sim::ledger::LedgerConfig;
use vault_sim::metrics::{summarize, Summary};
use vault_sim::paths::{Bar, Garch11, GbmJump, Path, PathSource, Replay, StationaryBootstrap};
use vault_sim::premium::BlackScholesMid;
use vault_sim::sale::{OnchainAuction, Pessimist, RfqBatch, SaleMechanism};
use vault_sim::strategy::{StrikeSelector, BTC_LADDER, SUI_LADDER};
use vault_sim::types::{RoundRecord, UnderlyingAmt};

use crate::data;
use crate::scenario::{parse_exercise, Cell, ExerciseSpec, Meta};

const BARS_PER_YEAR: f64 = 365.0;
const DAY_MS: u64 = 86_400_000;

/// (underlying_decimals, settlement_decimals, ladder) per asset.
fn asset_profile(asset: &str) -> Result<(u8, u8, Vec<f64>)> {
    Ok(match asset {
        "SUI" => (9, 6, SUI_LADDER.to_vec()),
        "BTC" => (8, 6, BTC_LADDER.to_vec()),
        // ETH is validation-only; wBTC-style decimals + the wide ladder.
        "ETH" => (8, 6, BTC_LADDER.to_vec()),
        other => bail!("unknown asset {other}"),
    })
}

/// Builds the path set once per scenario; every cell replays the same
/// paths so cells are comparable.
pub fn build_paths(meta: &Meta) -> Result<Vec<Path>> {
    let history = data::load_candles(std::path::Path::new(&meta.candles))?;
    let horizon = meta.rounds * meta.round_bars;
    let mut source: Box<dyn PathSource> = match meta.paths.as_str() {
        "replay" => Box::new(Replay::new(history)),
        "bootstrap" => Box::new(StationaryBootstrap::new(
            &history, 10.0, horizon, meta.n_paths, meta.seed,
        )),
        "gbm_jump" => {
            let (mu, sigma, intensity, jmean, jstd) = fit_gbm_jump(&history);
            Box::new(GbmJump::new(
                history.last().unwrap().close,
                mu,
                sigma,
                intensity,
                jmean,
                jstd,
                horizon,
                meta.n_paths,
                meta.seed,
            ))
        }
        "garch" => {
            let (mu_d, var_d) = daily_moments(&history);
            // Variance targeting with conventional crypto persistence
            // (α = 0.1, β = 0.85); a full MLE fit is not worth the
            // dependency for a cross-check generator.
            let (alpha, beta) = (0.1, 0.85);
            Box::new(Garch11::new(
                history.last().unwrap().close,
                mu_d,
                var_d * (1.0 - alpha - beta),
                alpha,
                beta,
                horizon,
                meta.n_paths,
                meta.seed,
            ))
        }
        other => bail!("unknown path source {other}"),
    };
    let mut paths = Vec::new();
    while let Some(p) = source.next_path() {
        paths.push(p);
    }
    Ok(paths)
}

fn daily_moments(history: &[Bar]) -> (f64, f64) {
    let rets: Vec<f64> =
        history.windows(2).map(|w| (w[1].close / w[0].close).ln()).collect();
    let mean = rets.iter().sum::<f64>() / rets.len() as f64;
    let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (rets.len() - 1) as f64;
    (mean, var)
}

/// Moment fit: jumps = daily moves beyond 3σ of the remainder.
fn fit_gbm_jump(history: &[Bar]) -> (f64, f64, f64, f64, f64) {
    let rets: Vec<f64> =
        history.windows(2).map(|w| (w[1].close / w[0].close).ln()).collect();
    let (mean, var) = daily_moments(history);
    let sd = var.sqrt();
    let (jumps, body): (Vec<f64>, Vec<f64>) =
        rets.iter().partition(|r| (**r - mean).abs() > 3.0 * sd);
    let body_mean = body.iter().sum::<f64>() / body.len().max(1) as f64;
    let body_var = body.iter().map(|r| (r - body_mean).powi(2)).sum::<f64>()
        / (body.len().saturating_sub(1)).max(1) as f64;
    let intensity = jumps.len() as f64 / rets.len() as f64 * BARS_PER_YEAR;
    let jmean = if jumps.is_empty() {
        0.0
    } else {
        jumps.iter().sum::<f64>() / jumps.len() as f64
    };
    let jstd = if jumps.len() < 2 {
        0.0
    } else {
        (jumps.iter().map(|r| (r - jmean).powi(2)).sum::<f64>() / (jumps.len() - 1) as f64)
            .sqrt()
    };
    (
        mean * BARS_PER_YEAR,
        (body_var * BARS_PER_YEAR).sqrt(),
        intensity,
        jmean,
        jstd,
    )
}

/// Uniform skew adjustment on top of any provider (the swept `skew_bps`).
struct SkewAdj<P> {
    inner: P,
    skew_bps: i64,
}

impl<P: IvProvider> IvProvider for SkewAdj<P> {
    fn iv(&self, ctx: &MarketCtx, tenor_years: f64, delta: f64) -> f64 {
        self.inner.iv(ctx, tenor_years, delta) * (1.0 + self.skew_bps as f64 / 10_000.0)
    }
}

fn build_iv(meta: &Meta, cell: &Cell, paths: &[Path]) -> Result<Box<dyn IvProvider>> {
    let window = meta.rv_window;
    let inner: Box<dyn IvProvider> = match cell.iv_provider.as_str() {
        "realized_only" => {
            Box::new(RealizedOnly { window, bars_per_year: BARS_PER_YEAR })
        }
        "vrp_transfer" => Box::new(VrpTransfer {
            window,
            bars_per_year: BARS_PER_YEAR,
            vrp_ratio: cell.vrp_ratio,
        }),
        "dvol" => {
            let file = meta.dvol.as_ref().context("dvol provider needs scenario.dvol")?;
            anyhow::ensure!(
                meta.paths == "replay",
                "dvol provider is replay-only (the series aligns to real bars)"
            );
            let dvol = data::load_vol_index_aligned(
                std::path::Path::new(file),
                &paths[0].bars,
            )?;
            Box::new(DeribitDvol { dvol, skew_bps: 0 })
        }
        "beta_scaled" => {
            let ref_candles = meta
                .ref_candles
                .as_ref()
                .context("beta_scaled needs scenario.ref_candles")?;
            let ref_dvol =
                meta.ref_dvol.as_ref().context("beta_scaled needs scenario.ref_dvol")?;
            anyhow::ensure!(meta.paths == "replay", "beta_scaled is replay-only");
            let ref_bars = data::load_candles(std::path::Path::new(ref_candles))?;
            anyhow::ensure!(
                ref_bars.len() >= paths[0].bars.len(),
                "reference candles shorter than the asset's history"
            );
            let ref_iv = data::load_vol_index_aligned(
                std::path::Path::new(ref_dvol),
                &paths[0].bars,
            )?;
            Box::new(BetaScaled {
                window,
                bars_per_year: BARS_PER_YEAR,
                ref_iv,
                ref_closes: ref_bars.iter().map(|b| b.close).collect(),
            })
        }
        other => bail!("unknown iv_provider {other}"),
    };
    Ok(Box::new(SkewAdj { inner, skew_bps: cell.skew_bps }))
}

fn build_sale(cell: &Cell, seed: u64) -> Result<Box<dyn SaleMechanism>> {
    Ok(match cell.mechanism.as_str() {
        "rfq_batch" => Box::new(RfqBatch { haircut_bps: cell.haircut_bps }),
        "onchain_auction" => Box::new(OnchainAuction::new(
            cell.n_bidders_mean,
            cell.haircut_bps, // mean bid markdown
            cell.reserve_bps,
            cell.no_show_prob,
            50, // min increment, the doc 02 suggested default
            seed,
        )),
        "pessimist" => Box::new(Pessimist::new(cell.fill_prob, cell.haircut_bps, seed)),
        other => bail!("unknown mechanism {other}"),
    })
}

fn build_exercise(cell: &Cell) -> Result<Box<dyn ExercisePolicy>> {
    Ok(match parse_exercise(&cell.exercise)? {
        ExerciseSpec::Rational => Box::new(RationalExpiry),
        ExerciseSpec::Early { thresh_bps } => Box::new(EarlyIntrinsic { thresh_bps }),
        ExerciseSpec::Partial { frac } => Box::new(Partial { frac }),
    })
}

fn engine_params(meta: &Meta, cell: &Cell) -> Result<EngineParams> {
    let (under_dec, settle_dec, _) = asset_profile(&meta.asset)?;
    let premium_policy = match cell.premium_policy.as_str() {
        "at_roll" => PremiumPolicy::AtRoll,
        "at_expiry" => PremiumPolicy::AtExpiry,
        "hold_usdc" => PremiumPolicy::HoldUsdc,
        other => bail!("unknown premium_policy {other}"),
    };
    Ok(EngineParams {
        underlying_decimals: under_dec,
        settlement_decimals: settle_dec,
        round_bars: meta.round_bars,
        bars_per_year: BARS_PER_YEAR,
        r: meta.r,
        deploy_fraction: cell.deploy_fraction,
        slices: cell.slices,
        premium_policy,
        rv_window: meta.rv_window,
        initial_deposit: UnderlyingAmt(
            (meta.initial_tokens * 10f64.powi(under_dec as i32)) as u64,
        ),
        ledger: LedgerConfig {
            mgmt_fee_bps_annual: cell.mgmt_bps_annual,
            perf_fee_bps: cell.perf_bps,
            round_ms: meta.round_bars as u64 * DAY_MS,
        },
        swap_slippage_bps: cell.slippage_bps,
        flows: FlowSchedule {
            deposit_per_round: (meta.deposit_tokens_per_round
                * 10f64.powi(under_dec as i32)) as u64,
            withdraw_fraction_bps: meta.withdraw_fraction_bps,
            stress_round: meta.stress_round,
            stress_fraction_bps: meta.stress_fraction_bps,
        },
    })
}

/// Per-cell aggregate across paths — one row of the sweep report.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CellSummary {
    pub cell: Cell,
    pub n_paths: usize,
    pub rounds_per_path: f64,
    pub apy_underlying_mean: f64,
    pub apy_usd_mean: f64,
    /// 5th percentile of per-path USD APY — the sweep's default rank key.
    pub apy_usd_p5: f64,
    pub premium_yield_p50_mean: f64,
    pub callaway_freq_mean: f64,
    pub max_drawdown_usd_p95: f64,
    pub sharpe_mean: f64,
    pub es95_mean: f64,
    pub vs_hodl_mean: f64,
    /// Fraction of paths that underperformed holding the underlying.
    pub p_underperform_hodl: f64,
    pub unsold_rate_mean: f64,
    pub total_fees_mean: f64,
}

pub struct CellRun {
    pub summary: CellSummary,
    /// Round records of the first path (the full dump for replay runs).
    pub first_path_records: Vec<RoundRecord>,
}

pub fn run_cell(meta: &Meta, cell_idx: usize, cell: &Cell, paths: &[Path]) -> Result<CellRun> {
    let (_, _, ladder) = asset_profile(&meta.asset)?;
    let params = engine_params(meta, cell)?;
    let selector = StrikeSelector {
        delta_target: cell.delta_target,
        ladder,
        r: meta.r,
        vol_floor: meta.vol_floor,
        vol_ceiling: meta.vol_ceiling,
    };
    let iv = build_iv(meta, cell, paths)?;
    let rounds_per_year = BARS_PER_YEAR / meta.round_bars as f64;

    let mut summaries: Vec<Summary> = Vec::with_capacity(paths.len());
    let mut first_records = Vec::new();
    for path in paths {
        // Deterministic per (cell, path) so sweep rows are reproducible
        // independent of execution order.
        let seed = meta
            .seed
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(cell_idx as u64)
            .wrapping_add(path.seed.wrapping_mul(1_000_003));
        let mut sale = build_sale(cell, seed)?;
        let mut exercise = build_exercise(cell)?;
        let records = run_path(
            &params,
            &selector,
            path,
            iv.as_ref(),
            &BlackScholesMid,
            sale.as_mut(),
            exercise.as_mut(),
        );
        summaries.push(summarize(&records, rounds_per_year));
        if first_records.is_empty() {
            first_records = records;
        }
    }

    let n = summaries.len();
    let mean = |f: &dyn Fn(&Summary) -> f64| -> f64 {
        summaries.iter().map(f).sum::<f64>() / n.max(1) as f64
    };
    let mut apys: Vec<f64> = summaries.iter().map(|s| s.apy_usd).collect();
    apys.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut dds: Vec<f64> = summaries.iter().map(|s| s.max_drawdown_usd).collect();
    dds.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |sorted: &[f64], p: f64| -> f64 {
        if sorted.is_empty() {
            return 0.0;
        }
        sorted[((p * (sorted.len() - 1) as f64).round() as usize).min(sorted.len() - 1)]
    };

    Ok(CellRun {
        summary: CellSummary {
            cell: cell.clone(),
            n_paths: n,
            rounds_per_path: mean(&|s| s.rounds as f64),
            apy_underlying_mean: mean(&|s| s.apy_underlying),
            apy_usd_mean: mean(&|s| s.apy_usd),
            apy_usd_p5: pct(&apys, 0.05),
            premium_yield_p50_mean: mean(&|s| s.premium_yield_p50),
            callaway_freq_mean: mean(&|s| s.callaway_freq),
            max_drawdown_usd_p95: pct(&dds, 0.95),
            sharpe_mean: mean(&|s| s.sharpe),
            es95_mean: mean(&|s| s.es95),
            vs_hodl_mean: mean(&|s| s.vs_hodl),
            p_underperform_hodl: summaries.iter().filter(|s| s.vs_hodl < 0.0).count()
                as f64
                / n.max(1) as f64,
            unsold_rate_mean: mean(&|s| s.unsold_rate),
            total_fees_mean: mean(&|s| s.total_fees as f64),
        },
        first_path_records: first_records,
    })
}

/// Rank key → value, for `sweep --rank`.
pub fn rank_value(s: &CellSummary, key: &str) -> Result<f64> {
    Ok(match key {
        "apy_p5" => s.apy_usd_p5,
        "apy" | "apy_usd" => s.apy_usd_mean,
        "apy_underlying" => s.apy_underlying_mean,
        "sharpe" => s.sharpe_mean,
        "vs_hodl" => s.vs_hodl_mean,
        "es95" => s.es95_mean,
        other => bail!("unknown rank key {other} (apy_p5 | apy | apy_underlying | sharpe | vs_hodl | es95)"),
    })
}
