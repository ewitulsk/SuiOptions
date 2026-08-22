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

/// Per-quote context for the V1 bid. `premium_notional`, `nav`, and
/// `slippage_cost` share one unit (the vault's settlement currency).
#[derive(Clone, Copy, Debug)]
pub struct BidContext {
    /// Vault NAV.
    pub nav: f64,
    /// This quote's fair premium, same units as `nav`.
    pub premium_notional: f64,
    /// Current vega utilization as a fraction of the vega cap (0..1, may
    /// exceed 1 when over the cap — the penalty keeps growing).
    pub vega_utilization: f64,
    /// Annualized funding rate on the perp hedge; positive = shorts earn
    /// (funding is income), negative = shorts pay (funding is a cost).
    pub funding_rate_annual: f64,
    /// Expected holding period of the position (years), for the funding
    /// cost estimate.
    pub expected_holding_years: f64,
    /// Absolute hedge slippage estimate, premium units.
    pub slippage_cost: f64,
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
}

/// The V1 vol discount decomposed into its terms (all vol points), so each
/// can be logged and unit-tested on its own. `total = base + size +
/// inventory`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VolDiscount {
    pub base: f64,
    pub size: f64,
    pub inventory: f64,
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

    let base = p.base_spread_volpts;
    Some(VolDiscount { base, size, inventory, total: base + size + inventory })
}

/// The V1 bid: fair value repriced at the discounted vol, minus the hedge
/// cost estimate.
///
/// `fair_at(σ)` must return the fair premium for the *full quote size* at
/// vol σ, in the same units as `ctx.premium_notional` (so the vol discount
/// and the absolute hedge costs compose). The bid vol is
/// `max(σ_fair − discount.total, 0)` — a deep discount collapses toward
/// intrinsic rather than going negative. Hedge cost =
/// `max(0, −funding_rate_annual) · expected_holding_years ·
/// premium_notional + slippage_cost`: when shorts *earn* funding the term
/// is 0 (income is upside, not priced into the bid); when shorts pay, the
/// expected payment over the holding period is charged. Returns `None`
/// when the discount declines the quote or the net bid is ≤ 0.
pub fn v1_bid(
    fair_at: impl Fn(f64) -> f64,
    sigma: f64,
    ctx: &BidContext,
    p: &V1BidParams,
) -> Option<f64> {
    let discount = v1_vol_discount(ctx, p)?;
    let sigma_bid = (sigma - discount.total).max(0.0);
    let hedge_cost = (-ctx.funding_rate_annual).max(0.0)
        * ctx.expected_holding_years
        * ctx.premium_notional
        + ctx.slippage_cost;
    let bid = fair_at(sigma_bid) - hedge_cost;
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
        }
    }

    /// NAV 100_000; premium expressed as % of it, utilization as given; no
    /// funding/slippage unless a test sets them.
    fn ctx(premium_pct: f64, util: f64) -> BidContext {
        BidContext {
            nav: 100_000.0,
            premium_notional: 1_000.0 * premium_pct,
            vega_utilization: util,
            funding_rate_annual: 0.0,
            expected_holding_years: 0.0,
            slippage_cost: 0.0,
        }
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
        // σ 0.60, 1% NAV (discount 0.06), shorts pay 10% funding for 0.1y on
        // premium 1000 → hedge 10, slippage 5.
        let c = BidContext {
            funding_rate_annual: -0.10,
            expected_holding_years: 0.1,
            slippage_cost: 5.0,
            ..ctx(1.0, 0.0)
        };
        let bid = v1_bid(|s| 10_000.0 * s, 0.60, &c, &v1_params()).unwrap();
        close(bid, 10_000.0 * (0.60 - 0.06) - 10.0 - 5.0, 1e-9);
        // Positive funding (shorts earn) is not priced in: only slippage.
        let c_earn = BidContext { funding_rate_annual: 0.25, ..c };
        let bid = v1_bid(|s| 10_000.0 * s, 0.60, &c_earn, &v1_params()).unwrap();
        close(bid, 10_000.0 * 0.54 - 5.0, 1e-9);
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
            funding_rate_annual: -0.05,
            expected_holding_years: 20.0 / 365.0,
            slippage_cost: fair * 0.002,
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
        let c = BidContext { slippage_cost: 10_000.0, ..ctx(1.0, 0.0) };
        assert!(v1_bid(|s| 10_000.0 * s, 0.60, &c, &p).is_none());
    }
}
