//! `desk-backtester run --scenario s.toml --store file:///lake --out dir`
//! `desk-backtester sweep --scenario s.toml --store … --out dir --bands 1.5,5,20 --risk-premiums 0,0.05 --max-leans 0,0.8`
//! `desk-backtester capacity --scenario s.toml --store … --out dir --volumes 10000,25000,… --mixes call_only,put_only,balanced,adversarial --seeds 8`
//! `desk-backtester market --scenario s.toml --store … --out dir --spreads 0.03,0.05,0.08 --seeds 8`

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use desk_backtester::{data, engine, report, scenario::Scenario, solver};

#[derive(Parser)]
struct Cli {
    /// `s3://bucket` (env creds/endpoint) or `file:///path` lake root.
    #[arg(long, env = "STORE_URL", global = true, default_value = "")]
    store: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Run {
        #[arg(long)]
        scenario: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    Sweep {
        #[arg(long)]
        scenario: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, value_delimiter = ',')]
        bands: Vec<f64>,
        #[arg(long, value_delimiter = ',')]
        risk_premiums: Vec<f64>,
        #[arg(long, value_delimiter = ',')]
        max_leans: Vec<f64>,
        #[arg(long, value_delimiter = ',')]
        sample_intervals: Vec<i64>,
    },
    /// Capacity mode (doc 08 §8.1/§8.6): minimum starting NAV per target
    /// accepted Earn notional per day and mix; writes `frontier.csv`.
    Capacity {
        #[arg(long)]
        scenario: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// Target accepted spot notional per day (default: the §8.1 log sweep).
        #[arg(long, value_delimiter = ',')]
        volumes: Vec<f64>,
        /// call_only,put_only,balanced,adversarial (default: all four).
        #[arg(long, value_delimiter = ',')]
        mixes: Vec<String>,
        #[arg(long, default_value_t = 8)]
        seeds: u64,
        #[arg(long, default_value_t = 1_000.0)]
        nav_lo: f64,
        #[arg(long, default_value_t = 1.0e9)]
        nav_hi: f64,
    },
    /// Market mode (doc 08 §8.1): offered flow and acceptance against the
    /// actual bid at the scenario NAV, over a sweep of base spreads.
    Market {
        #[arg(long)]
        scenario: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// Base spreads in vol points (default: the scenario's).
        #[arg(long, value_delimiter = ',')]
        spreads: Vec<f64>,
        #[arg(long, default_value_t = 8)]
        seeds: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    anyhow::ensure!(!cli.store.is_empty(), "--store or STORE_URL required");
    let store = data::open_store(&cli.store)?;
    match cli.cmd {
        Cmd::Run { scenario, out } => {
            let s = Scenario::load(&scenario)?;
            let (bars, funding, index) = load(&store, &s).await?;
            let o = engine::run(&s, &bars, &funding, &index)?;
            let summary = report::summarize(&s, &o);
            report::write_all(&out, &s, &o, &summary)?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        Cmd::Sweep { scenario, out, bands, risk_premiums, max_leans, sample_intervals } => {
            let base = Scenario::load(&scenario)?;
            let (bars, funding, index) = load(&store, &base).await?;
            let bands = if bands.is_empty() { vec![base.hedge.band_pct_nav] } else { bands };
            let rps = if risk_premiums.is_empty() { vec![base.estimator.risk_premium] } else { risk_premiums };
            let leans = if max_leans.is_empty() { vec![base.estimator.max_lean] } else { max_leans };
            let ivs = if sample_intervals.is_empty() { vec![base.estimator.sample_interval_s] } else { sample_intervals };
            std::fs::create_dir_all(&out)?;
            let mut csv = String::from("band_pct_nav,risk_premium,max_lean,sample_interval_s,fills,turns,coverage,nav_end,desk_gross_return,depositor_net_return_annualized,max_drawdown,hedge_turnover_nav_per_30d,hedge_fees,hedge_slippage,funding_paid,premium_paid,option_payoff,hedge_realized,mean_sigma_paid,mean_sigma_realized,mean_vol_bias,vol_pnl_proxy_total,hash\n");
            for &b in &bands {
                for &rp in &rps {
                    for &ml in &leans {
                        for &iv in &ivs {
                            let mut s = base.clone();
                            s.hedge.band_pct_nav = b;
                            s.hedge.band_wide_pct_nav = b * base.hedge.band_wide_pct_nav / base.hedge.band_pct_nav.max(1e-9);
                            s.estimator.risk_premium = rp;
                            s.estimator.max_lean = ml;
                            s.estimator.sample_interval_s = iv;
                            s.name = format!("{}-b{b}-rp{rp}-ml{ml}-iv{iv}", base.name);
                            let o = engine::run(&s, &bars, &funding, &index)?;
                            let m = report::summarize(&s, &o);
                            report::write_all(&out.join(&s.name), &s, &o, &m)?;
                            csv.push_str(&format!(
                                "{b},{rp},{ml},{iv},{},{},{:.4},{:.2},{:.5},{:.5},{:.4},{:.3},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.4},{:.4},{:.4},{:.2},{}\n",
                                m.fills, m.turns, m.coverage, m.nav_end, m.desk_gross_return, m.depositor_net_return_annualized,
                                m.max_drawdown, m.hedge_turnover_nav_per_30d, m.hedge_fees, m.hedge_slippage, m.funding_paid,
                                m.premium_paid, m.option_payoff, m.hedge_realized, m.mean_sigma_paid, m.mean_sigma_realized,
                                m.mean_vol_bias, m.vol_pnl_proxy_total, m.determinism_hash
                            ));
                            eprintln!("{}: nav_end {:.0} turnover {:.2}×/30d bias {:+.4}", s.name, m.nav_end, m.hedge_turnover_nav_per_30d, m.mean_vol_bias);
                        }
                    }
                }
            }
            std::fs::write(out.join("sweep.csv"), &csv)?;
            print!("{csv}");
        }
        Cmd::Capacity { scenario, out, volumes, mixes, seeds, nav_lo, nav_hi } => {
            let base = Scenario::load(&scenario)?;
            let (bars, funding, index) = load(&store, &base).await?;
            let data = solver::Data { bars: &bars, funding: &funding, vol_index: &index };
            let volumes = if volumes.is_empty() { solver::default_volumes() } else { volumes };
            let mixes: Vec<solver::Mix> = if mixes.is_empty() {
                vec![solver::Mix::CallOnly, solver::Mix::PutOnly, solver::Mix::Balanced, solver::Mix::Adversarial]
            } else {
                mixes.iter().map(|m| solver::Mix::parse(m)).collect::<Result<_>>()?
            };
            let cfg = solver::SolverConfig { nav_lo, nav_hi, seeds: (1..=seeds).collect(), ..Default::default() };
            std::fs::create_dir_all(&out)?;
            let results = solver::capacity_sweep(&base, &data, &volumes, &mixes, &cfg, Some(&out))?;
            print!("{}", solver::capacity_frontier_csv(&results));
        }
        Cmd::Market { scenario, out, spreads, seeds } => {
            let base = Scenario::load(&scenario)?;
            let (bars, funding, index) = load(&store, &base).await?;
            let data = solver::Data { bars: &bars, funding: &funding, vol_index: &index };
            let spreads = if spreads.is_empty() { vec![base.bid.base_spread_volpts] } else { spreads };
            let seeds: Vec<u64> = (1..=seeds).collect();
            let results = solver::market_sweep(&base, &data, &spreads, &seeds, Some(&out))?;
            print!("{}", solver::market_frontier_csv(&results));
        }
    }
    Ok(())
}

async fn load(store: &data::Store, s: &Scenario) -> Result<(Vec<data::Bar>, Vec<data::FundingRow>, Vec<(i64, f64)>)> {
    // Warm the estimator: read the long window before `from`.
    let mut warm_days = (s.estimator.long_window_hours / 24.0).ceil() as i64 + 1;
    if s.estimator.kind == "har" {
        warm_days = warm_days.max(s.estimator.calibration_days as i64 + 2);
    }
    let from = (chrono::NaiveDate::parse_from_str(&s.from, "%Y-%m-%d")? - chrono::Duration::days(warm_days)).format("%Y-%m-%d").to_string();
    let bars = data::load_bars(store, &s.spot_exchange, &s.spot_symbol, &from, &s.to).await?;
    let funding = data::load_funding(store, &s.funding_exchange, &s.funding_symbol, &s.from, &s.to).await?;
    let index = if s.vol_index_symbol.is_empty() {
        Vec::new()
    } else {
        data::load_vol_index(store, &s.vol_index_exchange, &s.vol_index_symbol, &from, &s.to).await?
    };
    eprintln!("loaded {} bars, {} funding rows, {} vol-index rows", bars.len(), funding.len(), index.len());
    Ok((bars, funding, index))
}
