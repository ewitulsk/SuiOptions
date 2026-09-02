//! The §5 exit ladder POLICY (pure): per held option, hold / exercise
//! with cash / flash-exercise / run the put waterfall. Execution (the
//! curator-session and wallet PTBs, the exits task) is
//! `services/mm-bot`'s `desk::exits`.
//!
//!   1. **Hold** — the default; gamma scalping monetizes. Resale is the
//!      listings engine's standing exchange asks, never decided here.
//!   2. **Exercise** when optimal — `forgone_carry > remaining_time_value
//!      × carry_mult` or near-expiry ITM. Wallet cash first, else the
//!      DeepBook flash-loan PTB.
//!
//! Puts (SO-443) go through the three-path atomic waterfall in [`put`],
//! gated on the §0.4 minimum profit and laddered inside expiry.

use std::collections::HashMap;

use serde::Deserialize;

use crate::model::MarketModel;

pub mod put;

/// `[desk.exits]` knobs. Defaults per 00-plan §5.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ExitsConfig {
    pub enabled: bool,
    pub tick_secs: u64,
    /// Step 0: net written positions against same-bucket VaultMm coin
    /// custody (`close_offset_*`) before any exercise.
    pub offset_close_enabled: bool,
    /// Exercise when `forgone_carry > remaining_time_value × this`.
    /// 00-plan: 1.1.
    pub carry_mult: f64,
    /// Force-exercise ITM holdings inside this many hours to expiry.
    pub near_expiry_hours: f64,
    /// Flash-exercise ladder chunk, underlying units per tx.
    pub max_slice: u64,
    /// Underlying-symbol → UNDERLYING/SETTLEMENT spot pool id (the
    /// flash-loan + sale venue). Missing entry ⇒ no flash path for that
    /// underlying (cash exercise only).
    pub spot_pools: HashMap<String, String>,
    pub gas_budget: u64,
    /// Put exercise policy (SO-443).
    pub put: put::PutExerciseConfig,
}

impl Default for ExitsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tick_secs: 300,
            offset_close_enabled: true,
            carry_mult: 1.1,
            near_expiry_hours: 24.0,
            max_slice: 1_000_000_000,
            spot_pools: HashMap::new(),
            gas_budget: 200_000_000,
            put: put::PutExerciseConfig::default(),
        }
    }
}

/// What the ladder decided for one holding this tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitAction {
    Hold,
    /// Exercise funded from wallet settlement cash.
    ExerciseCash,
    /// Exercise via the DeepBook flash-loan PTB.
    FlashExercise,
    /// Put: run the [`put`] waterfall (route chosen per slice).
    ExercisePut,
}

/// Pure ladder decision for one held option. (Resale is not decided
/// here: the listings engine keeps a standing ask resting on the
/// exchange instead.)
#[allow(clippy::too_many_arguments)]
pub fn decide_exit(
    cfg: &ExitsConfig,
    model: &MarketModel,
    is_put: bool,
    spot: f64,
    strike: f64,
    expiry_ms: u64,
    wallet_cash: u64,
    strike_cost: u64,
    now_ms: u64,
) -> ExitAction {
    let t = (expiry_ms.saturating_sub(now_ms)) as f64 / 1000.0 / 86_400.0 / 365.0;
    let (sigma, _) = model.sigma(spot, strike, t);
    if is_put {
        if !cfg.put.enabled {
            return ExitAction::Hold;
        }
        let hours = (expiry_ms.saturating_sub(now_ms)) as f64 / 3_600_000.0;
        let (carry, tv) = put::put_carry_and_time_value(model, spot, strike, t);
        return if put::put_exercise_wanted(cfg, spot, strike, hours, carry, tv) {
            ExitAction::ExercisePut
        } else {
            ExitAction::Hold
        };
    }
    // Exercise when optimal: forgone carry beats remaining time value
    // with margin, or near-expiry ITM.
    let itm = spot > strike;
    let near_expiry = (expiry_ms.saturating_sub(now_ms)) as f64 / 3_600_000.0
        <= cfg.near_expiry_hours;
    let carry = model.forgone_carry(spot, strike, t, sigma);
    let tv = model.remaining_time_value_call(spot, strike, t, sigma);
    let exercise = itm && (near_expiry || carry > tv * cfg.carry_mult);
    if exercise {
        return if wallet_cash >= strike_cost {
            ExitAction::ExerciseCash
        } else {
            ExitAction::FlashExercise
        };
    }
    // Default: hold and scalp (resting exchange asks handle resale).
    ExitAction::Hold
}

/// `ceil`-free mirror of the bucket's `apply_strike` (round-half-up).
pub fn strike_cost(amount: u64, strike: u128, strike_scale: u8) -> u64 {
    let divisor = 10u128.pow(strike_scale as u32);
    let numerator = amount as u128 * strike;
    u64::try_from((numerator + divisor / 2) / divisor).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn strike_cost_matches_apply_strike_rounding() {
        // Round-half-up mirror of bucket::apply_strike.
        assert_eq!(strike_cost(10, 100, 0), 1_000);
        assert_eq!(strike_cost(1, 15, 1), 2); // 1.5 rounds up
        assert_eq!(strike_cost(1, 14, 1), 1); // 1.4 rounds down
        assert_eq!(strike_cost(7, 100_000_000, 6), 700);
    }
}
