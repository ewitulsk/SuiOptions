//! Outputs: `summary.json` (labels, params, metrics, determinism hash),
//! `settled.csv` (per-option study rows), `nav.csv` (daily path).

use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::engine::RunOutput;
use crate::scenario::Scenario;

#[derive(Clone, Debug, Serialize)]
pub struct Labels {
    pub proxy_oracle: bool,
    pub proxy_venue: bool,
    pub taker_only: bool,
    pub no_resale: bool,
    pub constant_flow: bool,
    pub exercise: &'static str,
    pub estimator: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Summary {
    pub scenario: String,
    pub asset: String,
    pub labels: Labels,
    pub from: String,
    pub to: String,
    pub coverage: f64,
    pub stale_fraction: f64,
    pub turns: u64,
    pub fills: u64,
    pub declines_capacity: u64,
    pub declines_stale: u64,
    pub declines_priced_zero: u64,
    pub nav0: f64,
    pub nav_end: f64,
    pub desk_gross_return: f64,
    pub desk_gross_return_annualized: f64,
    /// After the curator performance fee on profit and the protocol's
    /// share of it (doc 09 G7): what a depositor keeps.
    pub depositor_net_return: f64,
    pub depositor_net_return_annualized: f64,
    pub required_return: f64,
    pub hurdle_pass: bool,
    pub max_drawdown: f64,
    pub drawdown_pass: bool,
    pub spot_return: f64,
    pub premium_paid: f64,
    pub option_payoff: f64,
    pub hedge_realized: f64,
    pub funding_paid: f64,
    pub hedge_fees: f64,
    pub hedge_slippage: f64,
    pub exercise_costs: f64,
    pub gas: f64,
    pub hedge_fills: u64,
    /// Σ|hedge notional| / NAV0, per 30 days of run (doc 07 §5's "turnover").
    pub hedge_turnover_nav_per_30d: f64,
    pub exercise_turnover_notional: f64,
    pub settled_count: usize,
    pub mean_sigma_paid: f64,
    pub mean_sigma_realized: f64,
    /// Mean (σ_realized − σ_paid): positive = the desk bought vol cheap.
    pub mean_vol_bias: f64,
    pub vol_pnl_proxy_total: f64,
    pub option_leg_pnl_total: f64,
    pub protocol_fee_wedge_total: f64,
    pub determinism_hash: String,
}

pub fn summarize(s: &Scenario, out: &RunOutput) -> Summary {
    let l = &out.ledger.lines;
    let days = out.minutes_total as f64 / 1440.0;
    let years = days / 365.0;
    let gross = out.nav_end / s.nav0 - 1.0;
    let profit = (out.nav_end - s.nav0).max(0.0);
    let curator_fee = profit * s.fees.curator_fee_bps / 10_000.0;
    let net_nav = out.nav_end - curator_fee;
    let net = net_nav / s.nav0 - 1.0;
    let ann = |r: f64| if years > 0.0 { (1.0 + r).powf(1.0 / years) - 1.0 } else { 0.0 };
    let n = out.settled.len().max(1) as f64;
    let mean_paid = out.settled.iter().map(|o| o.sigma_paid).sum::<f64>() / n;
    let mean_real = out.settled.iter().map(|o| o.sigma_realized).sum::<f64>() / n;
    let required = s.hurdle.required_return();
    let mut summary = Summary {
        scenario: s.name.clone(),
        asset: s.asset.clone(),
        labels: Labels {
            proxy_oracle: true, proxy_venue: true, taker_only: true, no_resale: true, constant_flow: true,
            exercise: "at_expiry",
            estimator: if s.estimator.kind == "har" { format!("har(q_bid={})", s.estimator.q_bid) } else { s.estimator.kind.clone() },
        },
        from: s.from.clone(),
        to: s.to.clone(),
        coverage: out.minutes_with_bar as f64 / out.minutes_total.max(1) as f64,
        stale_fraction: out.minutes_stale as f64 / out.minutes_total.max(1) as f64,
        turns: out.turns,
        fills: l.fills,
        declines_capacity: l.declines_capacity,
        declines_stale: l.declines_stale,
        declines_priced_zero: l.declines_priced_zero,
        nav0: s.nav0,
        nav_end: out.nav_end,
        desk_gross_return: gross,
        desk_gross_return_annualized: ann(gross),
        depositor_net_return: net,
        depositor_net_return_annualized: ann(net),
        required_return: required,
        hurdle_pass: ann(net) >= required,
        max_drawdown: out.max_drawdown,
        drawdown_pass: out.max_drawdown <= s.hurdle.max_drawdown,
        spot_return: out.spot_end / out.spot_start - 1.0,
        premium_paid: l.premium_paid,
        option_payoff: l.option_payoff,
        hedge_realized: l.hedge_realized,
        funding_paid: l.funding_paid,
        hedge_fees: l.hedge_fees,
        hedge_slippage: l.hedge_slippage,
        exercise_costs: l.exercise_costs,
        gas: l.gas,
        hedge_fills: l.hedge_fills,
        hedge_turnover_nav_per_30d: if days > 0.0 { l.hedge_turnover_notional / s.nav0 / (days / 30.0) } else { 0.0 },
        exercise_turnover_notional: l.exercise_turnover_notional,
        settled_count: out.settled.len(),
        mean_sigma_paid: mean_paid,
        mean_sigma_realized: mean_real,
        mean_vol_bias: mean_real - mean_paid,
        vol_pnl_proxy_total: out.settled.iter().map(|o| o.vol_pnl_proxy).sum(),
        option_leg_pnl_total: out.settled.iter().map(|o| o.option_leg_pnl).sum(),
        protocol_fee_wedge_total: l.premium_paid * s.fees.protocol_premium_fee_bps / 10_000.0,
        determinism_hash: String::new(),
    };
    let bytes = serde_json::to_vec(&summary).expect("summary serializes");
    let settled = serde_json::to_vec(&out.settled).expect("settled serializes");
    summary.determinism_hash = format!("{:016x}", crate::fnv1a(&[bytes, settled].concat()));
    summary
}

pub fn write_all(dir: &Path, s: &Scenario, out: &RunOutput, summary: &Summary) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("summary.json"), serde_json::to_string_pretty(summary)?)?;
    std::fs::write(dir.join("scenario.toml"), toml::to_string_pretty(s)?)?;
    let mut csv = String::from("id,is_put,strike,opened_ms,expiry_ms,qty,spot_open,spot_close,premium_paid,payoff,sigma_paid,sigma_surface,sigma_realized,vol_pnl_proxy,option_leg_pnl\n");
    for o in &out.settled {
        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            o.id, o.is_put, o.strike, o.opened_ms, o.expiry_ms, o.qty, o.spot_open, o.spot_close, o.premium_paid, o.payoff,
            o.sigma_paid, o.sigma_surface, o.sigma_realized, o.vol_pnl_proxy, o.option_leg_pnl
        ));
    }
    std::fs::write(dir.join("settled.csv"), csv)?;
    let mut nav = String::from("ts_ms,spot,nav,cash,option_marks,perp_position,net_delta_units,premium_deployed_pct,sigma_surface,stale\n");
    for p in &out.nav_path {
        nav.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{}\n",
            p.ts_ms, p.spot, p.nav, p.cash, p.option_marks, p.perp_position, p.net_delta_units, p.premium_deployed_pct,
            p.sigma_surface.map(|v| v.to_string()).unwrap_or_default(), p.stale
        ));
    }
    std::fs::write(dir.join("nav.csv"), nav)?;
    Ok(())
}
