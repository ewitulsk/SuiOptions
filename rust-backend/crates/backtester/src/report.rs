//! Outputs: `summary.json` (labels, params, metrics, determinism hash),
//! `settled.csv` (per-option study rows), `nav.csv` (daily path).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::engine::{RunOutput, VenueLabels};
use crate::exercise::ExerciseStats;
use crate::gaps::{GapSpan, InvalidatedSpan};
use crate::latency::LatencyConfig;
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
    /// PR N: `constant` | `generated_market` | `generated_capacity`.
    pub flow_source: &'static str,
    /// `instant` | `hazard_ttl`.
    pub acceptance: &'static str,
    /// `no_resale` | `resale=upside_scenario`.
    pub resale: &'static str,
    /// Every arrival/acceptance parameter is a stated prior (doc 08 §3.1).
    pub flow_provenance: &'static str,
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
    /// PR N: the six volumes, quote funnel, capacity peaks and gates.
    pub stats: crate::stats::RunStats,
    /// Doc 08 §7.2: the queue/fill assumption (`taker_only` until PR L).
    pub execution_assumption: String,
    /// Doc 08 §6.3: every stage's distribution and whether it is assumed.
    pub latency_assumptions: LatencyConfig,
    /// Doc 08 §6.4: `invalidate` or `bound`.
    pub gap_policy: String,
    pub required_feeds: Vec<String>,
    pub coverage_by_feed: BTreeMap<String, f64>,
    pub gaps: Vec<GapSpan>,
    pub invalidated_spans: Vec<InvalidatedSpan>,
    pub source_rows: Vec<(String, u64)>,
    pub timer_counts: BTreeMap<String, u64>,
    pub hedge_rejects: u64,
    pub pending_outcomes: u64,
    /// Doc 08 §7.2/§7.3 (PR L): venue lifecycle and margin lines.
    pub venue_labels: VenueLabels,
    pub maker_fees: f64,
    pub taker_fills: u64,
    pub passive_fills: u64,
    pub partial_fills: u64,
    pub cancels: u64,
    pub liquidations: u64,
    pub liquidation_loss: f64,
    pub min_margin_ratio: Option<f64>,
    pub closest_margin_headroom: Option<f64>,
    pub margin_topups: u64,
    pub topup_total: f64,
    pub topup_rejects: u64,
    pub topup_declines: u64,
    pub hedge_declines_margin: u64,
    pub first_liquidation_ms: Option<i64>,
    /// Doc 08 §7.5/§7.6 (PR M): paths taken, rejects, PTB failures,
    /// unexercised expiries and the non-atomic hedge-close delay.
    pub exercise_stats: ExerciseStats,
    pub hedge_close_delay_ms_mean: Option<f64>,
    /// `max(min_profit_usd, min_profit_bps × payout, mult × route uncertainty)`.
    pub exercise_min_profit_rule: String,
    /// Flash/pool capacity is a configured assumption (doc 08 §4.6).
    pub flash_capacity_assumed: bool,
    /// The call cash path sells the received underlying on the route
    /// (the live path leaves it in the vault).
    pub call_cash_path_sells_spot: bool,
    /// Event-ordering fingerprint (doc 08 §6.2).
    pub trace_hash: String,
    /// Doc 08 §9.1 (PR O): attribution lines, the idle-cost-adjusted
    /// return, capital efficiency, and the run-death / margin labels.
    pub attribution: crate::attribution::AttrLines,
    pub net_return_after_idle_cost_annualized: f64,
    /// Depositor-net profit per accepted Earn notional (capacity runs).
    pub net_profit_per_accepted_notional: Option<f64>,
    /// Depositor-net profit / peak capital deployed (marks + reservations + margin).
    pub return_on_peak_capital: Option<f64>,
    pub bankrupt_ms: Option<i64>,
    /// `isolated(bluefin_rules)` | `none(doc07_reproduction)`.
    pub margin_model: &'static str,
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
            proxy_oracle: true, proxy_venue: true, taker_only: out.execution_assumption == "taker_only", no_resale: !s.resale.enabled, constant_flow: out.flow_source == "constant",
            exercise: "american_sweep",
            estimator: if s.estimator.kind == "har" { format!("har(q_bid={})", s.estimator.q_bid) } else { s.estimator.kind.clone() },
            flow_source: out.flow_source,
            acceptance: out.acceptance,
            resale: if s.resale.enabled { "resale=upside_scenario" } else { "no_resale" },
            flow_provenance: crate::flow_gen::PRIOR_LABEL,
        },
        from: s.from.clone(),
        to: s.to.clone(),
        coverage: out.minutes_with_bar as f64 / out.minutes_total.max(1) as f64,
        stale_fraction: out.minutes_stale as f64 / out.minutes_total.max(1) as f64,
        turns: out.turns,
        fills: l.fills,
        declines_capacity: out.counters.declines_capacity,
        declines_stale: out.counters.declines_stale,
        declines_priced_zero: out.counters.declines_priced_zero,
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
        stats: out.stats.clone(),
        execution_assumption: out.execution_assumption.clone(),
        latency_assumptions: out.latency.clone(),
        gap_policy: out.coverage.policy.clone(),
        required_feeds: out.coverage.required_feeds.clone(),
        coverage_by_feed: out.coverage.feeds.iter().map(|(k, v)| (k.clone(), v.fraction)).collect(),
        gaps: out.coverage.gaps.clone(),
        invalidated_spans: out.coverage.invalidated_spans.clone(),
        source_rows: out.source_rows.clone(),
        timer_counts: out.timer_counts.clone(),
        hedge_rejects: l.hedge_rejects,
        pending_outcomes: out.pending_outcomes,
        venue_labels: out.venue_labels.clone(),
        maker_fees: l.maker_fees,
        taker_fills: l.taker_fills,
        passive_fills: l.passive_fills,
        partial_fills: l.partial_fills,
        cancels: l.cancels,
        liquidations: l.liquidations,
        liquidation_loss: l.liquidation_loss,
        min_margin_ratio: out.min_margin_ratio,
        closest_margin_headroom: out.closest_margin_headroom,
        margin_topups: l.margin_topups,
        topup_total: l.topup_total,
        topup_rejects: l.topup_rejects,
        topup_declines: out.counters.topup_declines,
        hedge_declines_margin: out.counters.hedge_declines_margin,
        first_liquidation_ms: out.first_liquidation_ms,
        exercise_stats: out.exercise.clone(),
        hedge_close_delay_ms_mean: out.exercise.hedge_close_delay_ms_mean(),
        exercise_min_profit_rule: format!("max(USD {}, {} bps x payout, {} x {} bps route uncertainty)", s.exercise.min_profit_usd, s.exercise.min_profit_bps, s.exercise.route_uncertainty_mult, s.exercise.route_uncertainty_bps),
        flash_capacity_assumed: true,
        call_cash_path_sells_spot: true,
        trace_hash: out.trace_hash.clone(),
        attribution: out.attribution,
        net_return_after_idle_cost_annualized: ann((net_nav - out.attribution.idle_cash_cost) / s.nav0 - 1.0),
        net_profit_per_accepted_notional: if out.stats.volumes.accepted_earn_notional > 0.0 { Some((net_nav - s.nav0) / out.stats.volumes.accepted_earn_notional) } else { None },
        return_on_peak_capital: if out.stats.peak_capital_deployed > 0.0 { Some((net_nav - s.nav0) / out.stats.peak_capital_deployed) } else { None },
        bankrupt_ms: out.bankrupt_ms,
        margin_model: if out.margin_model_enabled { "isolated(bluefin_rules)" } else { "none(doc07_reproduction)" },
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
    if let Some(a) = crate::attribution::report(s, out) {
        std::fs::write(dir.join("attribution.json"), serde_json::to_string_pretty(&a)?)?;
    }
    Ok(())
}
