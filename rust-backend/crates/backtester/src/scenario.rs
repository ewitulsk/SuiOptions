//! Scenario TOML: everything a run needs, so a run is reproducible from
//! the file plus the lake snapshot it names.

use serde::{Deserialize, Serialize};

use crate::gaps::GapConfig;
use crate::latency::LatencyConfig;
use crate::margin::MarginConfig;

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
    /// Optional silver vol_index source (e.g. deribit / BTC-DVOL) for the
    /// `vol_index` estimator kind — the "true IV" ceiling of the doc 07
    /// §3 ablation. Empty = none.
    pub vol_index_exchange: String,
    pub vol_index_symbol: String,
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
    /// Flow/acceptance seed (doc 08 §8.7): same seed ⇒ identical flow.
    pub seed: u64,
    /// Minutes between full book revaluations (marks, net delta, APY
    /// menu). 1 = every minute (v0); the solver runs coarser.
    pub revalue_interval_min: i64,
    pub flow_gen: FlowGenConfig,
    pub acceptance: AcceptanceConfig,
    pub resale: ResaleConfig,
    pub venue: VenueConfig,
    /// Per-stage latency distributions (doc 08 §6.3).
    pub latency: LatencyConfig,
    /// Required feeds and the gap policy (doc 08 §6.4).
    pub gaps: GapConfig,
    /// Bluefin isolated-margin rules (doc 08 §7.3).
    pub margin: MarginConfig,
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
            vol_index_exchange: String::new(),
            vol_index_symbol: String::new(),
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
            seed: 1,
            revalue_interval_min: 1,
            flow_gen: FlowGenConfig::default(),
            acceptance: AcceptanceConfig::default(),
            resale: ResaleConfig::default(),
            venue: VenueConfig::default(),
            latency: LatencyConfig::default(),
            gaps: GapConfig::default(),
            margin: MarginConfig::default(),
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
    /// `windows` = the live desk's two-window blend (pricing::surface);
    /// `vol_index` = the scenario's vol index (percent, e.g. DVOL) as the
    /// base ATM sigma, same risk premium / shape / clamp path;
    /// `har` = the G5 forecaster (`vol-forecast`) at `q_bid`, no max-lean.
    pub kind: String,
    /// `har`: bid quantile of the forecast distribution (doc 09 §2.2 #5).
    pub q_bid: f64,
    /// `har`: refit cadence, seconds.
    pub refit_secs: i64,
    /// `har`: calibration window, days (history the fit sees).
    pub calibration_days: u32,
    /// `har`: derive smile convexity from the forecast's kurtosis.
    pub convexity_from_kurtosis: bool,
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
            q_bid: 0.35,
            refit_secs: 86_400,
            calibration_days: 365,
            convexity_from_kurtosis: false,
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
    /// `constant` (this injector) or `generated` (`[flow_gen]`, PR N).
    pub source: String,
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
            source: "constant".into(),
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

/// Per-type arrival and size priors (doc 08 §8.2–§8.3). STATED PRIORS:
/// no RFQ data calibrates them (doc 08 §3.1, 2026-09-01).
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct TypePriors {
    /// Arrivals per day at reference conditions.
    pub base_rate_per_day: f64,
    /// Sensitivity to the trailing log return (+ = more after run-ups).
    pub return_coef: f64,
    /// Sensitivity to ln(σ / σ one window ago).
    pub vol_spike_coef: f64,
    /// Sensitivity to ln(σ / reference_vol).
    pub vol_level_coef: f64,
    /// Arrival elasticity to displayed writer-net APY: (APY/ref)^ε.
    pub apy_elasticity: f64,
    pub apy_ref: f64,
    /// The collateral's alternative yield (staking for call collateral,
    /// settlement lending for put collateral) and the arrival multiplier
    /// applied when the displayed APY is below it.
    pub alt_yield: f64,
    pub alt_yield_penalty: f64,
    /// Lognormal size in spot notional (settlement units).
    pub size_median: f64,
    pub size_log_sd: f64,
    /// Requested moneyness in standard deviations (calls OTM > 0, puts
    /// OTM < 0), quantised to the live lattice.
    pub moneyness_mean_z: f64,
    pub moneyness_sd_z: f64,
}

impl TypePriors {
    pub fn call_default() -> Self {
        Self {
            base_rate_per_day: 20.0,
            return_coef: 4.0,
            vol_spike_coef: 0.0,
            vol_level_coef: 0.3,
            apy_elasticity: 1.2,
            apy_ref: 1.0,
            alt_yield: 0.03,
            alt_yield_penalty: 0.4,
            size_median: 2_000.0,
            size_log_sd: 1.2,
            moneyness_mean_z: 0.5,
            moneyness_sd_z: 0.4,
        }
    }

    pub fn put_default() -> Self {
        Self {
            base_rate_per_day: 12.0,
            return_coef: -4.0,
            vol_spike_coef: 1.5,
            vol_level_coef: 0.5,
            apy_elasticity: 0.8,
            apy_ref: 0.8,
            alt_yield: 0.05,
            alt_yield_penalty: 0.4,
            size_median: 3_000.0,
            size_log_sd: 1.0,
            moneyness_mean_z: -0.5,
            moneyness_sd_z: 0.4,
        }
    }
}

impl Default for TypePriors {
    fn default() -> Self {
        Self::call_default()
    }
}

/// The Earn flow generator (`flow.source = "generated"`, doc 08 §8).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct FlowGenConfig {
    /// Always "prior" — every parameter below is a stated hypothesis.
    pub provenance: String,
    /// `market`: elastic Poisson arrivals against the strategy's bid;
    /// `capacity`: `rfqs_per_day` writers whose sizes are rescaled so the
    /// day's offered notional equals `target_notional_per_day`.
    pub mode: String,
    pub target_notional_per_day: f64,
    pub call_share: f64,
    pub rfqs_per_day: u32,
    pub max_rfqs_per_minute: u32,
    pub call: TypePriors,
    pub put: TypePriors,
    /// Window for the trailing return and vol-spike features, hours.
    pub trailing_window_hours: f64,
    pub reference_vol: f64,
    /// Time-of-day multiplier 1 + A·cos(2π(h − peak)/24).
    pub tod_amplitude: f64,
    pub tod_peak_hour: f64,
    /// Arrival boost in the window after a board expiry (writers roll).
    pub calendar_boost: f64,
    pub calendar_window_hours: f64,
    /// Expiry menu: the live board (doc 09 G13) or `tenor_menu_days`.
    pub use_expiry_board: bool,
    pub tenor_menu_days: Vec<f64>,
    /// Geometric mass on the nearest listed expiry (1 = all nearest).
    pub expiry_concentration: f64,
    pub min_tenor_days: f64,
    /// Probability a writer joins the last bucket of its type
    /// (synchronized bucket concentration).
    pub herd_prob: f64,
    /// Protocol / plausible-collateral size bounds, spot notional.
    pub min_notional: f64,
    pub max_notional: f64,
    /// The indicative menu entry whose bid sets the displayed APY.
    pub apy_reference_tenor_days: f64,
    pub apy_reference_notional: f64,
}

impl Default for FlowGenConfig {
    fn default() -> Self {
        Self {
            provenance: "prior".into(),
            mode: "market".into(),
            target_notional_per_day: 100_000.0,
            call_share: 0.5,
            rfqs_per_day: 24,
            max_rfqs_per_minute: 50,
            call: TypePriors::call_default(),
            put: TypePriors::put_default(),
            trailing_window_hours: 24.0,
            reference_vol: 0.8,
            tod_amplitude: 0.3,
            tod_peak_hour: 15.0,
            calendar_boost: 0.5,
            calendar_window_hours: 24.0,
            use_expiry_board: true,
            tenor_menu_days: vec![7.0, 14.0, 30.0],
            expiry_concentration: 0.5,
            min_tenor_days: 1.0,
            herd_prob: 0.25,
            min_notional: 100.0,
            max_notional: 250_000.0,
            apy_reference_tenor_days: 14.0,
            apy_reference_notional: 5_000.0,
        }
    }
}

/// Per-type acceptance priors (doc 08 §8.4).
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct TypeAcceptance {
    /// P(accept over a full TTL) at `apy_ref`, no value drift.
    pub accept_prob_at_ref: f64,
    pub apy_ref: f64,
    /// Hazard elasticity to displayed APY: wider bids reduce acceptance.
    pub apy_elasticity: f64,
}

impl Default for TypeAcceptance {
    fn default() -> Self {
        Self { accept_prob_at_ref: 0.6, apy_ref: 1.0, apy_elasticity: 1.5 }
    }
}

/// Quote lifecycle and acceptance hazard (doc 08 §7.1, §8.4). Priors.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AcceptanceConfig {
    pub provenance: String,
    /// `instant` (fill on arrival: v0 parity, capacity mode) or `hazard`.
    pub mode: String,
    pub ttl_ms: i64,
    pub response_latency_ms: i64,
    pub inclusion_latency_ms: i64,
    pub revert_prob: f64,
    pub call: TypeAcceptance,
    pub put: TypeAcceptance,
    /// Selection into stale quotes: exp(β · (fair_at_quote − fair_now)/fair_at_quote).
    pub stale_edge_coef: f64,
    /// exp(β · ln(notional / size_ref)).
    pub size_coef: f64,
    pub size_ref_notional: f64,
    /// exp(β · |z|).
    pub moneyness_coef: f64,
    /// Hazard shape over the TTL: 0 = flat, > 0 = front-loaded.
    pub front_load: f64,
}

impl Default for AcceptanceConfig {
    fn default() -> Self {
        Self {
            provenance: "prior".into(),
            mode: "instant".into(),
            ttl_ms: 90_000,
            response_latency_ms: 1_500,
            inclusion_latency_ms: 3_000,
            revert_prob: 0.02,
            call: TypeAcceptance { accept_prob_at_ref: 0.6, apy_ref: 1.0, apy_elasticity: 1.5 },
            put: TypeAcceptance { accept_prob_at_ref: 0.55, apy_ref: 0.8, apy_elasticity: 1.2 },
            stale_edge_coef: 8.0,
            size_coef: -0.15,
            size_ref_notional: 5_000.0,
            moneyness_coef: 0.0,
            front_load: 1.0,
        }
    }
}

/// Resale (doc 08 §8.5): off by default (hold/exercise). When enabled
/// the run is labeled `resale=upside_scenario` and carries these
/// assumptions.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ResaleConfig {
    pub enabled: bool,
    /// Resale demand as a hazard per position per day, by type.
    pub call_demand_per_day: f64,
    pub put_demand_per_day: f64,
    pub fill_prob: f64,
    /// Sale price = mark × (1 − discount).
    pub price_discount: f64,
    pub latency_ms: i64,
    pub min_holding_days: f64,
}

impl Default for ResaleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            call_demand_per_day: 0.10,
            put_demand_per_day: 0.05,
            fill_prob: 0.5,
            price_discount: 0.05,
            latency_ms: 60_000,
            min_holding_days: 2.0,
        }
    }
}

/// Venue / flash / external-budget assumptions the solver gates on. All
/// labeled `assumed` until measured (doc 09: flash capacity is an
/// assumption until a pool-balance poller exists; PR M).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct VenueConfig {
    /// Max absolute perp notional the venue absorbs (0 = unlimited).
    pub max_hedge_notional: f64,
    /// Max spot notional one exercise can flash/route (0 = unlimited).
    pub flash_max_notional_per_exercise: f64,
    pub router_capacity_notional: f64,
    /// Governance budget fractions from the live vault (doc 08 §8.6).
    pub external_budget_fraction: f64,
    pub external_daily_release_fraction: f64,
    pub maintenance_margin_fraction: f64,
    // ── execution lifecycle (doc 08 §7.2, PR L) ─────────────────────
    /// `taker_only | optimistic | central | conservative`.
    pub execution_assumption: String,
    /// Maker fee, bps (Bluefin SUI-PERP: 1 bp).
    pub maker_fee_bps: f64,
    /// Base step size (SUI-PERP: 1 SUI); sizes round toward zero.
    pub contract_size: f64,
    /// Minimum order (SUI-PERP: 1 SUI); smaller orders are rejected.
    pub min_order_units: f64,
    /// Bluefin market take protection: taker fills never cross the
    /// oracle by more than this, bps (SUI-PERP: 2%).
    pub take_protection_bps: f64,
    /// Taker depth per bar, units (0 = unlimited): larger orders fill
    /// across bars (partial fills, consumed depth).
    pub max_taker_units_per_bar: f64,
    /// Persistent own-order impact: bps added per $1m of taker notional,
    /// decaying with `impact_half_life_ms` (config until L2 exists).
    pub impact_bps_per_million: f64,
    pub impact_half_life_ms: i64,
    /// Passive: fraction of the bar volume eligible at the limit that a
    /// resting order can take (`central`/`conservative`).
    pub passive_participation: f64,
    /// Passive: queue ahead of the order, units, scaled by the assumption
    /// (`optimistic` 0×, `central` 0.5×, `conservative` 1×).
    pub queue_depth_units: f64,
    /// Passive `conservative`: the bar must trade THROUGH the limit by
    /// this many bps before anything fills.
    pub through_bps: f64,
    /// Cancel a working order after this long (mm-bot `order_timeout_secs`).
    pub order_timeout_secs: i64,
    /// After a timeout, re-submit the remainder as a taker.
    pub passive_timeout_to_taker: bool,
    /// Mark basis over spot, step series `(from_ms, bps)` (doc 08 §7.4).
    pub basis: Vec<BasisPoint>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
pub struct BasisPoint {
    pub from_ms: i64,
    pub bps: f64,
}

impl VenueConfig {
    pub fn is_passive(&self) -> bool {
        self.execution_assumption != "taker_only"
    }

    /// Queue-ahead multiple of `queue_depth_units` for the assumption.
    pub fn queue_ahead_mult(&self) -> f64 {
        match self.execution_assumption.as_str() {
            "optimistic" => 0.0,
            "central" => 0.5,
            _ => 1.0,
        }
    }

    pub fn basis_bps_at(&self, ts_ms: i64) -> f64 {
        self.basis.iter().rfind(|b| b.from_ms <= ts_ms).map(|b| b.bps).unwrap_or(0.0)
    }
}

impl Default for VenueConfig {
    fn default() -> Self {
        Self {
            max_hedge_notional: 0.0,
            flash_max_notional_per_exercise: 0.0,
            router_capacity_notional: 0.0,
            external_budget_fraction: 0.20,
            external_daily_release_fraction: 0.10,
            maintenance_margin_fraction: 0.05,
            execution_assumption: "taker_only".into(),
            maker_fee_bps: 1.0,
            contract_size: 1.0,
            min_order_units: 1.0,
            take_protection_bps: 200.0,
            max_taker_units_per_bar: 0.0,
            impact_bps_per_million: 0.0,
            impact_half_life_ms: 300_000,
            passive_participation: 0.1,
            queue_depth_units: 0.0,
            through_bps: 1.0,
            order_timeout_secs: 60,
            passive_timeout_to_taker: true,
            basis: Vec::new(),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every checked-in scenario parses with the current schema.
    #[test]
    fn scenario_files_load() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios");
        let mut n = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "toml") {
                let s = Scenario::load(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                assert!(!s.name.is_empty());
                n += 1;
            }
        }
        assert!(n >= 4, "{n} scenarios");
    }
}
