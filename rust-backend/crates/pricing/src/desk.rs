//! Desk quoting formulas for the mm-bot vol desk
//! (docs/mm-bot-v2/00-plan.md): the V1 long-only bid built from
//! separately-testable vol-discount terms (§V1 item 1 + "V1 starting
//! parameters"). The two-sided writing quote was removed with the
//! option-writing strategy (SO-426, doc 08 §4.1).
//!
//! Everything is expressed in **vol points** (annualized decimals: 0.05 =
//! 5 vol points) applied to the model fair sigma, so each term can be
//! audited independently and logged next to the surface vol it adjusts.
//! The V1 doctrine is "degrade with size/inventory, never refuse": the only
//! `None`s are the hard max-single-fill cap (a decline is never priced) and
//! a bid whose net value after hedge costs is ≤ 0.

/// Per-quote context for the V1 bid. `premium_notional`, `nav`, and the
/// hedge-cost fields share one unit (the vault's settlement currency).
#[derive(Clone, Copy, Debug)]
pub struct BidContext {
    /// Vault NAV.
    pub nav: f64,
    /// This quote's fair premium, same units as `nav`.
    pub premium_notional: f64,
    /// Current vega utilization as a fraction of the vega cap (0..1, may
    /// exceed 1 when over the cap — the penalty keeps growing).
    pub vega_utilization: f64,
    /// Expected cash cost of hedging THIS fill, resolved by the caller
    /// (doc 08 §4.3): direction-aware funding on incremental signed
    /// hedge notional, plus fees/slippage/fixed costs.
    pub hedge_cost: ExpectedHedgeCost,
    /// Composition throttle (doc 08 §4.5, SO-445): 0 while every
    /// composition metric (gamma by type/expiry, stressed hedge loss,
    /// exercise demand, concurrent demand) sits under its soft
    /// threshold, 1.0 when one reaches its hard threshold, and beyond.
    pub composition_utilization: f64,
}

/// Expected cash cost of hedging one proposed fill over its holding
/// period (doc 08 §4.3, SO-429). All fields are premium units; the
/// funding leg may be negative (income) only up to the caller's
/// configured conservative credit — see [`expected_funding_cost`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ExpectedHedgeCost {
    /// Signed expected funding cash flow (positive = cost).
    pub funding: f64,
    pub venue_fees: f64,
    pub slippage: f64,
    pub fixed_cost: f64,
    /// Financing charge on the incremental margin the hedge parks at the
    /// venue over the holding period (doc 08 §4.3; SO-437).
    pub margin_financing: f64,
}

impl ExpectedHedgeCost {
    /// The bid subtracts this; never negative — a net funding credit can
    /// offset fees but never ADD to the bid.
    pub fn total(&self) -> f64 {
        (self.funding + self.venue_fees + self.slippage + self.fixed_cost + self.margin_financing)
            .max(0.0)
    }
}

/// Venue cost inputs for [`expected_hedge_cost`] (doc 08 §4.3, SO-437).
/// `slippage_bps` and `taker_fee_bps` are bps of traded notional;
/// `fixed_fee_per_fill` is in settlement units; `rebalance_turnover_per_year`
/// is the expected number of extra fills per year per unit of initial
/// hedge notional (doc 07 §5: ~11.3× per 30d turn at 20% bands ≈ 137/yr);
/// `margin_financing_rate_annual` × `initial_margin_fraction` prices the
/// cash parked as margin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HedgeCostParams {
    pub slippage_bps: f64,
    pub taker_fee_bps: f64,
    pub fixed_fee_per_fill: f64,
    pub rebalance_turnover_per_year: f64,
    pub margin_financing_rate_annual: f64,
    pub initial_margin_fraction: f64,
}

/// Position-aware expected hedge cost of one fill (doc 09 G2). The fill
/// moves the signed hedge from `position_before_units` to
/// `position_before_units − incremental_delta_units`; every term is the
/// CHANGE that move causes, so a fill that reduces an existing hedge is
/// charged only the trade it takes to reduce it, never as if it opened a
/// fresh position.
pub fn expected_hedge_cost(
    position_before_units: f64,
    incremental_delta_units: f64,
    spot: f64,
    funding_rate_annual: f64,
    holding_years: f64,
    income_credit: f64,
    p: &HedgeCostParams,
) -> ExpectedHedgeCost {
    let after = position_before_units - incremental_delta_units;
    let trade_notional = incremental_delta_units.abs() * spot;
    let fills = 1.0 + p.rebalance_turnover_per_year.max(0.0) * holding_years.max(0.0);
    let turnover_notional = trade_notional * fills;
    let margin_delta = ((after.abs() - position_before_units.abs()) * spot).max(0.0);
    ExpectedHedgeCost {
        funding: expected_funding_cost_from_position(
            position_before_units,
            incremental_delta_units,
            spot,
            funding_rate_annual,
            holding_years,
            income_credit,
        ),
        venue_fees: turnover_notional * p.taker_fee_bps / 10_000.0,
        slippage: turnover_notional * p.slippage_bps / 10_000.0,
        fixed_cost: p.fixed_fee_per_fill * fills,
        margin_financing: margin_delta
            * p.initial_margin_fraction
            * p.margin_financing_rate_annual
            * holding_years.max(0.0),
    }
}

/// Position-aware expected funding (doc 09 G2): `cost(after) − cost(before)`
/// where `cost(pos) = funding × pos × spot × T` for a long (pays positive
/// funding) and a short's income is credited at `income_credit`. Reducing
/// a short under positive funding therefore costs only the credited income
/// it gives up — zero at the conservative default.
pub fn expected_funding_cost_from_position(
    position_before_units: f64,
    incremental_delta_units: f64,
    spot: f64,
    funding_rate_annual: f64,
    holding_years: f64,
    income_credit: f64,
) -> f64 {
    let cost = |pos: f64| {
        let c = funding_rate_annual * pos * spot * holding_years;
        if c >= 0.0 {
            c
        } else {
            c * income_credit.clamp(0.0, 1.0)
        }
    };
    cost(position_before_units - incremental_delta_units) - cost(position_before_units)
}

/// Direction-aware expected funding for one fill's incremental hedge
/// (doc 08 §4.3). The hedge for incremental option delta `d` is `−d`
/// signed perp units; market convention has longs PAY positive funding
/// and shorts receive it, so the expected cash flow is
/// `funding × (−d × spot) × holding_years` (positive = cost). Funding is
/// charged on hedge NOTIONAL, never on option premium. Income (a
/// negative cost) is credited only at `income_credit` — 0 is the
/// conservative default: income is upside, never priced into the bid.
pub fn expected_funding_cost(
    incremental_delta_units: f64,
    spot: f64,
    funding_rate_annual: f64,
    holding_years: f64,
    income_credit: f64,
) -> f64 {
    // The from-flat special case of the position-aware form.
    expected_funding_cost_from_position(
        0.0,
        incremental_delta_units,
        spot,
        funding_rate_annual,
        holding_years,
        income_credit,
    )
}

/// V1 bid parameters (plan "V1 starting parameters"). Requires
/// `inventory_penalty_start_util < 1.0` and positive thresholds.
#[derive(Clone, Copy, Debug)]
pub struct V1BidParams {
    /// Base spread below fair, vol points (starting 0.04–0.06).
    pub base_spread_volpts: f64,
    /// Size penalty per 1% of NAV of premium notional (starting 0.01).
    pub size_penalty_volpts_per_pct_nav: f64,
    /// %NAV beyond which the size penalty turns quadratic (starting ~3.0).
    pub size_penalty_quadratic_from_pct: f64,
    /// Inventory penalty at 100% vega utilization (starting 0.10).
    pub inventory_penalty_max_volpts: f64,
    /// Utilization below which there is no inventory penalty (starting 0.6).
    pub inventory_penalty_start_util: f64,
    /// Hard cap: max premium for a single fill, % of NAV (starting 5.0).
    /// Beyond this the quote is declined, not priced.
    pub max_single_fill_pct_nav: f64,
    /// Fraction of expected funding INCOME credited into the bid
    /// (0 = conservative: income is upside, never priced — doc 08 §4.3).
    pub funding_income_credit: f64,
    /// Bid widening at composition utilization 1.0 (a hard composition
    /// threshold), vol points; keeps growing above it (SO-445).
    pub composition_penalty_volpts: f64,
}

/// The V1 vol discount decomposed into its terms (all vol points), so each
/// can be logged and unit-tested on its own. `total = base + size +
/// inventory + composition`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VolDiscount {
    pub base: f64,
    pub size: f64,
    pub inventory: f64,
    /// Composition throttle (doc 08 §4.5): linear in the composition
    /// utilization, alongside — not instead of — the vega inventory term.
    pub composition: f64,
    pub total: f64,
}

/// The V1 vol discount for one quote. Returns `None` only when the quote
/// exceeds the max-single-fill cap (or NAV is non-positive, which makes
/// sizing meaningless) — a decline is never priced.
///
/// - **size**: `per_pct · pct` of NAV up to the quadratic threshold, then
///   `per_pct · pct²/threshold` (continuous at the threshold, ~quadratic
///   beyond — plan: "+1 vol pt per (notional / 1% NAV), ~quadratic beyond
///   3% NAV").
/// - **inventory**: 0 below `start_util`, then linear so it reaches
///   `max_volpts` exactly at utilization 1.0, and *keeps growing* at the
///   same slope above 1.0 — widen, never stop.
pub fn v1_vol_discount(ctx: &BidContext, p: &V1BidParams) -> Option<VolDiscount> {
    if ctx.nav <= 0.0 {
        return None;
    }
    let pct = 100.0 * ctx.premium_notional / ctx.nav;
    if pct > p.max_single_fill_pct_nav {
        return None;
    }

    let size = if pct <= p.size_penalty_quadratic_from_pct {
        p.size_penalty_volpts_per_pct_nav * pct
    } else {
        p.size_penalty_volpts_per_pct_nav * pct * pct / p.size_penalty_quadratic_from_pct
    };

    let inventory = if ctx.vega_utilization <= p.inventory_penalty_start_util {
        0.0
    } else {
        p.inventory_penalty_max_volpts * (ctx.vega_utilization - p.inventory_penalty_start_util)
            / (1.0 - p.inventory_penalty_start_util)
    };

    let composition = p.composition_penalty_volpts * ctx.composition_utilization.max(0.0);

    let base = p.base_spread_volpts;
    Some(VolDiscount {
        base,
        size,
        inventory,
        composition,
        total: base + size + inventory + composition,
    })
}

/// The V1 bid: fair value repriced at the discounted vol, minus the
/// caller-resolved expected hedge cost (doc 08 §4.3 — funding on signed
/// incremental hedge notional, direction-aware; never on premium).
///
/// `fair_at(σ)` must return the fair premium for the *full quote size* at
/// vol σ, in the same units as `ctx.premium_notional` (so the vol discount
/// and the absolute hedge costs compose). The bid vol is
/// `max(σ_fair − discount.total, 0)` — a deep discount collapses toward
/// intrinsic rather than going negative. Returns `None` when the discount
/// declines the quote or the net bid is ≤ 0.
pub fn v1_bid(
    fair_at: impl Fn(f64) -> f64,
    sigma: f64,
    ctx: &BidContext,
    p: &V1BidParams,
) -> Option<f64> {
    let discount = v1_vol_discount(ctx, p)?;
    let sigma_bid = (sigma - discount.total).max(0.0);
    let bid = fair_at(sigma_bid) - ctx.hedge_cost.total();
    if bid > 0.0 { Some(bid) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{call_price_per_unit, CallInputs};

    fn close(a: f64, b: f64, eps: f64) {
        assert!((a - b).abs() < eps, "{a} vs {b}, eps {eps}");
    }

    /// The plan's "V1 starting parameters".
    fn v1_params() -> V1BidParams {
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

    /// NAV 100_000; premium expressed as % of it, utilization as given;
    /// no hedge cost unless a test sets it.
    fn ctx(premium_pct: f64, util: f64) -> BidContext {
        BidContext {
            nav: 100_000.0,
            premium_notional: 1_000.0 * premium_pct,
            vega_utilization: util,
            hedge_cost: ExpectedHedgeCost::default(),
            composition_utilization: 0.0,
        }
    }

    /// Doc 08 §4.5 (SO-445): the composition throttle widens the bid
    /// linearly in its utilization, on top of the vega inventory term.
    #[test]
    fn v1_composition_throttle_widens_alongside_inventory() {
        let p = v1_params();
        let flat = v1_vol_discount(&ctx(0.0, 0.8), &p).unwrap();
        close(flat.composition, 0.0, 1e-12);
        let c = BidContext { composition_utilization: 0.5, ..ctx(0.0, 0.8) };
        let d = v1_vol_discount(&c, &p).unwrap();
        close(d.composition, 0.025, 1e-12);
        close(d.inventory, flat.inventory, 1e-12);
        close(d.total, flat.total + 0.025, 1e-12);
        // At a hard threshold the full penalty applies, and it keeps
        // growing past it — widen, never stop.
        let c = BidContext { composition_utilization: 1.5, ..ctx(0.0, 0.8) };
        close(v1_vol_discount(&c, &p).unwrap().composition, 0.075, 1e-12);
    }

    #[test]
    fn v1_discount_base_plus_linear_size() {
        // 1% NAV, no inventory: base 0.05 + size 0.01·1 = 0.06 total.
        let d = v1_vol_discount(&ctx(1.0, 0.0), &v1_params()).unwrap();
        close(d.base, 0.05, 1e-12);
        close(d.size, 0.01, 1e-12);
        close(d.inventory, 0.0, 1e-12);
        close(d.total, 0.06, 1e-12);
    }

    #[test]
    fn v1_size_penalty_goes_quadratic_past_threshold() {
        let p = v1_params();
        // At the 3% threshold the linear and quadratic branches agree.
        close(v1_vol_discount(&ctx(3.0, 0.0), &p).unwrap().size, 0.03, 1e-12);
        // 4% NAV: 0.01·4²/3 = 0.05333… > the linear 0.04.
        let d = v1_vol_discount(&ctx(4.0, 0.0), &p).unwrap();
        close(d.size, 0.01 * 16.0 / 3.0, 1e-12);
        assert!(d.size > 0.04);
        // 5% NAV (at the cap, still quoted): 0.01·25/3 = 0.08333…
        close(v1_vol_discount(&ctx(5.0, 0.0), &p).unwrap().size, 0.01 * 25.0 / 3.0, 1e-12);
    }

    #[test]
    fn v1_declines_oversize_and_bad_nav() {
        let p = v1_params();
        assert!(v1_vol_discount(&ctx(5.01, 0.0), &p).is_none());
        assert!(v1_vol_discount(&BidContext { nav: 0.0, ..ctx(1.0, 0.0) }, &p).is_none());
        // At exactly the cap it's still priced (decline is `>`, not `>=`).
        assert!(v1_vol_discount(&ctx(5.0, 0.0), &p).is_some());
    }

    #[test]
    fn v1_inventory_penalty_ramps_from_start_util_and_never_stops() {
        let p = v1_params();
        // ≤ 60% utilization: free.
        close(v1_vol_discount(&ctx(0.0, 0.0), &p).unwrap().inventory, 0.0, 1e-12);
        close(v1_vol_discount(&ctx(0.0, 0.6), &p).unwrap().inventory, 0.0, 1e-12);
        // Linear 0.6 → 1.0: at 0.8 exactly half of the 0.10 max.
        close(v1_vol_discount(&ctx(0.0, 0.8), &p).unwrap().inventory, 0.05, 1e-12);
        close(v1_vol_discount(&ctx(0.0, 1.0), &p).unwrap().inventory, 0.10, 1e-12);
        // Past 100% the same slope keeps widening — never stop quoting.
        close(v1_vol_discount(&ctx(0.0, 1.2), &p).unwrap().inventory, 0.15, 1e-12);
    }

    #[test]
    fn v1_bid_arithmetic_with_synthetic_linear_fair() {
        // fair_at(σ) = 10_000·σ makes every term hand-checkable.
        // σ 0.60, 1% NAV (discount 0.06), funding cost 10, slippage 5.
        let c = BidContext {
            hedge_cost: ExpectedHedgeCost { funding: 10.0, slippage: 5.0, ..Default::default() },
            ..ctx(1.0, 0.0)
        };
        let bid = v1_bid(|s| 10_000.0 * s, 0.60, &c, &v1_params()).unwrap();
        close(bid, 10_000.0 * (0.60 - 0.06) - 10.0 - 5.0, 1e-9);
        // A net funding credit can offset fees but never ADDS to the bid.
        let c_earn = BidContext {
            hedge_cost: ExpectedHedgeCost { funding: -25.0, slippage: 5.0, ..Default::default() },
            ..ctx(1.0, 0.0)
        };
        let bid = v1_bid(|s| 10_000.0 * s, 0.60, &c_earn, &v1_params()).unwrap();
        close(bid, 10_000.0 * 0.54, 1e-9);
    }

    #[test]
    fn v1_bid_sits_below_fair_with_real_bs_pricing() {
        let (spot, strike, t) = (3.5, 4.0, 30.0 / 365.0);
        let sigma = 0.75;
        let units = 10_000.0;
        let fair_at = |s: f64| {
            units * call_price_per_unit(CallInputs { spot, strike, t_years: t, r: 0.0, sigma: s })
        };
        let fair = fair_at(sigma);
        let c = BidContext {
            nav: 1_000_000.0,
            premium_notional: fair,
            vega_utilization: 0.8,
            hedge_cost: ExpectedHedgeCost {
                funding: fair * 0.01,
                slippage: fair * 0.002,
                ..Default::default()
            },
            composition_utilization: 0.0,
        };
        let bid = v1_bid(fair_at, sigma, &c, &v1_params()).unwrap();
        assert!(bid > 0.0 && bid < fair, "bid {bid} not inside (0, fair {fair})");
    }

    #[test]
    fn v1_bid_declines_oversize_and_nonpositive_net() {
        let p = v1_params();
        // Oversize: > 5% NAV premium.
        assert!(v1_bid(|s| 10_000.0 * s, 0.6, &ctx(6.0, 0.0), &p).is_none());
        // Vol discount swallows the whole sigma → intrinsic 0 → net ≤ 0.
        assert!(v1_bid(|s| 100.0 * s, 0.05, &ctx(1.0, 0.0), &p).is_none());
        // Hedge costs exceed the discounted fair → net ≤ 0.
        let c = BidContext {
            hedge_cost: ExpectedHedgeCost { slippage: 10_000.0, ..Default::default() },
            ..ctx(1.0, 0.0)
        };
        assert!(v1_bid(|s| 10_000.0 * s, 0.60, &c, &p).is_none());
    }

    // ── direction-aware funding (doc 08 §4.3, SO-429) ──────────────────

    #[test]
    fn funding_sign_matrix() {
        // Long CALL book: fill delta +100 units → short hedge. Positive
        // funding is INCOME for a short: credit 0 charges nothing,
        // credit 1 credits it in full.
        close(expected_funding_cost(100.0, 10.0, 0.10, 0.1, 0.0), 0.0, 1e-12);
        close(expected_funding_cost(100.0, 10.0, 0.10, 0.1, 1.0), -10.0, 1e-12);
        // Negative funding: the short PAYS — always charged.
        close(expected_funding_cost(100.0, 10.0, -0.10, 0.1, 0.0), 10.0, 1e-12);
        // Long PUT book: fill delta −100 → LONG hedge. Positive funding
        // is a COST for a long — always charged.
        close(expected_funding_cost(-100.0, 10.0, 0.10, 0.1, 0.0), 10.0, 1e-12);
        // Negative funding: the long RECEIVES — income, credit-gated.
        close(expected_funding_cost(-100.0, 10.0, -0.10, 0.1, 0.0), 0.0, 1e-12);
        close(expected_funding_cost(-100.0, 10.0, -0.10, 0.1, 0.5), -5.0, 1e-12);
    }

    #[test]
    fn funding_is_proportional_to_hedge_notional_not_premium() {
        // Doubling the fill's delta doubles the funding charge; the
        // premium never enters.
        let one = expected_funding_cost(-100.0, 10.0, 0.10, 0.1, 0.0);
        let two = expected_funding_cost(-200.0, 10.0, 0.10, 0.1, 0.0);
        close(two, 2.0 * one, 1e-12);
        // A delta-net mixed fill (net delta 0) has zero incremental cost.
        close(expected_funding_cost(0.0, 10.0, 0.10, 0.1, 0.0), 0.0, 1e-12);
    }

    #[test]
    fn hedge_cost_total_floors_at_zero() {
        let c = ExpectedHedgeCost {
            funding: -50.0,
            venue_fees: 10.0,
            slippage: 5.0,
            fixed_cost: 0.0,
            margin_financing: 0.0,
        };
        close(c.total(), 0.0, 1e-12);
        let c = ExpectedHedgeCost {
            funding: -5.0,
            venue_fees: 10.0,
            slippage: 5.0,
            fixed_cost: 1.0,
            margin_financing: 2.0,
        };
        close(c.total(), 13.0, 1e-12);
    }

    // ── position-aware hedge cost (doc 09 G2, SO-437) ──────────────────

    fn params() -> HedgeCostParams {
        HedgeCostParams {
            slippage_bps: 2.0,
            taker_fee_bps: 3.5,
            fixed_fee_per_fill: 0.03,
            rebalance_turnover_per_year: 0.0,
            margin_financing_rate_annual: 0.05,
            initial_margin_fraction: 0.10,
        }
    }

    #[test]
    fn reducing_fill_is_not_charged_as_a_fresh_position() {
        // Call-heavy book: hedge is short 100. A put fill (delta −20)
        // REDUCES the short to 80. Under positive funding the short still
        // receives; with income uncredited the funding change is zero and
        // no new margin is parked — only the reducing trade's own fees.
        let c = expected_hedge_cost(-100.0, -20.0, 10.0, 0.10, 0.1, 0.0, &params());
        close(c.funding, 0.0, 1e-12);
        close(c.margin_financing, 0.0, 1e-12);
        close(c.venue_fees, 200.0 * 3.5 / 10_000.0, 1e-12);
        // The same put from FLAT opens a long: funding and margin both bite.
        let o = expected_hedge_cost(0.0, -20.0, 10.0, 0.10, 0.1, 0.0, &params());
        close(o.funding, 0.10 * 20.0 * 10.0 * 0.1, 1e-12);
        close(o.margin_financing, 200.0 * 0.10 * 0.05 * 0.1, 1e-12);
        assert!(o.total() > c.total());
        // With income credited, reducing the short gives up income: a cost.
        let credited = expected_funding_cost_from_position(-100.0, -20.0, 10.0, 0.10, 0.1, 1.0);
        close(credited, 2.0, 1e-12);
    }

    #[test]
    fn turnover_scales_fees_slippage_and_fixed_cost() {
        let mut p = params();
        p.rebalance_turnover_per_year = 100.0; // ×0.1y → 10 extra fills
        let c = expected_hedge_cost(0.0, 100.0, 10.0, 0.0, 0.1, 0.0, &p);
        close(c.venue_fees, 1000.0 * 11.0 * 3.5 / 10_000.0, 1e-9);
        close(c.slippage, 1000.0 * 11.0 * 2.0 / 10_000.0, 1e-9);
        close(c.fixed_cost, 0.03 * 11.0, 1e-12);
    }
}
