//! Pure predicted-APY math. No I/O — fed resolved market data + vault state.
//! Ported unchanged from the Sui twin (the math has no chain in it); only the
//! Tier-1 premium *source* differs upstream (venue auctions instead of RFQs).
//!
//! Tier 1 annualizes the premium the vault is on track to collect this round
//! (from live auction bids). Tier 2 prices the next K rounds' premium with
//! Black–Scholes at the keeper's delta-target strike. Both then subtract the
//! *expected assignment cost* — `E[max(S_T − K, 0)]`, the probability-weighted
//! payout of the call the vault sold — and net fees.
//!
//! Net of assignment, a fairly-priced call earns ≈ −fees: the premium is the
//! market price of the upside you give up, so there is no edge unless implied
//! vol exceeds realized (the variance risk premium). Tier 2 makes that VRP an
//! explicit, config-driven assumption (`assumed_vrp`); Tier 1 takes the premium
//! as observed. Each point carries a `[apy_low, apy_high]` range, the per-round
//! assignment probability `N(d2)`, and a `downside_round_yield` — the single
//! round's loss *if* the call is assigned.
//!
//! Annualization is simple (linear), not compound: compounding a short-cadence
//! (hourly) round's tiny premium over thousands of rounds explodes it into
//! absurd APYs. The result is clamped to ±`apy_cap`.

use pricing::{assignment_prob, call_price_per_unit, strike_for_delta, CallInputs};

const YEAR_MS: f64 = 365.25 * 86_400_000.0;

#[derive(Debug, Clone, PartialEq)]
pub struct PredictionPoint {
    /// `current` (Tier 1) | `forecast` (Tier 2).
    pub kind: &'static str,
    /// 0 = current round; 1..=K = rounds ahead.
    pub horizon: i32,
    pub t_ms: i64,
    /// Central annualized APY: premium net of expected assignment and fees.
    pub apy: f64,
    /// Low end of the range (more drag / smaller VRP).
    pub apy_low: f64,
    /// High end of the range (less drag / larger VRP).
    pub apy_high: f64,
    /// Probability the call finishes ITM and the vault is assigned this round.
    pub assignment_prob: f64,
    /// Per-round (NOT annualized) net yield *conditional on assignment* — the
    /// drawdown of a single bad round. Typically negative.
    pub downside_round_yield: f64,
    pub confidence: f64,
}

/// Resolved inputs for one vault at one tick.
pub struct VaultInputs {
    /// USD cross: settlement units per 1 underlying.
    pub spot: f64,
    /// Annualized realized vol of the underlying.
    pub sigma: f64,
    /// Assets under management, in whole underlying units.
    pub aum_underlying: f64,
    /// Round length in ms (derived from finalize spacing, else config).
    pub round_ms: u64,
    /// Expiry of the current round (ms) — where the Tier-1 point lands.
    pub current_expiry_ms: i64,
    /// Premium the vault is on track to collect this round, in whole
    /// underlying units (Tier 1).
    pub current_premium_underlying: f64,
    /// Fraction of the round's slice notional already settled (Tier-1
    /// confidence: rises toward 1.0 as auctions close).
    pub current_premium_confidence: f64,
    /// Strike actually sold this round, in spot units (settlement per
    /// underlying). `None` if not yet indexed — Tier 1 then falls back to
    /// premium-only (no assignment term).
    pub current_strike: Option<f64>,
    /// Performance fee as a fraction (e.g. 0.10).
    pub perf_fee: f64,
    /// Management fee, annualized fraction (e.g. 0.02).
    pub mgmt_fee_annual: f64,
    /// Tier-2 horizon (number of future rounds).
    pub horizon: u32,
    /// Keeper's delta-target snap (e.g. 0.10).
    pub delta_target: f64,
    /// Clamp annualized APY to ±this fraction.
    pub apy_cap: f64,
    /// Tier-2 assumed variance risk premium (vol points): implied = realized +
    /// this.
    pub assumed_vrp: f64,
    /// Tier-2 range half-width in vol points around `assumed_vrp`.
    pub vrp_band: f64,
    /// Tier-1 range half-width as a fraction of realized vol.
    pub vol_band: f64,
}

/// Expected assignment economics at one strike under one vol, per unit of
/// underlying notional (fraction of spot).
struct Assignment {
    /// `E[max(S_T − K, 0)] / spot` — the unconditional expected cost.
    expected: f64,
    /// `N(d2)` — probability the call finishes ITM.
    prob: f64,
    /// `E[max(S_T − K, 0) | assigned] / spot` — the cost *given* assignment.
    conditional: f64,
}

fn assignment(spot: f64, strike: f64, t_years: f64, sigma: f64) -> Assignment {
    let ci = CallInputs { spot, strike, t_years, r: 0.0, sigma };
    let expected = call_price_per_unit(ci) / spot;
    let prob = assignment_prob(ci);
    let conditional = if prob > 1e-9 { expected / prob } else { 0.0 };
    Assignment { expected, prob, conditional }
}

/// Net a gross round yield for fees: performance fee on profit only, plus this
/// round's share of the annual management fee. At zero gross profit this leaves
/// exactly `−mgmt_round`, which is why a fairly-priced round annualizes to
/// `−mgmt_fee_annual`.
fn net_of_fees(gross: f64, perf_fee: f64, mgmt_round: f64) -> f64 {
    gross - perf_fee * gross.max(0.0) - mgmt_round
}

/// Simple (linear) annualization of a per-round yield, clamped to ±`cap`.
/// `None` only for a degenerate round length.
fn annualize(round_yield: f64, round_ms: u64, cap: f64) -> Option<f64> {
    if round_ms == 0 {
        return None;
    }
    let apy = round_yield * (YEAR_MS / round_ms as f64);
    apy.is_finite().then(|| apy.clamp(-cap, cap))
}

pub fn predict(i: &VaultInputs) -> Vec<PredictionPoint> {
    let mut out = Vec::new();
    let t_years = i.round_ms as f64 / YEAR_MS;
    let mgmt_round = i.mgmt_fee_annual * t_years;

    // ── Tier 1: current round, from live RFQ premium ──
    if i.aum_underlying > 0.0 && i.current_premium_underlying >= 0.0 {
        let premium_yield = i.current_premium_underlying / i.aum_underlying;

        // Net expected assignment at the strike actually sold (per unit of AUM,
        // i.e. assuming the vault writes its full AUM as notional). Without a
        // strike or vol we can't model drag — fall back to premium-only. The
        // range brackets realized vol: more vol → more drag → lower APY.
        let (a_mid, a_less_drag, a_more_drag, prob, conditional) = match i.current_strike {
            Some(k) if i.sigma > 0.0 && k > 0.0 => {
                let mid = assignment(i.spot, k, t_years, i.sigma);
                let less = assignment(i.spot, k, t_years, i.sigma * (1.0 - i.vol_band));
                let more = assignment(i.spot, k, t_years, i.sigma * (1.0 + i.vol_band));
                (mid.expected, less.expected, more.expected, mid.prob, mid.conditional)
            }
            _ => (0.0, 0.0, 0.0, 0.0, 0.0),
        };

        let apy = annualize(net_of_fees(premium_yield - a_mid, i.perf_fee, mgmt_round), i.round_ms, i.apy_cap);
        let apy_high = annualize(net_of_fees(premium_yield - a_less_drag, i.perf_fee, mgmt_round), i.round_ms, i.apy_cap);
        let apy_low = annualize(net_of_fees(premium_yield - a_more_drag, i.perf_fee, mgmt_round), i.round_ms, i.apy_cap);

        if let (Some(apy), Some(apy_low), Some(apy_high)) = (apy, apy_low, apy_high) {
            out.push(PredictionPoint {
                kind: "current",
                horizon: 0,
                t_ms: i.current_expiry_ms,
                apy,
                apy_low,
                apy_high,
                assignment_prob: prob,
                downside_round_yield: net_of_fees(premium_yield - conditional, i.perf_fee, mgmt_round),
                confidence: i.current_premium_confidence.clamp(0.0, 1.0),
            });
        }
    }

    // ── Tier 2: forecast next K rounds via Black–Scholes ──
    if i.spot > 0.0 && i.sigma > 0.0 {
        let strike = strike_for_delta(i.spot, i.sigma, t_years, 0.0, i.delta_target);
        // Premium priced at implied vol = realized + VRP; assignment at realized
        // vol. The whole edge is the VRP — at vrp = 0 the two cancel to −fees.
        let a = assignment(i.spot, strike, t_years, i.sigma);
        let premium_yield = |vrp: f64| {
            call_price_per_unit(CallInputs {
                spot: i.spot,
                strike,
                t_years,
                r: 0.0,
                sigma: i.sigma + vrp.max(0.0),
            }) / i.spot
        };
        let net = |vrp: f64| net_of_fees(premium_yield(vrp) - a.expected, i.perf_fee, mgmt_round);

        let apy = annualize(net(i.assumed_vrp), i.round_ms, i.apy_cap);
        let apy_low = annualize(net((i.assumed_vrp - i.vrp_band).max(0.0)), i.round_ms, i.apy_cap);
        let apy_high = annualize(net(i.assumed_vrp + i.vrp_band), i.round_ms, i.apy_cap);
        let downside = net_of_fees(premium_yield(i.assumed_vrp) - a.conditional, i.perf_fee, mgmt_round);

        if let (Some(apy), Some(apy_low), Some(apy_high)) = (apy, apy_low, apy_high) {
            for n in 1..=i.horizon as i64 {
                out.push(PredictionPoint {
                    // Stationary spot/vol assumption → flat forecast; the
                    // horizon extends the line, confidence decays with it.
                    kind: "forecast",
                    horizon: n as i32,
                    t_ms: i.current_expiry_ms + n * i.round_ms as i64,
                    apy,
                    apy_low,
                    apy_high,
                    assignment_prob: a.prob,
                    downside_round_yield: downside,
                    confidence: 0.6_f64.powi(n as i32),
                });
            }
        }
    }

    out
}

/// Median finalize-to-finalize spacing across a vault's finalized rounds, or
/// `None` with fewer than two. Used to derive `round_ms` from observed cadence.
pub fn median_round_ms(mut finalize_ms: Vec<u64>) -> Option<u64> {
    finalize_ms.sort_unstable();
    if finalize_ms.len() < 2 {
        return None;
    }
    let mut diffs: Vec<u64> = finalize_ms.windows(2).map(|w| w[1] - w[0]).collect();
    diffs.sort_unstable();
    Some(diffs[diffs.len() / 2])
}

#[cfg(test)]
mod tests {
    use super::*;
    use pricing::{call_price_per_unit, strike_for_delta, CallInputs};

    const WEEK: u64 = 7 * 86_400_000;
    const HOUR: u64 = 3_600_000;

    /// Annualization factor (rounds per year) for a round length.
    fn periods(round_ms: u64) -> f64 {
        YEAR_MS / round_ms as f64
    }

    /// Baseline inputs; tests override the fields they exercise.
    fn inputs() -> VaultInputs {
        VaultInputs {
            spot: 50_000.0,
            sigma: 0.6,
            aum_underlying: 100.0,
            round_ms: WEEK,
            current_expiry_ms: 1_000,
            current_premium_underlying: 0.0,
            current_premium_confidence: 0.5,
            current_strike: None,
            perf_fee: 0.10,
            mgmt_fee_annual: 0.02,
            horizon: 4,
            delta_target: 0.10,
            apy_cap: 5.0,
            assumed_vrp: 0.04,
            vrp_band: 0.02,
            vol_band: 0.25,
        }
    }

    #[test]
    fn tier1_premium_only_when_no_strike() {
        // sigma 0 disables Tier 2; no strike → premium-only, net of fees,
        // simply annualized. premium 0.2/100 = 0.002/wk, perf 10%, mgmt 2%/yr:
        //   net_round = 0.002 − 0.0002 − 0.02·(7/365.25) ≈ 0.0014167
        //   apy = 0.0014167 · (365.25/7) ≈ 0.0739
        let i = VaultInputs {
            sigma: 0.0,
            current_premium_underlying: 0.2,
            ..inputs()
        };
        let pts = predict(&i);
        let cur = pts.iter().find(|p| p.kind == "current").unwrap();
        assert!((cur.apy - 0.0739).abs() < 0.005, "apy {}", cur.apy);
        assert_eq!(cur.assignment_prob, 0.0); // no strike → no drag modeled
        assert!(pts.iter().all(|p| p.kind != "forecast"));
    }

    #[test]
    fn tier1_nets_assignment_with_strike() {
        // With the actual sold strike + vol, Tier 1 subtracts expected
        // assignment and emits a vol-bracketed range + a downside.
        let strike = strike_for_delta(50_000.0, 0.6, WEEK as f64 / YEAR_MS, 0.0, 0.10);
        let i = VaultInputs {
            current_premium_underlying: 0.4,
            current_strike: Some(strike),
            ..inputs()
        };
        let cur = predict(&i).into_iter().find(|p| p.kind == "current").unwrap();
        assert!(cur.apy_low < cur.apy_high, "range {} .. {}", cur.apy_low, cur.apy_high);
        assert!(cur.apy_low <= cur.apy && cur.apy <= cur.apy_high);
        assert!(cur.assignment_prob > 0.0 && cur.assignment_prob < 0.10);
        assert!(cur.downside_round_yield < 0.0, "downside {}", cur.downside_round_yield);
    }

    #[test]
    fn tier2_fairly_priced_round_earns_minus_fees() {
        // The anti-1600% guard: with no variance risk premium, the premium
        // exactly offsets expected assignment, so net APY ≈ −mgmt_fee_annual.
        let i = VaultInputs {
            aum_underlying: 0.0, // disable Tier 1
            assumed_vrp: 0.0,
            vrp_band: 0.0,
            ..inputs()
        };
        let fc = predict(&i).into_iter().find(|p| p.kind == "forecast").unwrap();
        assert!((fc.apy - -0.02).abs() < 1e-6, "apy {} (want ≈ −mgmt)", fc.apy);
    }

    #[test]
    fn tier2_emits_horizon_points_with_range() {
        let i = VaultInputs { aum_underlying: 0.0, ..inputs() };
        let pts = predict(&i);
        let fc: Vec<_> = pts.iter().filter(|p| p.kind == "forecast").collect();
        assert_eq!(fc.len(), 4);
        assert_eq!(fc[0].t_ms, 1_000 + WEEK as i64);
        assert!(fc[0].apy > 0.0, "vrp 4pts should give positive edge: {}", fc[0].apy);
        assert!(fc[0].apy_low < fc[0].apy && fc[0].apy < fc[0].apy_high);
        assert!(fc[0].assignment_prob > 0.0 && fc[0].assignment_prob < 0.10);
        assert!(fc[0].downside_round_yield < 0.0);
        assert!(fc[1].confidence < fc[0].confidence);
    }

    #[test]
    fn hourly_round_stays_within_cap() {
        // The reported bug: an hourly vault must not annualize to 1600%.
        let i = VaultInputs {
            aum_underlying: 0.0,
            round_ms: HOUR,
            ..inputs()
        };
        let fc = predict(&i).into_iter().find(|p| p.kind == "forecast").unwrap();
        assert!(fc.apy.is_finite() && fc.apy.abs() <= 5.0, "apy {}", fc.apy);
        assert!(fc.apy_high <= 5.0);
    }

    // ── Unit tests for the private math primitives ──

    #[test]
    fn net_of_fees_charges_perf_on_profit_only() {
        // Zero gross profit → only the management drag remains.
        assert!((net_of_fees(0.0, 0.10, 0.02) - -0.02).abs() < 1e-12);
        // Profit: perf fee on the gross, then mgmt.
        assert!((net_of_fees(1.0, 0.10, 0.0) - 0.9).abs() < 1e-12);
        // A loss pays NO performance fee (only profit is charged).
        assert!((net_of_fees(-0.5, 0.10, 0.0) - -0.5).abs() < 1e-12);
        assert!((net_of_fees(-0.5, 0.10, 0.02) - -0.52).abs() < 1e-12);
    }

    #[test]
    fn annualize_is_simple_and_capped() {
        // Linear: y × periods, no compounding.
        let wk = annualize(0.002, WEEK, 5.0).unwrap();
        assert!((wk - 0.002 * periods(WEEK)).abs() < 1e-12);
        assert!((wk - 0.10436).abs() < 1e-4);
        // Hourly 0.001/round = 8.766 raw → clamped to +5.0 (the anti-1600% cap).
        assert_eq!(annualize(0.001, HOUR, 5.0), Some(5.0));
        // Negative side clamps symmetrically.
        assert_eq!(annualize(-0.001, HOUR, 5.0), Some(-5.0));
        // Degenerate round length.
        assert_eq!(annualize(0.01, 0, 5.0), None);
    }

    #[test]
    fn assignment_identity_and_conditional() {
        let (spot, sigma, t) = (50_000.0, 0.6, WEEK as f64 / YEAR_MS);
        let k = strike_for_delta(spot, sigma, t, 0.0, 0.10);
        let a = assignment(spot, k, t, sigma);
        // Expected assignment cost IS the undiscounted call payoff per unit
        // (r = 0), i.e. call_price / spot — the no-free-lunch identity.
        let call = call_price_per_unit(CallInputs { spot, strike: k, t_years: t, r: 0.0, sigma });
        assert!((a.expected - call / spot).abs() < 1e-12);
        // Conditional loss = expected / P(assigned), and is strictly worse than
        // the unconditional expectation.
        assert!((a.conditional - a.expected / a.prob).abs() < 1e-12);
        assert!(a.conditional > a.expected);
        // 0.10-delta strike → assignment prob N(d2) < 0.10.
        assert!(a.prob > 0.0 && a.prob < 0.10);
    }

    #[test]
    fn assignment_more_vol_means_more_drag() {
        let (spot, t) = (50_000.0, WEEK as f64 / YEAR_MS);
        let k = strike_for_delta(spot, 0.6, t, 0.0, 0.10);
        // Hold the strike fixed; raising vol raises both the assignment cost and
        // the probability of finishing ITM.
        let lo = assignment(spot, k, t, 0.45);
        let hi = assignment(spot, k, t, 0.75);
        assert!(hi.expected > lo.expected);
        assert!(hi.prob > lo.prob);
    }

    // ── End-to-end: predict() locked to an independent recompute ──

    #[test]
    fn tier2_net_matches_independent_recompute() {
        let i = VaultInputs { aum_underlying: 0.0, ..inputs() };
        let t = i.round_ms as f64 / YEAR_MS;
        let strike = strike_for_delta(i.spot, i.sigma, t, 0.0, i.delta_target);
        // Premium at implied (realized + VRP), assignment at realized.
        let premium = call_price_per_unit(CallInputs {
            spot: i.spot, strike, t_years: t, r: 0.0, sigma: i.sigma + i.assumed_vrp,
        }) / i.spot;
        let assign = call_price_per_unit(CallInputs {
            spot: i.spot, strike, t_years: t, r: 0.0, sigma: i.sigma,
        }) / i.spot;
        let mgmt_round = i.mgmt_fee_annual * t;
        let gross = premium - assign;
        let net_round = gross - i.perf_fee * gross.max(0.0) - mgmt_round;
        let want = (net_round * periods(i.round_ms)).clamp(-i.apy_cap, i.apy_cap);
        let fc = predict(&i).into_iter().find(|p| p.kind == "forecast").unwrap();
        assert!((fc.apy - want).abs() < 1e-12, "{} vs {}", fc.apy, want);
    }

    #[test]
    fn tier1_net_matches_independent_recompute() {
        let strike = strike_for_delta(50_000.0, 0.6, WEEK as f64 / YEAR_MS, 0.0, 0.10);
        let i = VaultInputs {
            sigma: 0.0, // disable Tier 2 so only the 'current' point is emitted
            current_premium_underlying: 0.4,
            current_strike: Some(strike),
            ..inputs()
        };
        // sigma 0 → no assignment modeled, so this is the premium-only branch.
        let t = i.round_ms as f64 / YEAR_MS;
        let premium_yield = i.current_premium_underlying / i.aum_underlying;
        let mgmt_round = i.mgmt_fee_annual * t;
        let net_round = net_of_fees(premium_yield, i.perf_fee, mgmt_round);
        let want = (net_round * periods(i.round_ms)).clamp(-i.apy_cap, i.apy_cap);
        let cur = predict(&i).into_iter().find(|p| p.kind == "current").unwrap();
        assert!((cur.apy - want).abs() < 1e-12, "{} vs {}", cur.apy, want);
    }

    #[test]
    fn tier2_apy_increases_with_vrp() {
        let apy = |vrp: f64| {
            let i = VaultInputs { aum_underlying: 0.0, assumed_vrp: vrp, vrp_band: 0.0, ..inputs() };
            predict(&i).into_iter().find(|p| p.kind == "forecast").unwrap().apy
        };
        assert!(apy(0.0) < apy(0.04));
        assert!(apy(0.04) < apy(0.08));
        // No variance risk premium → net edge is exactly −mgmt_fee_annual.
        assert!((apy(0.0) - -0.02).abs() < 1e-6);
    }

    #[test]
    fn downside_is_worse_than_central_round() {
        let i = VaultInputs { aum_underlying: 0.0, ..inputs() };
        let fc = predict(&i).into_iter().find(|p| p.kind == "forecast").unwrap();
        // De-annualize the (uncapped, weekly) central APY to a round yield.
        let central_round = fc.apy / periods(i.round_ms);
        assert!(fc.downside_round_yield < central_round);
        assert!(fc.downside_round_yield < 0.0);
    }

    #[test]
    fn hourly_and_weekly_both_within_cap() {
        let apy = |round_ms: u64| {
            let i = VaultInputs { aum_underlying: 0.0, round_ms, ..inputs() };
            predict(&i).into_iter().find(|p| p.kind == "forecast").unwrap().apy
        };
        let (weekly, hourly) = (apy(WEEK), apy(HOUR));
        // Short-dated vol selling genuinely earns more theta per unit time
        // (~√168×), but it must stay finite and bounded — never the old 1600%.
        assert!(weekly.is_finite() && weekly > 0.0);
        assert!(hourly.is_finite() && hourly > 0.0 && hourly <= 5.0);
        assert!(hourly > weekly);
    }

    #[test]
    fn median_round_ms_from_finalizes() {
        assert_eq!(median_round_ms(vec![]), None);
        assert_eq!(median_round_ms(vec![0]), None);
        assert_eq!(median_round_ms(vec![0, WEEK, 2 * WEEK]), Some(WEEK));
    }
}
