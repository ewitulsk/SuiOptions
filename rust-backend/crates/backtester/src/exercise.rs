//! Exercise execution model (doc 08 §7.5/§7.6, PR M): the live exit
//! policies from `services/mm-bot/src/desk/exits.rs` and `exits/put.rs`
//! (SO-443), run on the exits cadence against a modeled Sui route.
//!
//! The pure policy functions here are MIRRORS of the live ones (they are
//! not importable: `mm-bot` links the Sui SDK) and are held to parity by
//! `fixtures/put_route_goldens.json`, which both crates assert against:
//!
//! - `strike_payout`, `min_profit`, `route_uncertainty`, `plan_slice`,
//!   `ladder`, `put_exercise_wanted` ← `exits/put.rs`
//! - `lot_round_up`, `quote_needed_for_base`, `PutPath`, `PoolLiquidity`
//!   ← `sui_tx::tx::put_exercise`
//! - the call branch of `decide_exit` (ITM and near-expiry, or forgone
//!   carry beats the CRR time value × `carry_mult`; wallet cash first,
//!   else the DeepBook flash-loan PTB) ← `exits.rs`
//!
//! Everything integer-valued runs in the live raw units (9-decimal
//! underlying, 6-decimal settlement, strike at `strike_scale` 6) so the
//! rounding is the chain's. The route is a linear-depth ask/bid ladder
//! around spot with configured pool balances (flash capacity is an
//! ASSUMPTION until a pool-balance poller exists, doc 08 §4.6). PTB
//! failure is a seeded hazard; a failed PTB changes no balance.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::latency::SplitMix64;

pub const UNDERLYING_DECIMALS: u32 = 9;
pub const SETTLEMENT_DECIMALS: u8 = 6;
pub const STRIKE_SCALE: u8 = 6;
/// DeepBook price float scaling (`sui_tx::tx::put_exercise::FLOAT_SCALING`).
pub const FLOAT_SCALING: u128 = 1_000_000_000;

/// `[exercise]` in the scenario (the v0 `spot_*`/`gas_*` knobs stay).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ExerciseConfig {
    /// v0: spot execution slippage/fee on an expiry settlement — now the
    /// bid/ask half-spread of the route ladder's first level, bps.
    pub spot_slippage_bps: f64,
    pub spot_fee_bps: f64,
    /// Gas per exercise PTB, settlement units.
    pub gas_per_exercise: f64,
    /// Gas per hedge rebalance (0 on Bluefin: off-chain sequencer).
    pub gas_per_rebalance: f64,
    /// Exits cadence: every position is checked once per
    /// `check_interval_secs` (daily), and every `sweep_interval_secs`
    /// inside `near_expiry_hours` (the redundant keeper sweep).
    pub check_interval_secs: i64,
    pub sweep_interval_secs: i64,
    pub near_expiry_hours: f64,
    /// Exercise when `forgone_carry > remaining_time_value × this`.
    pub carry_mult: f64,
    /// Minimum-profit rule terms (doc 08 §0.4): `max($, bps × payout,
    /// mult × route uncertainty)`; applied to every put route and to the
    /// call flash route (the live call cash path has no threshold: it is
    /// exercised whenever the ladder says so and never at a loss here).
    pub min_profit_usd: f64,
    pub min_profit_bps: f64,
    pub route_uncertainty_bps: f64,
    pub route_uncertainty_mult: f64,
    /// Taker fee bound on the route, bps (whitelisted pools charge none).
    pub swap_fee_bps: f64,
    /// Flash-loan fee bound, bps (DeepBook v3: none).
    pub flash_fee_bps: f64,
    /// Ladder chunk, underlying units per tx; per-slice inclusion
    /// allowance and the no-start margin before expiry.
    pub max_slice_units: f64,
    pub ladder_tx_secs: u64,
    pub expiry_margin_secs: u64,
    /// Route model: `route_levels` price levels of `route_level_bps`
    /// each, `route_depth_units_per_bps` units per bp on both sides.
    pub route_levels: u32,
    pub route_level_bps: f64,
    pub route_depth_units_per_bps: f64,
    /// Pool balances (flash capacity), underlying units / settlement.
    /// ASSUMED until measured (doc 08 §4.6).
    pub pool_base_balance_units: f64,
    pub pool_quote_balance: f64,
    pub lot_size_units: f64,
    pub min_size_units: f64,
    /// Underlying the vault holds free (the put vault-underlying route).
    pub vault_free_underlying_units: f64,
    /// Probability a submitted PTB fails (seeded hazard per slice).
    pub ptb_failure_prob: f64,
    /// Extra delay before the non-atomic hedge close is sent, on top of
    /// the detection and strategy latencies (modeled separately).
    pub hedge_close_delay_ms: i64,
}

impl Default for ExerciseConfig {
    fn default() -> Self {
        Self {
            spot_slippage_bps: 5.0,
            spot_fee_bps: 2.5,
            gas_per_exercise: 0.05,
            gas_per_rebalance: 0.0,
            check_interval_secs: 86_400,
            sweep_interval_secs: 300,
            near_expiry_hours: 24.0,
            carry_mult: 1.1,
            min_profit_usd: 10.0,
            min_profit_bps: 5.0,
            route_uncertainty_bps: 20.0,
            route_uncertainty_mult: 2.0,
            swap_fee_bps: 0.0,
            flash_fee_bps: 0.0,
            max_slice_units: 100_000.0,
            ladder_tx_secs: 30,
            expiry_margin_secs: 120,
            route_levels: 50,
            route_level_bps: 5.0,
            route_depth_units_per_bps: 2_000.0,
            pool_base_balance_units: 5_000_000.0,
            pool_quote_balance: 10_000_000.0,
            lot_size_units: 0.001,
            min_size_units: 0.01,
            vault_free_underlying_units: 0.0,
            ptb_failure_prob: 0.0,
            hedge_close_delay_ms: 0,
        }
    }
}

// ── mirrors of sui_tx::tx::put_exercise ────────────────────────────────

/// The three atomic put-exercise routes, in policy order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum PutPath {
    VaultUnderlying,
    BaseFlash,
    QuoteFlash,
}

impl PutPath {
    pub const ORDER: [PutPath; 3] = [PutPath::VaultUnderlying, PutPath::BaseFlash, PutPath::QuoteFlash];

    pub fn label(self) -> &'static str {
        match self {
            PutPath::VaultUnderlying => "vault_underlying",
            PutPath::BaseFlash => "base_flash",
            PutPath::QuoteFlash => "quote_flash",
        }
    }
}

/// What the spot pool can do for an exercise right now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PoolLiquidity {
    /// Base flash-loan capacity.
    pub base_balance: u64,
    /// Quote flash-loan capacity.
    pub quote_balance: u64,
    pub lot_size: u64,
    pub min_size: u64,
    /// Asks best-first: `(price_raw, base_quantity)`.
    pub asks: Vec<(u64, u64)>,
}

/// Round `amount` up to the pool's lot; `None` below `min_size`.
pub fn lot_round_up(amount: u64, lot_size: u64, min_size: u64) -> Option<u64> {
    let lot = lot_size.max(1);
    let rounded = amount.div_ceil(lot).checked_mul(lot)?;
    (rounded >= min_size).then_some(rounded)
}

/// Settlement needed to lift `base_needed` off the ask ladder (ceil per
/// level), `None` when the ladder is too shallow.
pub fn quote_needed_for_base(asks: &[(u64, u64)], base_needed: u64) -> Option<u64> {
    let mut remaining = base_needed;
    let mut cost: u128 = 0;
    for &(price_raw, qty) in asks {
        if remaining == 0 {
            break;
        }
        let take = remaining.min(qty);
        cost += (take as u128 * price_raw as u128).div_ceil(FLOAT_SCALING);
        remaining -= take;
    }
    (remaining == 0).then(|| u64::try_from(cost).unwrap_or(u64::MAX))
}

/// Settlement received for selling `base` into the bid ladder (floor per
/// level), `None` when the ladder is too shallow. The call-side twin of
/// `quote_needed_for_base`.
pub fn quote_received_for_base(bids: &[(u64, u64)], base: u64) -> Option<u64> {
    let mut remaining = base;
    let mut proceeds: u128 = 0;
    for &(price_raw, qty) in bids {
        if remaining == 0 {
            break;
        }
        let take = remaining.min(qty);
        proceeds += take as u128 * price_raw as u128 / FLOAT_SCALING;
        remaining -= take;
    }
    (remaining == 0).then(|| u64::try_from(proceeds).unwrap_or(u64::MAX))
}

// ── mirrors of exits/put.rs (pure policy) ──────────────────────────────

/// `put_bucket::exercise_payout` mirror: floor(amount × strike).
pub fn strike_payout(amount: u64, strike: u128, strike_scale: u8) -> u64 {
    let divisor = 10u128.pow(strike_scale as u32);
    u64::try_from(amount as u128 * strike / divisor).unwrap_or(u64::MAX)
}

/// `bucket::apply_strike` mirror (round-half-up): the call strike cost.
pub fn strike_cost(amount: u64, strike: u128, strike_scale: u8) -> u64 {
    let divisor = 10u128.pow(strike_scale as u32);
    let numerator = amount as u128 * strike;
    u64::try_from((numerator + divisor / 2) / divisor).unwrap_or(u64::MAX)
}

fn bps_of(amount: u64, bps: f64) -> u64 {
    (amount as f64 * bps / 10_000.0).ceil().max(0.0) as u64
}

fn usd_raw(usd: f64, settlement_decimals: u8) -> u64 {
    (usd * 10f64.powi(settlement_decimals as i32)).ceil().max(0.0) as u64
}

/// The subset of the config the pure policy reads (the live
/// `PutExerciseConfig` fields, same names).
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct PolicyConfig {
    pub min_profit_usd: f64,
    pub min_profit_bps: f64,
    pub route_uncertainty_bps: f64,
    pub route_uncertainty_mult: f64,
    pub swap_fee_bps: f64,
    pub flash_fee_bps: f64,
    pub gas_cost_usd: f64,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self { min_profit_usd: 10.0, min_profit_bps: 5.0, route_uncertainty_bps: 20.0, route_uncertainty_mult: 2.0, swap_fee_bps: 0.0, flash_fee_bps: 0.0, gas_cost_usd: 0.05 }
    }
}

impl From<&ExerciseConfig> for PolicyConfig {
    fn from(c: &ExerciseConfig) -> Self {
        Self {
            min_profit_usd: c.min_profit_usd,
            min_profit_bps: c.min_profit_bps,
            route_uncertainty_bps: c.route_uncertainty_bps,
            route_uncertainty_mult: c.route_uncertainty_mult,
            swap_fee_bps: c.swap_fee_bps,
            flash_fee_bps: c.flash_fee_bps,
            gas_cost_usd: c.gas_per_exercise,
        }
    }
}

/// `max($ term, bps × payout, mult × route uncertainty)` in settlement
/// raw units.
pub fn min_profit(cfg: &PolicyConfig, payout: u64, settlement_decimals: u8) -> u64 {
    usd_raw(cfg.min_profit_usd, settlement_decimals)
        .max(bps_of(payout, cfg.min_profit_bps))
        .max(bps_of(route_uncertainty(cfg, payout), cfg.route_uncertainty_mult * 10_000.0))
}

/// The conservative cash bound on route drift for one slice.
pub fn route_uncertainty(cfg: &PolicyConfig, payout: u64) -> u64 {
    bps_of(payout, cfg.route_uncertainty_bps)
}

/// What the desk can draw on for one slice.
#[derive(Clone, Debug, Default)]
pub struct PutLiquidity {
    pub own_underlying: u64,
    pub pool: PoolLiquidity,
}

/// One slice's chosen route and bounds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PutPlan {
    pub path: PutPath,
    pub amount: u64,
    pub payout: u64,
    pub max_quote_in: u64,
    pub min_profit: u64,
    pub expected_net: u64,
}

/// Why no route was taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanReject {
    NoRoute,
    Profit { net: i128, min_profit: u64 },
    Capacity,
}

impl PlanReject {
    pub fn label(&self) -> &'static str {
        match self {
            PlanReject::NoRoute => "no_route",
            PlanReject::Profit { .. } => "profit",
            PlanReject::Capacity => "capacity",
        }
    }
}

/// Choose the first profitable available route for `amount` put units.
pub fn plan_slice(cfg: &PolicyConfig, amount: u64, strike: u128, strike_scale: u8, settlement_decimals: u8, liq: &PutLiquidity) -> Result<PutPlan, PlanReject> {
    let payout = strike_payout(amount, strike, strike_scale);
    let base_needed = lot_round_up(amount, liq.pool.lot_size, liq.pool.min_size).ok_or(PlanReject::NoRoute)?;
    let acquisition = quote_needed_for_base(&liq.pool.asks, base_needed).ok_or(PlanReject::NoRoute)?;
    let swap_fee = bps_of(acquisition, cfg.swap_fee_bps);
    let uncertainty = route_uncertainty(cfg, payout);
    let max_quote_in = acquisition.saturating_add(swap_fee).saturating_add(uncertainty);
    let min_profit = min_profit(cfg, payout, settlement_decimals);
    let gas = usd_raw(cfg.gas_cost_usd, settlement_decimals);

    let mut best_reject = PlanReject::Capacity;
    for path in PutPath::ORDER {
        let flash_cost = match path {
            PutPath::VaultUnderlying => 0,
            PutPath::BaseFlash => bps_of(acquisition, cfg.flash_fee_bps),
            PutPath::QuoteFlash => bps_of(max_quote_in, cfg.flash_fee_bps),
        };
        let net = payout as i128 - acquisition as i128 - swap_fee as i128 - flash_cost as i128 - gas as i128;
        let onchain_ok = max_quote_in.saturating_add(min_profit) <= payout;
        if net < min_profit as i128 || !onchain_ok {
            best_reject = PlanReject::Profit { net, min_profit };
            continue;
        }
        let available = match path {
            PutPath::VaultUnderlying => liq.own_underlying >= amount,
            PutPath::BaseFlash => liq.pool.base_balance >= amount,
            PutPath::QuoteFlash => liq.pool.quote_balance >= max_quote_in,
        };
        if !available {
            continue;
        }
        return Ok(PutPlan { path, amount, payout, max_quote_in, min_profit, expected_net: (net - min_profit as i128).max(0) as u64 + min_profit });
    }
    Err(best_reject)
}

/// Slice `amount` into a ladder that cannot cross expiry.
pub fn ladder(amount: u64, max_slice: u64, remaining_ms: u64, tx_ms: u64, margin_ms: u64) -> Vec<u64> {
    if amount == 0 || remaining_ms <= margin_ms {
        return Vec::new();
    }
    let max_slice = max_slice.max(1);
    let allowed = ((remaining_ms - margin_ms) / tx_ms.max(1)).max(1);
    let wanted = amount.div_ceil(max_slice);
    let n = wanted.min(allowed);
    let slice = amount.div_ceil(n);
    let mut out = Vec::with_capacity(n as usize);
    let mut left = amount;
    while left > 0 {
        let s = left.min(slice);
        out.push(s);
        left -= s;
    }
    out
}

/// The exercise-timing rule for a held put (mirror): ITM, and either
/// inside the near-expiry sweep window or American-optimal.
pub fn put_exercise_wanted(near_expiry_hours: f64, carry_mult: f64, spot: f64, strike: f64, hours_to_expiry: f64, carry: f64, time_value: f64) -> bool {
    let itm = spot < strike;
    let near_expiry = hours_to_expiry <= near_expiry_hours;
    itm && (near_expiry || carry >= time_value * carry_mult)
}

/// The call branch of `decide_exit` (mirror): ITM and near expiry, or
/// ITM with forgone carry strictly above the time value × `carry_mult`.
pub fn call_exercise_wanted(near_expiry_hours: f64, carry_mult: f64, spot: f64, strike: f64, hours_to_expiry: f64, carry: f64, time_value: f64) -> bool {
    let itm = spot > strike;
    let near_expiry = hours_to_expiry <= near_expiry_hours;
    itm && (near_expiry || carry > time_value * carry_mult)
}

// ── the call routes ────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum CallPath {
    /// Vault settlement cash pays the strike (`exercise_cash` /
    /// `exercise_call_coin`); the underlying is sold on the route.
    Cash,
    /// DeepBook quote flash loan pays the strike, the underlying is sold,
    /// the loan repaid exactly, the residual kept (`flash_exercise_call`).
    QuoteFlash,
}

impl CallPath {
    pub fn label(self) -> &'static str {
        match self {
            CallPath::Cash => "call_cash",
            CallPath::QuoteFlash => "call_quote_flash",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallPlan {
    pub path: CallPath,
    pub amount: u64,
    pub cost: u64,
    pub proceeds: u64,
    /// Net settlement after cost, swap fee, flash cost and gas.
    pub net: i128,
    pub min_profit: u64,
}

/// Call route: cash first when the vault's free settlement covers the
/// strike cost (never at a loss), else the quote flash with the
/// minimum-profit rule and the pool's quote capacity.
#[allow(clippy::too_many_arguments)]
pub fn plan_call_slice(cfg: &PolicyConfig, amount: u64, strike: u128, strike_scale: u8, settlement_decimals: u8, free_cash: u64, pool: &PoolLiquidity, bids: &[(u64, u64)]) -> Result<CallPlan, PlanReject> {
    let cost = strike_cost(amount, strike, strike_scale);
    let base = lot_round_up(amount, pool.lot_size, pool.min_size).ok_or(PlanReject::NoRoute)?;
    let proceeds = quote_received_for_base(bids, base.min(amount).max(pool.min_size)).ok_or(PlanReject::NoRoute)?;
    let swap_fee = bps_of(proceeds, cfg.swap_fee_bps);
    let gas = usd_raw(cfg.gas_cost_usd, settlement_decimals);
    let min_profit = min_profit(cfg, cost, settlement_decimals);
    let net_cash = proceeds as i128 - cost as i128 - swap_fee as i128 - gas as i128;
    if free_cash >= cost {
        if net_cash < 0 {
            return Err(PlanReject::Profit { net: net_cash, min_profit: 0 });
        }
        return Ok(CallPlan { path: CallPath::Cash, amount, cost, proceeds, net: net_cash, min_profit: 0 });
    }
    let flash_cost = bps_of(cost, cfg.flash_fee_bps);
    let net = net_cash - flash_cost as i128;
    if net < min_profit as i128 {
        return Err(PlanReject::Profit { net, min_profit });
    }
    if pool.quote_balance < cost {
        return Err(PlanReject::Capacity);
    }
    Ok(CallPlan { path: CallPath::QuoteFlash, amount, cost, proceeds, net, min_profit })
}

// ── the modeled route ──────────────────────────────────────────────────

/// Underlying units → raw (9 decimals).
pub fn units_raw(units: f64) -> u64 {
    (units * 10f64.powi(UNDERLYING_DECIMALS as i32)).round().max(0.0) as u64
}

/// Settlement → raw (6 decimals).
pub fn settle_raw(usd: f64) -> u64 {
    (usd * 10f64.powi(SETTLEMENT_DECIMALS as i32)).round().max(0.0) as u64
}

pub fn raw_settle(raw: i128) -> f64 {
    raw as f64 / 10f64.powi(SETTLEMENT_DECIMALS as i32)
}

/// Strike per unit → the chain's scaled strike (settlement-raw per
/// underlying-raw × 10^scale).
pub fn strike_raw(strike: f64) -> u128 {
    (strike * 10f64.powi(SETTLEMENT_DECIMALS as i32 - UNDERLYING_DECIMALS as i32 + STRIKE_SCALE as i32)).round().max(0.0) as u128
}

/// Price per unit → DeepBook price_raw (quote-atomic per base-atomic ×
/// FLOAT_SCALING).
pub fn price_raw(px: f64) -> u64 {
    (px * 10f64.powi(SETTLEMENT_DECIMALS as i32 - UNDERLYING_DECIMALS as i32) * FLOAT_SCALING as f64).round().max(0.0) as u64
}

/// The route as the exercise sees it at `spot`: a linear-depth ladder on
/// each side (first level at the half-spread, then `route_level_bps`
/// steps), plus the pool balances.
pub struct Route {
    pub pool: PoolLiquidity,
    pub bids: Vec<(u64, u64)>,
}

pub fn route_at(cfg: &ExerciseConfig, spot: f64) -> Route {
    let qty = units_raw(cfg.route_depth_units_per_bps * cfg.route_level_bps.max(1e-9));
    let mut asks = Vec::with_capacity(cfg.route_levels as usize);
    let mut bids = Vec::with_capacity(cfg.route_levels as usize);
    for k in 0..cfg.route_levels.max(1) {
        let bps = cfg.spot_slippage_bps + k as f64 * cfg.route_level_bps;
        asks.push((price_raw(spot * (1.0 + bps / 10_000.0)), qty));
        bids.push((price_raw(spot * (1.0 - bps / 10_000.0)), qty));
    }
    Route {
        pool: PoolLiquidity {
            base_balance: units_raw(cfg.pool_base_balance_units),
            quote_balance: settle_raw(cfg.pool_quote_balance),
            lot_size: units_raw(cfg.lot_size_units).max(1),
            min_size: units_raw(cfg.min_size_units).max(1),
            asks,
        },
        bids,
    }
}

/// Seeded PTB failure hazard.
pub struct PtbHazard {
    rng: SplitMix64,
    prob: f64,
}

impl PtbHazard {
    pub fn new(seed: u64, prob: f64) -> Self {
        Self { rng: SplitMix64::new(seed ^ 0x5075_7442), prob: prob.clamp(0.0, 1.0) }
    }

    /// True when the PTB fails (draws only when the hazard is non-zero).
    pub fn fails(&mut self) -> bool {
        self.prob > 0.0 && self.rng.next_f64() < self.prob
    }
}

/// Per-path counters and the labels every summary carries.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ExerciseStats {
    /// Successful PTBs by path label.
    pub paths: BTreeMap<String, u64>,
    pub slices_submitted: u64,
    pub ptb_failed: u64,
    /// Slices not attempted, by reject label.
    pub rejects: BTreeMap<String, u64>,
    /// Options that reached expiry unexercised (worthless on chain) and
    /// the intrinsic value they carried at the decision price.
    pub expired_unexercised: u64,
    pub expired_unexercised_itm: u64,
    pub expired_itm_value: f64,
    /// Non-atomic hedge close: PTB detection → close command, ms.
    pub hedge_close_delay_ms_sum: i64,
    pub hedge_closes: u64,
}

impl ExerciseStats {
    pub fn path(&mut self, label: &str) {
        *self.paths.entry(label.to_string()).or_insert(0) += 1;
    }

    pub fn reject(&mut self, label: &str) {
        *self.rejects.entry(label.to_string()).or_insert(0) += 1;
    }

    pub fn hedge_close_delay_ms_mean(&self) -> Option<f64> {
        (self.hedge_closes > 0).then(|| self.hedge_close_delay_ms_sum as f64 / self.hedge_closes as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared put-route fixture: the live `exits/put.rs` asserts the
    /// same file (doc 08 P3 gate: every route decision matches).
    #[test]
    fn put_route_goldens_match_the_shared_fixture() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/put_route_goldens.json");
        let text = std::fs::read_to_string(&path).unwrap();
        let g: serde_json::Value = serde_json::from_str(&text).unwrap();
        for case in g["plan"].as_array().unwrap() {
            let cfg: PolicyConfig = serde_json::from_value(case["cfg"].clone()).unwrap();
            let liq = PutLiquidity {
                own_underlying: case["own_underlying"].as_u64().unwrap(),
                pool: PoolLiquidity {
                    base_balance: case["base_balance"].as_u64().unwrap(),
                    quote_balance: case["quote_balance"].as_u64().unwrap(),
                    lot_size: case["lot_size"].as_u64().unwrap(),
                    min_size: case["min_size"].as_u64().unwrap(),
                    asks: case["asks"].as_array().unwrap().iter().map(|a| (a[0].as_u64().unwrap(), a[1].as_u64().unwrap())).collect(),
                },
            };
            let got = plan_slice(&cfg, case["amount"].as_u64().unwrap(), case["strike"].as_u64().unwrap() as u128, case["strike_scale"].as_u64().unwrap() as u8, case["settlement_decimals"].as_u64().unwrap() as u8, &liq);
            let name = case["name"].as_str().unwrap();
            let exp = &case["expect"];
            match got {
                Ok(plan) => {
                    assert_eq!(plan.path.label(), exp["path"].as_str().unwrap(), "{name}");
                    assert_eq!(plan.payout, exp["payout"].as_u64().unwrap(), "{name}");
                    assert_eq!(plan.max_quote_in, exp["max_quote_in"].as_u64().unwrap(), "{name}");
                    assert_eq!(plan.min_profit, exp["min_profit"].as_u64().unwrap(), "{name}");
                }
                Err(r) => {
                    assert_eq!(r.label(), exp["reject"].as_str().unwrap(), "{name}: {r:?}");
                    if let PlanReject::Profit { net, min_profit } = r {
                        if let Some(n) = exp["net"].as_i64() {
                            assert_eq!(net, n as i128, "{name}");
                        }
                        if let Some(m) = exp["min_profit"].as_u64() {
                            assert_eq!(min_profit, m, "{name}");
                        }
                        if exp["net_negative"].as_bool() == Some(true) {
                            assert!(net < 0, "{name}");
                        }
                    }
                }
            }
        }
        for case in g["ladder"].as_array().unwrap() {
            let got = ladder(case["amount"].as_u64().unwrap(), case["max_slice"].as_u64().unwrap(), case["remaining_ms"].as_u64().unwrap(), case["tx_ms"].as_u64().unwrap(), case["margin_ms"].as_u64().unwrap());
            let exp: Vec<u64> = case["expect"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
            assert_eq!(got, exp, "{}", case["name"]);
        }
        for case in g["min_profit"].as_array().unwrap() {
            let cfg: PolicyConfig = serde_json::from_value(case["cfg"].clone()).unwrap();
            assert_eq!(min_profit(&cfg, case["payout"].as_u64().unwrap(), 6), case["expect"].as_u64().unwrap(), "{}", case["name"]);
        }
        for case in g["put_wanted"].as_array().unwrap() {
            let f = |k: &str| case[k].as_f64().unwrap();
            assert_eq!(put_exercise_wanted(24.0, 1.1, f("spot"), f("strike"), f("hours"), f("carry"), f("time_value")), case["expect"].as_bool().unwrap(), "{}", case["name"]);
        }
    }

    #[test]
    fn call_route_cash_first_then_flash_with_the_profit_rule() {
        let cfg = PolicyConfig::default();
        // 9-dec underlying, 6-dec settlement: strike $3.00 → 3_000 at scale 6.
        let strike = strike_raw(3.0);
        assert_eq!(strike, 3_000);
        let amount = units_raw(1_000.0);
        assert_eq!(strike_cost(amount, strike, STRIKE_SCALE), 3_000_000_000);
        let pool = PoolLiquidity { base_balance: 0, quote_balance: settle_raw(10_000.0), lot_size: 1_000, min_size: 10_000, asks: vec![] };
        // Bids at $3.50: proceeds $3500 for 1000 units.
        let bids = vec![(price_raw(3.5), units_raw(1e6))];
        assert_eq!(price_raw(3.5), 3_500_000);
        let cash = plan_call_slice(&cfg, amount, strike, STRIKE_SCALE, SETTLEMENT_DECIMALS, settle_raw(5_000.0), &pool, &bids).unwrap();
        assert_eq!(cash.path, CallPath::Cash);
        assert_eq!(cash.proceeds, 3_500_000_000);
        assert_eq!(cash.net, 3_500_000_000 - 3_000_000_000 - 50_000);
        // No free cash: the quote flash, needing pool quote ≥ cost and the
        // minimum profit (max($10, 5 bp × $3000 = $1.50, 2 × 20 bp = $12) = $12).
        let flash = plan_call_slice(&cfg, amount, strike, STRIKE_SCALE, SETTLEMENT_DECIMALS, 0, &pool, &bids).unwrap();
        assert_eq!(flash.path, CallPath::QuoteFlash);
        assert_eq!(flash.min_profit, 12_000_000);
        let dry = PoolLiquidity { quote_balance: 2_999_999_999, ..pool.clone() };
        assert_eq!(plan_call_slice(&cfg, amount, strike, STRIKE_SCALE, SETTLEMENT_DECIMALS, 0, &dry, &bids), Err(PlanReject::Capacity));
        // Bids barely above strike: below the minimum → profit reject on
        // the flash path; the cash path exercises as long as net ≥ 0.
        let thin = vec![(price_raw(3.005), units_raw(1e6))];
        assert!(matches!(plan_call_slice(&cfg, amount, strike, STRIKE_SCALE, SETTLEMENT_DECIMALS, 0, &pool, &thin), Err(PlanReject::Profit { .. })));
        assert_eq!(plan_call_slice(&cfg, amount, strike, STRIKE_SCALE, SETTLEMENT_DECIMALS, settle_raw(5_000.0), &pool, &thin).unwrap().path, CallPath::Cash);
        // Below strike: never, even with cash.
        let under = vec![(price_raw(2.9), units_raw(1e6))];
        assert!(matches!(plan_call_slice(&cfg, amount, strike, STRIKE_SCALE, SETTLEMENT_DECIMALS, settle_raw(5_000.0), &pool, &under), Err(PlanReject::Profit { .. })));
        // Shallow bids: no route.
        let shallow = vec![(price_raw(3.5), 10)];
        assert_eq!(plan_call_slice(&cfg, amount, strike, STRIKE_SCALE, SETTLEMENT_DECIMALS, 0, &pool, &shallow), Err(PlanReject::NoRoute));
    }

    #[test]
    fn route_ladder_prices_depth_and_the_timing_rules() {
        let cfg = ExerciseConfig { route_levels: 3, route_level_bps: 10.0, route_depth_units_per_bps: 100.0, spot_slippage_bps: 5.0, ..ExerciseConfig::default() };
        let r = route_at(&cfg, 3.0);
        assert_eq!(r.pool.asks.len(), 3);
        assert_eq!(r.pool.asks[0], (price_raw(3.0 * 1.0005), units_raw(1_000.0)));
        assert_eq!(r.pool.asks[1].0, price_raw(3.0 * 1.0015));
        assert_eq!(r.bids[0].0, price_raw(3.0 * 0.9995));
        // Buying 2500 units walks two and a half levels.
        let cost = quote_needed_for_base(&r.pool.asks, units_raw(2_500.0)).unwrap();
        let expect = 1_000.0 * 3.0 * 1.0005 + 1_000.0 * 3.0 * 1.0015 + 500.0 * 3.0 * 1.0025;
        assert!((raw_settle(cost as i128) - expect).abs() < 0.01, "{}", raw_settle(cost as i128));
        assert!(quote_needed_for_base(&r.pool.asks, units_raw(3_001.0)).is_none());
        assert!(call_exercise_wanted(24.0, 1.1, 3.1, 3.0, 1.0, 0.0, 0.5));
        assert!(!call_exercise_wanted(24.0, 1.1, 3.1, 3.0, 48.0, 0.0, 0.5));
        assert!(call_exercise_wanted(24.0, 1.1, 3.1, 3.0, 48.0, 0.6, 0.5));
        assert!(!call_exercise_wanted(24.0, 1.1, 2.9, 3.0, 1.0, 0.0, 0.0));
        let mut h = PtbHazard::new(1, 0.0);
        assert!(!h.fails());
        let mut h = PtbHazard::new(1, 1.0);
        assert!(h.fails());
    }
}
