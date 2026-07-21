//! Quote decisions for the two WS-RFQ flows (00-plan V1 §1, V2 §2).
//!
//! WRITER-flow RFQs (retail sells, the desk buys): V1 bid = model fair at
//! a discounted vol via `pricing::desk::v1_bid` (through the model
//! adapter), limits enforced first — quote every RFQ, degrade with
//! size/inventory, decline only over hard caps.
//!
//! TRADER-flow RFQs (retail buys, the desk writes): declined while
//! `[desk.v2]` is disabled; else the V2 skewed sigmas + write-size scale,
//! fully-collateralized writes only, near-expiry throttle, and the
//! hard-capped naked-short budget.
//!
//! Everything here is pure — the desk runtime resolves spot/exposure and
//! signs; these functions only decide.

use super::limits::{self, BookExposure, HardDecline, LimitsConfig, ProposedFill};
use super::model::{BidContext, MarketModel, V2Params};

/// The bucket-resolved inputs for one RFQ (api-service is the source, as
/// before — never the wire broadcast).
#[derive(Clone, Copy, Debug)]
pub struct RfqInputs {
    pub write_amount: u64,
    pub is_put: bool,
    pub strike: u128,
    pub strike_scale: u8,
    pub expiry_ms: u64,
}

impl RfqInputs {
    pub fn strike_scaled(&self) -> f64 {
        self.strike as f64 / 10f64.powi(self.strike_scale as i32)
    }
    pub fn t_years(&self, now_ms: u64) -> f64 {
        self.expiry_ms.saturating_sub(now_ms) as f64 / 1000.0 / 86_400.0 / 365.0
    }
}

/// Outcome of pricing one RFQ.
#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    Quote {
        /// Premium in settlement smallest-units.
        premium: u64,
    },
    Decline {
        reason: String,
    },
}

/// Cross-flow context the desk runtime resolves before deciding.
#[derive(Clone, Debug)]
pub struct FlowContext {
    /// Spot in settlement-raw per underlying-raw.
    pub spot: f64,
    /// Cached book exposure (kill switch, budgets, net vega, …).
    pub exposure: BookExposure,
    /// Hedge venue funding, annualized (short receives when positive).
    pub funding_rate_annual: f64,
    /// Expected holding period for a bought option, years (config).
    pub expected_holding_years: f64,
    /// Hedge slippage estimate, bps of delta notional (config).
    pub slippage_bps: f64,
    /// Naked short units already written (V2 budget usage).
    pub naked_written_units: u64,
    /// Nightly stress gate: true blocks NEW short risk (V2 §7).
    pub stress_blocked: bool,
}

/// V1 writer flow: the desk buys retail's option.
pub fn price_writer_flow(
    model: &MarketModel,
    v1: &super::model::V1BidParams,
    limits_cfg: &LimitsConfig,
    ctx: &FlowContext,
    inputs: &RfqInputs,
    now_ms: u64,
) -> Decision {
    let t = inputs.t_years(now_ms);
    let strike = inputs.strike_scaled();
    let amount = inputs.write_amount as f64;
    let (sigma, _) = model.sigma(ctx.spot, strike, t);

    // The proposed fill's own risk at fair vol.
    let fair_pu = model.fair_per_unit(inputs.is_put, ctx.spot, strike, t, sigma);
    let greeks = model.greeks_per_unit(inputs.is_put, ctx.spot, strike, t, sigma);
    let fill = ProposedFill {
        premium: fair_pu * amount,
        vega_per_volpt: greeks.vega * amount / 100.0,
        // Long options decay: per-day theta < 0 ⇒ positive daily cost.
        theta_cost_per_day: (-greeks.theta * amount).max(0.0),
        expiry_ms: inputs.expiry_ms,
        strike_bucket: limits::strike_bucket(strike, ctx.spot),
    };
    let util = match limits::evaluate(limits_cfg, &ctx.exposure, &fill) {
        Ok(u) => u,
        Err(hard) => return decline_hard(hard),
    };

    let bid_ctx = BidContext {
        nav: ctx.exposure.nav,
        premium_notional: fair_pu * amount,
        vega_utilization: util.vega,
        funding_rate_annual: ctx.funding_rate_annual,
        expected_holding_years: ctx.expected_holding_years,
        slippage_cost: greeks.delta.abs() * ctx.spot * amount * ctx.slippage_bps / 10_000.0,
    };
    let Some((total_bid, _sigma)) =
        model.v1_bid_total(inputs.is_put, ctx.spot, strike, t, amount, &bid_ctx, v1)
    else {
        return Decision::Decline {
            reason: "over max single fill (or bid ≤ 0 after hedge costs)".into(),
        };
    };
    let premium = total_bid.floor();
    if !(premium >= 1.0) {
        return Decision::Decline {
            reason: "priced to zero".into(),
        };
    }
    Decision::Quote { premium: premium as u64 }
}

/// V2 trader flow: the desk writes to retail. `v2` is `None` while
/// `[desk.v2]` is disabled. `cover_available_units` is the held long
/// inventory in the SAME series available to cover this write.
pub fn price_trader_flow(
    model: &MarketModel,
    v2: Option<&V2Params>,
    naked_vega_cap_nav_per_volpt: f64,
    ctx: &FlowContext,
    inputs: &RfqInputs,
    cover_available_units: u64,
    now_ms: u64,
) -> Decision {
    let Some(v2) = v2 else {
        return Decision::Decline {
            reason: "trader flow disabled ([desk.v2] off)".into(),
        };
    };
    if ctx.exposure.kill_switch {
        return decline_hard(HardDecline::KillSwitch);
    }
    if ctx.stress_blocked {
        return Decision::Decline {
            reason: "stress gate: new short risk blocked".into(),
        };
    }
    let t = inputs.t_years(now_ms);
    let strike = inputs.strike_scaled();
    let amount = inputs.write_amount as f64;
    let nav = ctx.exposure.nav;
    if nav <= 0.0 {
        return Decision::Decline { reason: "no NAV".into() };
    }
    let (sigma_fair, _) = model.sigma(ctx.spot, strike, t);
    let net_vega_per_nav_volpt = ctx.exposure.net_vega_per_volpt / nav;

    // Skewed effective sigmas; None = outside the signed vega band.
    let Some((_bid_sigma, ask_sigma)) =
        super::model::v2_effective_sigmas(sigma_fair, net_vega_per_nav_volpt, t, v2)
    else {
        return Decision::Decline {
            reason: "outside signed vega band".into(),
        };
    };

    // Asymmetric size cap with the short-edge scale and the near-expiry
    // size throttle.
    let scale = super::model::v2_write_size_scale(net_vega_per_nav_volpt, v2);
    let near_expiry = t * 365.0 * 24.0 <= v2.near_expiry_hours;
    let size_mult = if near_expiry { v2.near_expiry_size_mult } else { 1.0 };
    let max_units = (v2.write_cap_pct_nav / 100.0) * nav / ctx.spot.max(f64::MIN_POSITIVE)
        * scale
        * size_mult;
    if amount > max_units {
        return Decision::Decline {
            reason: format!("write size over cap ({amount:.0} > {max_units:.0} units)"),
        };
    }

    // Naked-short budget: written-not-covered-by-held vega, hard-capped.
    let naked_add = inputs.write_amount.saturating_sub(cover_available_units);
    if naked_add > 0 {
        let greeks = model.greeks_per_unit(inputs.is_put, ctx.spot, strike, t, sigma_fair);
        // Existing naked vega approximated with the same per-unit vega —
        // conservative enough for the hard cap.
        let naked_vega_after =
            greeks.vega * (naked_add + ctx.naked_written_units) as f64 / 100.0;
        if naked_vega_after > naked_vega_cap_nav_per_volpt * nav {
            return Decision::Decline {
                reason: "naked short vega budget exhausted".into(),
            };
        }
    }

    let per_unit = model.fair_per_unit(inputs.is_put, ctx.spot, strike, t, ask_sigma);
    let premium = (per_unit * amount).ceil();
    if !(premium >= 1.0) {
        return Decision::Decline {
            reason: "priced to zero".into(),
        };
    }
    Decision::Quote { premium: premium as u64 }
}

fn decline_hard(hard: HardDecline) -> Decision {
    Decision::Decline {
        reason: format!("hard limit: {}", hard.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desk::model::{MarketModel, SurfaceConfig, V1BidParams, V2Params};
    use parking_lot::RwLock;
    use pyth_client::RollingVolBuffer;
    use std::sync::Arc;

    const DAY_MS: u64 = 86_400_000;

    /// Model on cold vol buffers: the surface quotes the 0.60 fallback
    /// (+0 risk premium so numbers are hand-checkable).
    fn model() -> MarketModel {
        MarketModel::new(
            "TSUI".into(),
            "0x1::tsui::TSUI".into(),
            Arc::new(RwLock::new(RollingVolBuffer::new(DAY_MS))),
            Arc::new(RwLock::new(RollingVolBuffer::new(7 * DAY_MS))),
            0.60,
            0.0,
            0.0,
            SurfaceConfig {
                risk_premium: 0.0,
                skew: 0.0,
                convexity: 0.0,
                term_short_boost: 0.0,
                term_decay_years: 0.25,
                anchor_ratio: None,
                floor_vol: 0.01,
                cap_vol: 5.0,
                short_window_weight: 1.0,
                long_window_weight: 1.0,
            },
        )
    }

    fn v1() -> V1BidParams {
        super::super::V1Config::default().into()
    }

    fn v2() -> V2Params {
        super::super::V2Config::default().into()
    }

    fn ctx() -> FlowContext {
        FlowContext {
            spot: 100.0,
            exposure: BookExposure { nav: 1e9, ..Default::default() },
            funding_rate_annual: 0.0,
            expected_holding_years: 21.0 / 365.0,
            slippage_bps: 0.0,
            naked_written_units: 0,
            stress_blocked: false,
        }
    }

    /// ATM call, 30 days out, 1M units.
    fn atm(write_amount: u64) -> RfqInputs {
        RfqInputs {
            write_amount,
            is_put: false,
            strike: 100,
            strike_scale: 0,
            expiry_ms: 30 * DAY_MS,
        }
    }

    fn premium_of(d: &Decision) -> u64 {
        match d {
            Decision::Quote { premium } => *premium,
            other => panic!("expected Quote, got {other:?}"),
        }
    }

    // ── V1 writer flow ─────────────────────────────────────────────────

    #[test]
    fn v1_happy_path_bids_below_fair() {
        let m = model();
        let d = price_writer_flow(&m, &v1(), &LimitsConfig::default(), &ctx(), &atm(1_000_000), 0);
        let premium = premium_of(&d);
        // Fair ATM 30d at σ=0.60 ≈ 6.8 per unit → ~6.8M total; the bid
        // sits below fair (5-volpt base discount) but well above zero.
        let fair = m.fair_per_unit(false, 100.0, 100.0, 30.0 / 365.0, 0.60) * 1e6;
        assert!((premium as f64) < fair, "premium {premium} !< fair {fair}");
        assert!((premium as f64) > fair * 0.5, "premium {premium} collapsed vs fair {fair}");
    }

    #[test]
    fn v1_degrades_with_inventory_and_size_never_refusing() {
        let m = model();
        let base = premium_of(&price_writer_flow(
            &m,
            &v1(),
            &LimitsConfig::default(),
            &ctx(),
            &atm(1_000_000),
            0,
        ));
        // High vega utilization (85% of cap): still quotes, but lower.
        let mut hot = ctx();
        hot.exposure.net_vega_per_volpt = 0.85 * 0.005 * 1e9;
        let degraded = premium_of(&price_writer_flow(
            &m,
            &v1(),
            &LimitsConfig::default(),
            &hot,
            &atm(1_000_000),
            0,
        ));
        assert!(degraded < base, "{degraded} !< {base}");
        // Twice the size pays a worse per-unit price.
        let double = premium_of(&price_writer_flow(
            &m,
            &v1(),
            &LimitsConfig::default(),
            &ctx(),
            &atm(2_000_000),
            0,
        ));
        assert!(
            (double as f64) < 2.0 * base as f64,
            "double-size clip must be worse per unit: {double} vs 2×{base}"
        );
    }

    #[test]
    fn v1_declines_only_over_hard_caps() {
        let m = model();
        // Premium budget hard cap: 34.9% deployed, fill would cross 35%.
        let mut full = ctx();
        full.exposure.premium_deployed = 0.349 * 1e9;
        let d = price_writer_flow(&m, &v1(), &LimitsConfig::default(), &full, &atm(10_000_000), 0);
        assert!(
            matches!(&d, Decision::Decline { reason } if reason.contains("hard limit")),
            "{d:?}"
        );
        // Kill switch.
        let mut killed = ctx();
        killed.exposure.kill_switch = true;
        let d = price_writer_flow(&m, &v1(), &LimitsConfig::default(), &killed, &atm(1_000_000), 0);
        assert!(
            matches!(&d, Decision::Decline { reason } if reason.contains("kill switch")),
            "{d:?}"
        );
        // Max single fill (5% NAV premium): a huge clip declines.
        let d = price_writer_flow(
            &m,
            &v1(),
            &LimitsConfig::default(),
            &ctx(),
            // ~50M units × ~6.8 ≈ 340M premium ≈ 34% NAV — way over 5%.
            &atm(50_000_000),
            0,
        );
        assert!(matches!(&d, Decision::Decline { .. }), "{d:?}");
    }

    // ── V2 trader flow ─────────────────────────────────────────────────

    #[test]
    fn v2_gate_declines_when_disabled_and_quotes_when_on() {
        let m = model();
        let d = price_trader_flow(&m, None, 0.001, &ctx(), &atm(200_000), 0, 0);
        assert!(
            matches!(&d, Decision::Decline { reason } if reason.contains("disabled")),
            "{d:?}"
        );
        // Enabled + fully covered (and under the 3%-NAV write cap of
        // 300k units): quotes above fair (ask side).
        let p = v2();
        let d = price_trader_flow(&m, Some(&p), 0.001, &ctx(), &atm(200_000), 200_000, 0);
        let premium = premium_of(&d);
        let fair = m.fair_per_unit(false, 100.0, 100.0, 30.0 / 365.0, 0.60) * 200_000.0;
        assert!((premium as f64) > fair, "ask {premium} !> fair {fair}");
    }

    #[test]
    fn v2_declines_past_short_band_stress_gate_and_naked_budget() {
        let m = model();
        let p = v2();
        // Net vega at the short edge: no writing capacity.
        let mut short = ctx();
        short.exposure.net_vega_per_volpt = -p.vega_band_short * 1e9;
        let d = price_trader_flow(&m, Some(&p), 0.001, &short, &atm(200_000), 200_000, 0);
        assert!(
            matches!(&d, Decision::Decline { reason } if reason.contains("vega band")),
            "{d:?}"
        );
        // Stress gate blocks new short risk.
        let mut stressed = ctx();
        stressed.stress_blocked = true;
        let d = price_trader_flow(&m, Some(&p), 0.001, &stressed, &atm(200_000), 200_000, 0);
        assert!(
            matches!(&d, Decision::Decline { reason } if reason.contains("stress gate")),
            "{d:?}"
        );
        // Naked budget: zero cover + zero cap declines any naked write.
        let d = price_trader_flow(&m, Some(&p), 0.0, &ctx(), &atm(200_000), 0, 0);
        assert!(
            matches!(&d, Decision::Decline { reason } if reason.contains("naked")),
            "{d:?}"
        );
    }

    #[test]
    fn v2_near_expiry_throttle_halves_size_cap() {
        let m = model();
        let p = v2();
        // Write cap: 3% NAV / spot = 300k units; near expiry halves it.
        // 24h to expiry (inside the 48h window), 200k units > 150k cap.
        let inputs = RfqInputs {
            write_amount: 200_000,
            is_put: false,
            strike: 100,
            strike_scale: 0,
            expiry_ms: DAY_MS,
        };
        let d = price_trader_flow(&m, Some(&p), 0.001, &ctx(), &inputs, 200_000, 0);
        assert!(
            matches!(&d, Decision::Decline { reason } if reason.contains("size over cap")),
            "{d:?}"
        );
        // The same clip far from expiry fits under the full 300k cap.
        let far = RfqInputs { expiry_ms: 30 * DAY_MS, ..inputs };
        let d = price_trader_flow(&m, Some(&p), 0.001, &ctx(), &far, 200_000, 0);
        assert!(matches!(&d, Decision::Quote { .. }), "{d:?}");
    }
}
