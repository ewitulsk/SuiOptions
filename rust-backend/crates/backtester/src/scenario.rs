//! Scenario TOML: everything a run needs, so a run is reproducible from
//! the file plus the lake snapshot it names.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Scenario {
    pub name: String,
    /// Underlying label for reports (e.g. "SUI").
    pub asset: String,
    /// Gold bars source for the spot path: `exchange` + `symbol` partition
    /// names (e.g. binance / SUI-USDT). The perp mark is this same path
    /// (`proxy_venue=true`) until Bluefin history exists.
    pub spot_exchange: String,
    pub spot_symbol: String,
    /// Silver funding_rates source (exchange / symbol partitions).
    pub funding_exchange: String,
    pub funding_symbol: String,
    /// Inclusive UTC dates.
    pub from: String,
    pub to: String,
    /// Starting NAV, settlement units (USD).
    pub nav0: f64,
    /// Staking / carry yield of the underlying (BAW dividend rate).
    pub carry_yield: f64,
    pub oracle: OracleModel,
    pub estimator: EstimatorConfig,
    pub bid: BidConfig,
    pub limits: LimitsConfig,
    pub flow: FlowConfig,
    pub hedge: HedgeConfig,
    pub exercise: ExerciseConfig,
    pub fees: ProtocolFees,
    pub hurdle: HurdleConfig,
}

impl Default for Scenario {
    fn default() -> Self {
        Self {
            name: "unnamed".into(),
            asset: "SUI".into(),
            spot_exchange: "binance".into(),
            spot_symbol: "SUI-USDT".into(),
            funding_exchange: "binance".into(),
            funding_symbol: "SUI-USDT-PERP".into(),
            from: "2025-08-01".into(),
            to: "2026-07-31".into(),
            nav0: 1_000_000.0,
            carry_yield: 0.0,
            oracle: OracleModel::default(),
            estimator: EstimatorConfig::default(),
            bid: BidConfig::default(),
            limits: LimitsConfig::default(),
            flow: FlowConfig::default(),
            hedge: HedgeConfig::default(),
            exercise: ExerciseConfig::default(),
            fees: ProtocolFees::default(),
            hurdle: HurdleConfig::default(),
        }
    }
}

/// The oracle proxy (doc 09 §3 / doc 08 §6.1): lake mids degraded through
/// an explicit model. Never a provider's history.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct OracleModel {
    /// Cadence at which the strategy receives a fresh decision price.
    pub update_ms: i64,
    /// Publish-to-actionable latency added to every observation.
    pub latency_ms: i64,
    /// Confidence half-width, bps of price (informational; the bid does
    /// not widen on it in v0).
    pub conf_bps: f64,
    /// Observations older than this are stale: no quotes, no hedge trades.
    pub max_age_ms: i64,
}

impl Default for OracleModel {
    fn default() -> Self {
        Self { update_ms: 60_000, latency_ms: 2_000, conf_bps: 5.0, max_age_ms: 180_000 }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct EstimatorConfig {
    /// `windows` = the live desk's two-window blend (pricing::surface).
    pub kind: String,
    /// Realized-vol sampling interval, seconds. Doc 07 §4: SUI ≥ 900.
    pub sample_interval_s: i64,
    pub short_window_hours: f64,
    pub long_window_hours: f64,
    pub short_window_weight: f64,
    pub long_window_weight: f64,
    /// Doc 07 §4 / doc 09 §2.3: the live blend lifts the mean to
    /// `max_lean × max(window)`; 0 disables the lift.
    pub max_lean: f64,
    pub risk_premium: f64,
    pub skew: f64,
    pub convexity: f64,
    pub term_short_boost: f64,
    pub term_decay_years: f64,
    pub floor_vol: f64,
    pub cap_vol: f64,
    /// Fallback while the windows are cold.
    pub fallback_vol: f64,
}

impl Default for EstimatorConfig {
    fn default() -> Self {
        Self {
            kind: "windows".into(),
            sample_interval_s: 900,
            short_window_hours: 24.0,
            long_window_hours: 168.0,
            short_window_weight: 1.0,
            long_window_weight: 1.0,
            max_lean: 0.8,
            risk_premium: 0.05,
            skew: 0.0,
            convexity: 0.0,
            term_short_boost: 0.0,
            term_decay_years: 0.05,
            floor_vol: 0.10,
            cap_vol: 4.0,
            fallback_vol: 0.80,
        }
    }
}

/// The V1 bid (pricing::desk) parameters.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct BidConfig {
    pub base_spread_volpts: f64,
    pub size_penalty_volpts_per_pct_nav: f64,
    pub size_penalty_quadratic_from_pct: f64,
    pub inventory_penalty_max_volpts: f64,
    pub inventory_penalty_start_util: f64,
    pub max_single_fill_pct_nav: f64,
    pub funding_income_credit: f64,
    /// Expected holding period used for the hedge-cost horizon, years.
    pub expected_holding_years: f64,
}

impl Default for BidConfig {
    fn default() -> Self {
        Self {
            base_spread_volpts: 0.05,
            size_penalty_volpts_per_pct_nav: 0.01,
            size_penalty_quadratic_from_pct: 3.0,
            inventory_penalty_max_volpts: 0.10,
            inventory_penalty_start_util: 0.6,
            max_single_fill_pct_nav: 100.0,
            funding_income_credit: 0.0,
            expected_holding_years: 21.0 / 365.0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct LimitsConfig {
    pub premium_budget_hard: f64,
    pub call_premium_max: f64,
    pub put_premium_max: f64,
    pub per_expiry_max: f64,
    pub vega_cap_nav_per_volpt: f64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            premium_budget_hard: 0.30,
            call_premium_max: 0.20,
            put_premium_max: 0.20,
            per_expiry_max: 0.10,
            vega_cap_nav_per_volpt: 0.005,
        }
    }
}

/// Constant-flow injector (doc 08 §8 capacity-mode subset).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct FlowConfig {
    /// `per_turn`: at each turn start buy `notional_nav_multiple × NAV`
    /// of spot notional (doc 07's M = 3.0 framing); `daily`: buy
    /// `notional_per_day` of spot notional every day at `hour_utc`.
    pub mode: String,
    pub notional_nav_multiple: f64,
    pub notional_per_day: f64,
    pub hour_utc: u32,
    /// Fraction of notional in calls; the rest are puts.
    pub call_share: f64,
    /// Tenor in days; the expiry is the listed board entry closest to it.
    pub tenor_days: f64,
    /// Strike moneyness in standard deviations (0 = ATM), quantised to
    /// the live lattice.
    pub moneyness_z: f64,
    /// Live board parameters (api-service /buckets defaults).
    pub tick_pct: f64,
    pub z_width: f64,
    /// Bucket every fill onto the epoch-aligned weekly/month-end board
    /// (true) or use the exact tenor (false, doc 07 style).
    pub use_expiry_board: bool,
}

impl Default for FlowConfig {
    fn default() -> Self {
        Self {
            mode: "per_turn".into(),
            notional_nav_multiple: 3.0,
            notional_per_day: 100_000.0,
            hour_utc: 0,
            call_share: 1.0,
            tenor_days: 30.0,
            moneyness_z: 0.0,
            tick_pct: 0.025,
            z_width: 2.0,
            use_expiry_board: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct HedgeConfig {
    pub band_pct_nav: f64,
    pub band_wide_pct_nav: f64,
    pub funding_widen_threshold: f64,
    /// Taker fill: spot × (1 ± slippage_bps).
    pub slippage_bps: f64,
    pub taker_fee_bps: f64,
    /// Flat per-fill fee, settlement units (Bluefin: 0.03).
    pub fixed_fee_per_fill: f64,
    /// Financing on parked initial margin (bid-side term).
    pub margin_financing_rate_annual: f64,
    pub initial_margin_fraction: f64,
    /// Expected rebalance fills per year per unit of initial notional,
    /// for the bid's expected-cost term only (the engine trades the real
    /// path).
    pub rebalance_turnover_per_year: f64,
}

impl Default for HedgeConfig {
    fn default() -> Self {
        Self {
            band_pct_nav: 15.0,
            band_wide_pct_nav: 25.0,
            funding_widen_threshold: -0.25,
            slippage_bps: 3.5,
            taker_fee_bps: 3.5,
            fixed_fee_per_fill: 0.03,
            margin_financing_rate_annual: 0.0,
            initial_margin_fraction: 0.10,
            rebalance_turnover_per_year: 0.0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ExerciseConfig {
    /// Spot execution slippage on the exercise leg, bps.
    pub spot_slippage_bps: f64,
    pub spot_fee_bps: f64,
    /// Gas per exercise PTB, settlement units.
    pub gas_per_exercise: f64,
    /// Gas per hedge rebalance (0 on Bluefin: off-chain sequencer).
    pub gas_per_rebalance: f64,
}

impl Default for ExerciseConfig {
    fn default() -> Self {
        Self { spot_slippage_bps: 5.0, spot_fee_bps: 2.5, gas_per_exercise: 0.05, gas_per_rebalance: 0.0 }
    }
}

/// Doc 09 G7: the protocol premium fee is a writer-side wedge (the desk
/// pays gross, the writer receives net — it sets the DISPLAYED APY), the
/// vault fees split the desk's gross return between depositors and the
/// curator/protocol.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ProtocolFees {
    /// `ProtocolConfig.fee_bps` skimmed from gross premium on every write.
    pub protocol_premium_fee_bps: f64,
    /// Curator performance fee on profit (trading-vault `curator_fee_bps`).
    pub curator_fee_bps: f64,
    /// Protocol share of the curator fee (registry `protocol_fee_bps`).
    pub vault_protocol_fee_bps: f64,
}

impl Default for ProtocolFees {
    fn default() -> Self {
        Self { protocol_premium_fee_bps: 0.0, curator_fee_bps: 2_000.0, vault_protocol_fee_bps: 1_000.0 }
    }
}

/// Doc 08 §0.4, restated depositor-net (doc 09 G7).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct HurdleConfig {
    pub min_annual_return: f64,
    pub settlement_cash_yield: f64,
    pub cash_yield_spread: f64,
    pub max_drawdown: f64,
}

impl Default for HurdleConfig {
    fn default() -> Self {
        Self { min_annual_return: 0.12, settlement_cash_yield: 0.04, cash_yield_spread: 0.08, max_drawdown: 0.15 }
    }
}

impl HurdleConfig {
    /// `max(12%, settlement cash yield + 8%)`.
    pub fn required_return(&self) -> f64 {
        self.min_annual_return.max(self.settlement_cash_yield + self.cash_yield_spread)
    }
}

impl Scenario {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Scenario> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let s: Scenario = toml::from_str(&text)?;
        anyhow::ensure!(s.nav0 > 0.0, "nav0 must be positive");
        anyhow::ensure!((0.0..=1.0).contains(&s.flow.call_share), "call_share in [0,1]");
        anyhow::ensure!(s.estimator.sample_interval_s >= 60, "sample_interval_s ≥ 60");
        Ok(s)
    }
}
