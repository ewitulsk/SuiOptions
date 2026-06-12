//! Scenario TOML schema and cartesian sweep expansion (doc 06 §7). Every
//! field under `[strategy]`, `[premium]`, `[exercise]`, `[fees]`, `[swap]`
//! accepts a scalar or a list; the cross product of all lists is the sweep.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Scalar-or-list for sweepable fields.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T: Clone> OneOrMany<T> {
    fn values(&self) -> Vec<T> {
        match self {
            OneOrMany::One(v) => vec![v.clone()],
            OneOrMany::Many(vs) => vs.clone(),
        }
    }
}

fn default_one<T: Default>() -> OneOrMany<T> {
    OneOrMany::One(T::default())
}

#[derive(Clone, Debug, Deserialize)]
pub struct RawScenario {
    pub scenario: Meta,
    pub strategy: StrategySection,
    pub premium: PremiumSection,
    pub exercise: ExerciseSection,
    #[serde(default)]
    pub fees: FeesSection,
    #[serde(default)]
    pub swap: SwapSection,
}

/// Non-swept run-level settings.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Meta {
    /// SUI | BTC | ETH — picks decimals and the z-ladder.
    pub asset: String,
    /// replay | bootstrap | gbm_jump | garch
    pub paths: String,
    /// Candle CSV (see data.rs schema). History for replay; calibration +
    /// start price for synthetic generators.
    pub candles: String,
    /// Vol-index CSV for the `dvol` IV provider (replay only).
    #[serde(default)]
    pub dvol: Option<String>,
    /// Reference candles + vol index for `beta_scaled` (replay only).
    #[serde(default)]
    pub ref_candles: Option<String>,
    #[serde(default)]
    pub ref_dvol: Option<String>,
    /// Monte Carlo path count (ignored for replay).
    #[serde(default = "default_n_paths")]
    pub n_paths: usize,
    /// Horizon in rounds for synthetic paths.
    #[serde(default = "default_rounds")]
    pub rounds: usize,
    /// Bars per round (daily bars ⇒ 7 = weekly).
    #[serde(default = "default_round_bars")]
    pub round_bars: usize,
    /// Initial vault deposit, in whole tokens.
    #[serde(default = "default_initial_tokens")]
    pub initial_tokens: f64,
    /// Risk-free rate for pricing.
    #[serde(default)]
    pub r: f64,
    /// Master seed for synthetic paths and auction draws.
    #[serde(default = "default_seed")]
    pub seed: u64,
    /// Trailing realized-vol window, bars.
    #[serde(default = "default_rv_window")]
    pub rv_window: usize,
    /// LP flow schedule (doc 06 §6 step 2): steady deposits, a steady
    /// withdrawal trickle, and an optional one-round redemption stress.
    #[serde(default)]
    pub deposit_tokens_per_round: f64,
    #[serde(default)]
    pub withdraw_fraction_bps: u64,
    #[serde(default)]
    pub stress_round: Option<u64>,
    #[serde(default)]
    pub stress_fraction_bps: u64,
    /// Grid-vol clamp (scheduler's vol_floor / vol_ceiling).
    #[serde(default = "default_vol_floor")]
    pub vol_floor: f64,
    #[serde(default = "default_vol_ceiling")]
    pub vol_ceiling: f64,
}

fn default_n_paths() -> usize {
    1000
}
fn default_rounds() -> usize {
    156
}
fn default_round_bars() -> usize {
    7
}
fn default_initial_tokens() -> f64 {
    1000.0
}
fn default_seed() -> u64 {
    42
}
fn default_rv_window() -> usize {
    30
}
fn default_vol_floor() -> f64 {
    0.2
}
fn default_vol_ceiling() -> f64 {
    3.0
}

#[derive(Clone, Debug, Deserialize)]
pub struct StrategySection {
    pub delta_target: OneOrMany<f64>,
    pub deploy_fraction: OneOrMany<f64>,
    pub slices: OneOrMany<u32>,
    /// at_roll | at_expiry | hold_usdc
    pub premium_policy: OneOrMany<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PremiumSection {
    /// vrp_transfer | beta_scaled | realized_only | dvol
    pub iv_provider: OneOrMany<String>,
    /// IV/RV ratio for vrp_transfer.
    #[serde(default = "default_vrp_ratio")]
    pub vrp_ratio: OneOrMany<f64>,
    /// Applied to every provider's output: σ × (1 + skew_bps/10⁴).
    #[serde(default = "default_one")]
    pub skew_bps: OneOrMany<i64>,
    /// rfq_batch | onchain_auction | pessimist
    pub mechanism: OneOrMany<String>,
    /// RfqBatch haircut; also the auction's mean bid markdown and the
    /// pessimist's haircut.
    pub haircut_bps: OneOrMany<u64>,
    #[serde(default = "default_n_bidders")]
    pub n_bidders_mean: OneOrMany<f64>,
    /// Vault reserve floor, bps of spot notional (doc 03 §6).
    #[serde(default = "default_reserve_bps")]
    pub reserve_bps: OneOrMany<u64>,
    #[serde(default = "default_one")]
    pub no_show_prob: OneOrMany<f64>,
    /// Pessimist fill probability.
    #[serde(default = "default_fill_prob")]
    pub fill_prob: OneOrMany<f64>,
}

fn default_vrp_ratio() -> OneOrMany<f64> {
    OneOrMany::One(1.15)
}
fn default_n_bidders() -> OneOrMany<f64> {
    OneOrMany::One(3.0)
}
fn default_reserve_bps() -> OneOrMany<u64> {
    OneOrMany::One(10)
}
fn default_fill_prob() -> OneOrMany<f64> {
    OneOrMany::One(0.8)
}

#[derive(Clone, Debug, Deserialize)]
pub struct ExerciseSection {
    /// rational | early_<bps> | partial_<pct>
    pub policy: OneOrMany<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FeesSection {
    pub mgmt_bps_annual: OneOrMany<u64>,
    pub perf_bps: OneOrMany<u64>,
}

impl Default for FeesSection {
    fn default() -> Self {
        Self {
            mgmt_bps_annual: OneOrMany::One(200),
            perf_bps: OneOrMany::One(1000),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct SwapSection {
    pub slippage_bps: OneOrMany<u64>,
}

impl Default for SwapSection {
    fn default() -> Self {
        Self { slippage_bps: OneOrMany::One(50) }
    }
}

/// One point of the sweep — every parameter scalar.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cell {
    pub delta_target: f64,
    pub deploy_fraction: f64,
    pub slices: u32,
    pub premium_policy: String,
    pub iv_provider: String,
    pub vrp_ratio: f64,
    pub skew_bps: i64,
    pub mechanism: String,
    pub haircut_bps: u64,
    pub n_bidders_mean: f64,
    pub reserve_bps: u64,
    pub no_show_prob: f64,
    pub fill_prob: f64,
    pub exercise: String,
    pub mgmt_bps_annual: u64,
    pub perf_bps: u64,
    pub slippage_bps: u64,
}

#[derive(Clone, Debug)]
pub struct Scenario {
    pub meta: Meta,
    pub cells: Vec<Cell>,
}

pub fn parse(toml_str: &str) -> Result<Scenario> {
    let raw: RawScenario = toml::from_str(toml_str).context("parsing scenario TOML")?;
    validate_meta(&raw.scenario)?;

    // Cartesian product: fold each sweepable field over the cells built so
    // far.
    macro_rules! sweep {
        ($cells:ident, $vals:expr, $field:ident) => {
            let vals = $vals.values();
            $cells = $cells
                .into_iter()
                .flat_map(|cell| {
                    vals.iter().cloned().map(move |v| {
                        let mut cell = cell.clone();
                        cell.$field = v;
                        cell
                    })
                })
                .collect();
        };
    }

    let mut cells = vec![Cell::default()];
    sweep!(cells, raw.strategy.delta_target, delta_target);
    sweep!(cells, raw.strategy.deploy_fraction, deploy_fraction);
    sweep!(cells, raw.strategy.slices, slices);
    sweep!(cells, raw.strategy.premium_policy, premium_policy);
    sweep!(cells, raw.premium.iv_provider, iv_provider);
    sweep!(cells, raw.premium.vrp_ratio, vrp_ratio);
    sweep!(cells, raw.premium.skew_bps, skew_bps);
    sweep!(cells, raw.premium.mechanism, mechanism);
    sweep!(cells, raw.premium.haircut_bps, haircut_bps);
    sweep!(cells, raw.premium.n_bidders_mean, n_bidders_mean);
    sweep!(cells, raw.premium.reserve_bps, reserve_bps);
    sweep!(cells, raw.premium.no_show_prob, no_show_prob);
    sweep!(cells, raw.premium.fill_prob, fill_prob);
    sweep!(cells, raw.exercise.policy, exercise);
    sweep!(cells, raw.fees.mgmt_bps_annual, mgmt_bps_annual);
    sweep!(cells, raw.fees.perf_bps, perf_bps);
    sweep!(cells, raw.swap.slippage_bps, slippage_bps);

    for cell in &cells {
        validate_cell(cell)?;
    }
    Ok(Scenario { meta: raw.scenario, cells })
}

fn validate_meta(meta: &Meta) -> Result<()> {
    match meta.asset.as_str() {
        "SUI" | "BTC" | "ETH" => {}
        other => bail!("unknown asset {other}: expected SUI | BTC | ETH"),
    }
    match meta.paths.as_str() {
        "replay" | "bootstrap" | "gbm_jump" | "garch" => {}
        other => bail!("unknown path source {other}"),
    }
    Ok(())
}

fn validate_cell(cell: &Cell) -> Result<()> {
    match cell.premium_policy.as_str() {
        "at_roll" | "at_expiry" | "hold_usdc" => {}
        other => bail!("unknown premium_policy {other}"),
    }
    match cell.iv_provider.as_str() {
        "vrp_transfer" | "beta_scaled" | "realized_only" | "dvol" => {}
        other => bail!("unknown iv_provider {other}"),
    }
    match cell.mechanism.as_str() {
        "rfq_batch" | "onchain_auction" | "pessimist" => {}
        other => bail!("unknown mechanism {other}"),
    }
    parse_exercise(&cell.exercise)?;
    Ok(())
}

/// Parses the exercise policy spec: `rational`, `early_<bps>`,
/// `partial_<pct>`.
pub enum ExerciseSpec {
    Rational,
    Early { thresh_bps: u64 },
    Partial { frac: f64 },
}

pub fn parse_exercise(spec: &str) -> Result<ExerciseSpec> {
    if spec == "rational" {
        return Ok(ExerciseSpec::Rational);
    }
    if let Some(bps) = spec.strip_prefix("early_") {
        let bps = bps.strip_suffix("bps").unwrap_or(bps);
        return Ok(ExerciseSpec::Early {
            thresh_bps: bps.parse().with_context(|| format!("bad early spec {spec}"))?,
        });
    }
    if let Some(pct) = spec.strip_prefix("partial_") {
        let pct: f64 = pct.parse().with_context(|| format!("bad partial spec {spec}"))?;
        bail_unless(pct > 0.0 && pct <= 100.0, spec)?;
        return Ok(ExerciseSpec::Partial { frac: pct / 100.0 });
    }
    bail!("unknown exercise policy {spec}: expected rational | early_<bps> | partial_<pct>")
}

fn bail_unless(cond: bool, spec: &str) -> Result<()> {
    if !cond {
        bail!("partial_<pct> out of (0, 100]: {spec}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
[scenario]
asset = "SUI"
paths = "replay"
candles = "data/sui_usd_1d.csv"

[strategy]
delta_target = [0.05, 0.10]
deploy_fraction = 1.0
slices = [1, 4]
premium_policy = ["at_roll", "hold_usdc"]

[premium]
iv_provider = "vrp_transfer"
mechanism = ["rfq_batch", "onchain_auction"]
haircut_bps = [0, 500]

[exercise]
policy = ["rational", "early_500", "partial_60"]
"#;

    #[test]
    fn expands_the_cartesian_product() {
        let s = parse(MINIMAL).unwrap();
        // 2 × 1 × 2 × 2 × 1 × 2 × 2 × 3 = 96 cells.
        assert_eq!(s.cells.len(), 96);
        assert_eq!(s.meta.round_bars, 7); // default
        assert_eq!(s.meta.initial_tokens, 1000.0);
        // Defaults flow into every cell.
        assert!(s.cells.iter().all(|c| c.mgmt_bps_annual == 200 && c.perf_bps == 1000));
        assert!(s.cells.iter().all(|c| c.slippage_bps == 50));
    }

    #[test]
    fn rejects_unknown_values() {
        assert!(parse(&MINIMAL.replace("\"SUI\"", "\"DOGE\"")).is_err());
        assert!(parse(&MINIMAL.replace("rfq_batch", "dark_pool")).is_err());
        assert!(parse(&MINIMAL.replace("\"rational\"", "\"vibes\"")).is_err());
    }

    #[test]
    fn parses_exercise_specs() {
        assert!(matches!(parse_exercise("rational").unwrap(), ExerciseSpec::Rational));
        match parse_exercise("early_500").unwrap() {
            ExerciseSpec::Early { thresh_bps } => assert_eq!(thresh_bps, 500),
            _ => panic!(),
        }
        match parse_exercise("partial_60").unwrap() {
            ExerciseSpec::Partial { frac } => assert!((frac - 0.6).abs() < 1e-12),
            _ => panic!(),
        }
        assert!(parse_exercise("partial_0").is_err());
        assert!(parse_exercise("sometimes").is_err());
    }
}
