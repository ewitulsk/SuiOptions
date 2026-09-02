//! Quote decisions for WRITER-flow RFQs (retail sells, the desk buys):
//! V1 bid = model fair at a discounted vol via `pricing::desk::v1_bid`
//! (through the model adapter), limits enforced first — quote every
//! eligible RFQ, degrade with size/inventory, decline only over hard
//! caps.
//!
//! The desk NEVER writes options (SO-426, doc 08 §4.1): trader-flow
//! RFQs decline unconditionally in `Desk::price_ws_rfq`.
//!
//! Everything here is pure — the desk runtime resolves spot/exposure and
//! signs; these functions only decide.

use super::limits::{self, BookExposure, HardDecline, LimitsConfig, ProposedFill};
use super::model::{BidContext, MarketModel};

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
        /// Model fair TOTAL premium at the decision's surface vol —
        /// diagnostics for the RFQ funnel (SO-425), never the bid.
        model_fair: f64,
        /// Surface vol the decision priced at, annualized.
        surface_vol: f64,
        /// The fill's hedge notional and exercise demand (strike cash /
        /// underlying value) — what its reservation holds against the
        /// capital policy (SO-444).
        hedge_notional: f64,
        exercise_cash: f64,
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
    /// Current signed perp hedge position for this underlying, hedge
    /// units (long > 0). The bid prices the CHANGE a fill causes from
    /// here (doc 09 G2, SO-437).
    pub hedge_position_units: f64,
    /// Venue cost inputs (slippage, fees, turnover, margin financing).
    pub hedge_cost: pricing::desk::HedgeCostParams,
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
        is_put: inputs.is_put,
        vega_per_volpt: greeks.vega * amount / 100.0,
        // Long options decay: per-day theta < 0 ⇒ positive daily cost.
        theta_cost_per_day: (-greeks.theta * amount).max(0.0),
        expiry_ms: inputs.expiry_ms,
        strike_bucket: limits::strike_bucket(strike, ctx.spot),
        // The fill's own venue and exercise demands (doc 08 §4.6): the
        // hedge it needs, and the strike cash a call exercise pays /
        // the underlying value a put exercise delivers.
        hedge_notional: (greeks.delta * amount).abs() * ctx.spot,
        exercise_cash: if inputs.is_put { ctx.spot } else { strike } * amount,
        // Delta-notional change per 1% spot move (doc 08 §4.5, SO-445).
        gamma_notional_per_pct: greeks.gamma * amount * 0.01 * ctx.spot * ctx.spot,
    };
    let util = match limits::evaluate(limits_cfg, &ctx.exposure, &fill, now_ms) {
        Ok(u) => u,
        Err(hard) => return decline_hard(hard),
    };

    // Direction- and position-aware expected hedge cost (doc 08 §4.3,
    // doc 09 G2): every term is the change the fill's SIGNED incremental
    // delta causes from the CURRENT hedge position (puts carry negative
    // delta → a LONG hedge that PAYS positive funding; a put against a
    // call-heavy book merely reduces the short), never on premium.
    let incremental_delta_units = greeks.delta * amount;
    let bid_ctx = BidContext {
        // Size penalty and max-single-fill scale from the fresh risk
        // NAV the caps were just checked against (doc 08 §0.4).
        nav: util.risk_nav,
        premium_notional: fair_pu * amount,
        vega_utilization: util.vega,
        hedge_cost: pricing::desk::expected_hedge_cost(
            ctx.hedge_position_units,
            incremental_delta_units,
            ctx.spot,
            ctx.funding_rate_annual,
            ctx.expected_holding_years,
            v1.funding_income_credit,
            &ctx.hedge_cost,
        ),
        composition_utilization: util.composition,
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
    Decision::Quote {
        premium: premium as u64,
        model_fair: fair_pu * amount,
        surface_vol: sigma,
        hedge_notional: fill.hedge_notional,
        exercise_cash: fill.exercise_cash,
    }
}

fn decline_hard(hard: HardDecline) -> Decision {
    Decision::Decline {
        reason: format!("hard limit: {}", hard.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MarketModel, SurfaceConfig, V1BidParams};
    use parking_lot::RwLock;
    use std::sync::Arc;
    use vol_forecast::RollingVolBuffer;

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

    /// The 00-plan V1 starting parameters (mm-bot's `V1Config` defaults).
    fn v1() -> V1BidParams {
        V1BidParams {
            base_spread_volpts: 0.05,
            size_penalty_volpts_per_pct_nav: 0.01,
            size_penalty_quadratic_from_pct: 3.0,
            inventory_penalty_max_volpts: 0.10,
            inventory_penalty_start_util: 0.6,
            max_single_fill_pct_nav: 5.0,
            funding_income_credit: 0.0,
            composition_penalty_volpts: 0.05,
        }
    }

    fn ctx() -> FlowContext {
        FlowContext {
            spot: 100.0,
            exposure: BookExposure {
                nav: 1e9,
                capital: crate::limits::CapitalSnapshot::test_fresh(1e9, 0),
                ..Default::default()
            },
            funding_rate_annual: 0.0,
            expected_holding_years: 21.0 / 365.0,
            hedge_position_units: 0.0,
            hedge_cost: pricing::desk::HedgeCostParams {
                slippage_bps: 0.0,
                taker_fee_bps: 0.0,
                fixed_fee_per_fill: 0.0,
                rebalance_turnover_per_year: 0.0,
                margin_financing_rate_annual: 0.0,
                initial_margin_fraction: 0.10,
            },
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
            Decision::Quote { premium, .. } => *premium,
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
        // Funnel diagnostics (SO-425): the reported model fair is the
        // pre-discount fair and the surface vol is the one priced at.
        let Decision::Quote { model_fair, surface_vol, .. } = d else { unreachable!() };
        assert!((model_fair - fair).abs() < 1.0, "model_fair {model_fair} != fair {fair}");
        assert!((premium as f64) <= model_fair, "bid above its own model fair");
        assert!((surface_vol - 0.60).abs() < 1e-9, "surface_vol {surface_vol} != 0.60");
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
        // Stale capital (doc 08 §0.4): the snapshot cannot back new risk.
        let mut stale = ctx();
        stale.exposure.capital = Default::default();
        let d = price_writer_flow(&m, &v1(), &LimitsConfig::default(), &stale, &atm(1_000_000), 0);
        assert!(
            matches!(&d, Decision::Decline { reason } if reason.contains("no capital snapshot")),
            "{d:?}"
        );
    }

    /// The bid's size denominator is the fresh RISK NAV, not the
    /// indexer budget base: a haircut risk NAV makes the same clip a
    /// larger fraction of it and prices worse.
    #[test]
    fn bid_sizes_against_risk_nav() {
        let m = model();
        let limits = LimitsConfig::default();
        let full = premium_of(&price_writer_flow(&m, &v1(), &limits, &ctx(), &atm(1_000_000), 0));
        let mut haircut = ctx();
        haircut.exposure.capital.risk_nav = Some(2e8); // budget base still 1e9
        let less = premium_of(&price_writer_flow(&m, &v1(), &limits, &haircut, &atm(1_000_000), 0));
        assert!(less < full, "{less} !< {full}");
    }


    #[test]
    fn funding_sign_flows_into_the_bid_by_direction() {
        let m = model();
        let limits = LimitsConfig::default();
        let put = RfqInputs { is_put: true, ..atm(1_000_000) };
        let call = atm(1_000_000);
        let mut pos_funding = ctx();
        pos_funding.funding_rate_annual = 0.30;
        let mut neg_funding = ctx();
        neg_funding.funding_rate_annual = -0.30;

        // Positive funding: the long-perp PUT hedge pays → bid drops.
        let flat_p = premium_of(&price_writer_flow(&m, &v1(), &limits, &ctx(), &put, 0));
        let pay_p = premium_of(&price_writer_flow(&m, &v1(), &limits, &pos_funding, &put, 0));
        assert!(pay_p < flat_p, "put bid {pay_p} !< {flat_p} under positive funding");
        // The short-perp CALL hedge RECEIVES it — income is not priced
        // (funding_income_credit = 0), so the call bid is unchanged.
        let flat_c = premium_of(&price_writer_flow(&m, &v1(), &limits, &ctx(), &call, 0));
        let earn_c = premium_of(&price_writer_flow(&m, &v1(), &limits, &pos_funding, &call, 0));
        assert_eq!(flat_c, earn_c, "call bid must not price in funding income");
        // Negative funding reverses both: the call hedge now pays…
        let pay_c = premium_of(&price_writer_flow(&m, &v1(), &limits, &neg_funding, &call, 0));
        assert!(pay_c < flat_c, "call bid {pay_c} !< {flat_c} under negative funding");
        // …and the put hedge would earn (uncredited → unchanged).
        let earn_p = premium_of(&price_writer_flow(&m, &v1(), &limits, &neg_funding, &put, 0));
        assert_eq!(flat_p, earn_p, "put bid must not price in funding income");
    }

    /// Doc 08 §4.3 gate 4 / doc 09 G2 (SO-437): a put fill against a
    /// call-heavy book REDUCES the short hedge and must not be charged as
    /// if it opened a fresh long.
    #[test]
    fn put_against_call_heavy_book_is_charged_as_a_reduction() {
        let m = model();
        let limits = LimitsConfig::default();
        let put = RfqInputs { is_put: true, ..atm(1_000_000) };
        let mut flat = ctx();
        flat.funding_rate_annual = 0.30;
        flat.hedge_cost.margin_financing_rate_annual = 0.10;
        // Same book, but the desk is already short 10M hedge units
        // (deeply call-heavy): the put's −0.5M delta only trims it.
        let mut call_heavy = flat.clone();
        call_heavy.hedge_position_units = -10_000_000.0;
        let from_flat = premium_of(&price_writer_flow(&m, &v1(), &limits, &flat, &put, 0));
        let reducing = premium_of(&price_writer_flow(&m, &v1(), &limits, &call_heavy, &put, 0));
        assert!(reducing > from_flat, "reducing put bid {reducing} !> opening put bid {from_flat}");
        // And it matches the zero-funding, zero-margin price: nothing
        // but the reducing trade itself is charged.
        let mut none = ctx();
        none.hedge_cost = flat.hedge_cost;
        none.hedge_cost.margin_financing_rate_annual = 0.0;
        let unpriced = premium_of(&price_writer_flow(&m, &v1(), &limits, &none, &put, 0));
        assert_eq!(reducing, unpriced);
    }

    /// Venue fees and fixed costs are no longer hard-coded to zero.
    #[test]
    fn venue_fees_and_fixed_cost_lower_the_bid() {
        let m = model();
        let limits = LimitsConfig::default();
        let call = atm(1_000_000);
        let free = premium_of(&price_writer_flow(&m, &v1(), &limits, &ctx(), &call, 0));
        let mut fees = ctx();
        fees.hedge_cost.taker_fee_bps = 3.5;
        fees.hedge_cost.fixed_fee_per_fill = 30_000.0;
        let paid = premium_of(&price_writer_flow(&m, &v1(), &limits, &fees, &call, 0));
        assert!(paid < free, "fees {paid} !< free {free}");
    }
}
