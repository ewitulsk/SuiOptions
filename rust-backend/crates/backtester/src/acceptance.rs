//! Quote lifecycle and acceptance hazard (doc 08 §7.1, §8.4):
//!
//! ```text
//! RFQ arrival → response latency → quote sent, premium reserved
//!   → acceptance hazard over the remaining TTL
//!   → chain inclusion or revert → fill detection or expiry
//! ```
//!
//! Acceptance is a hazard over the quote's life, not a one-shot draw:
//!
//! ```text
//! h(t) = h₀ · shape(t/TTL)
//!        · (APY / APY_ref)^ε                       displayed writer-net APY (elasticity)
//!        · exp(β_stale · (fair_at_quote − fair_now) / fair_at_quote)   selection into stale quotes
//!        · exp(β_size · ln(notional / size_ref))   large writers deliberate longer
//!        · exp(β_money · |z|)                      moneyness
//! ```
//!
//! with `h₀` set so a reference quote is accepted with probability
//! `accept_prob_at_ref` over a full TTL. The per-RFQ threshold
//! `−ln u` is a common random number keyed on the RFQ, so a variant
//! that widens the bid lowers the hazard and the same writer accepts
//! later or never — wider bids reduce acceptance, favorable stale quotes
//! are accepted more often, unfavorable ones expire more often.
//!
//! Every parameter is a stated prior (doc 08 §3.1). `mode = "instant"`
//! (capacity mode / v0 parity) fills every quote on arrival.

use serde::Serialize;

use crate::flow_gen::RfqEvent;
use crate::rng::Pcg32;
use crate::scenario::AcceptanceConfig;

const TAG_ACCEPT: u64 = 0x61;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub enum Outcome {
    /// Accepted and included on chain at this time.
    Filled(i64),
    Expired,
    Reverted,
}

/// A live quote: premium is reserved from `sent_ms` until the terminal
/// event.
#[derive(Clone, Debug, Serialize)]
pub struct LiveQuote {
    pub rfq: RfqEvent,
    pub sent_ms: i64,
    pub valid_until_ms: i64,
    /// Gross premium the desk pays (the reservation).
    pub bid: f64,
    /// Writer-net after the protocol fee wedge (the displayed base).
    pub writer_net: f64,
    pub displayed_apy: f64,
    pub fair_at_quote: f64,
    pub sigma_quote: f64,
    pub sigma_paid: f64,
    pub delta: f64,
    pub gamma: f64,
    pub vega: f64,
    pub moneyness_z: f64,
    pub last_step_ms: i64,
    pub cum_hazard: f64,
    pub threshold: f64,
    pub u_revert: f64,
    pub accepted_ms: Option<i64>,
}

/// Writer-net premium APY as the Earn page displays it: net premium
/// over the collateral posted (underlying for a call, strike cash for a
/// put), annualized over the tenor.
pub fn displayed_apy(is_put: bool, writer_net_premium: f64, qty: f64, spot: f64, strike: f64, tenor_years: f64) -> f64 {
    let collateral = if is_put { qty * strike } else { qty * spot };
    if collateral <= 0.0 || tenor_years <= 0.0 {
        return 0.0;
    }
    writer_net_premium / collateral / tenor_years
}

pub struct AcceptanceModel {
    cfg: AcceptanceConfig,
    seed: u64,
}

impl AcceptanceModel {
    pub fn new(cfg: AcceptanceConfig, seed: u64) -> Self {
        Self { cfg, seed }
    }

    pub fn is_instant(&self) -> bool {
        self.cfg.mode == "instant"
    }

    pub fn label(&self) -> &'static str {
        if self.is_instant() { "instant" } else { "hazard_ttl" }
    }

    pub fn config(&self) -> &AcceptanceConfig {
        &self.cfg
    }

    /// Open a quote for `rfq` at `now`: sent after the response latency,
    /// live for the TTL, threshold and revert uniform keyed on the RFQ.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        &self,
        rfq: RfqEvent,
        now_ms: i64,
        bid: f64,
        writer_net: f64,
        spot: f64,
        fair_at_quote: f64,
        sigma_quote: f64,
        sigma_paid: f64,
        greeks: (f64, f64, f64),
    ) -> LiveQuote {
        let mut rng = Pcg32::keyed(self.seed, &[rfq.key.minute as u64, rfq.key.is_put as u64, rfq.key.k as u64, TAG_ACCEPT]);
        let u = rng.uniform();
        let u_revert = rng.uniform();
        let tenor_years = ((rfq.expiry_ms - now_ms) as f64 / crate::MS_PER_YEAR_F).max(1e-9);
        let sent_ms = now_ms + self.cfg.response_latency_ms;
        let z = (rfq.strike / spot).ln() / (sigma_quote.max(1e-6) * tenor_years.sqrt());
        LiveQuote {
            rfq,
            sent_ms,
            valid_until_ms: sent_ms + self.cfg.ttl_ms,
            bid,
            writer_net,
            displayed_apy: displayed_apy(rfq.is_put, writer_net, rfq.qty, spot, rfq.strike, tenor_years),
            fair_at_quote,
            sigma_quote,
            sigma_paid,
            delta: greeks.0,
            gamma: greeks.1,
            vega: greeks.2,
            moneyness_z: z,
            last_step_ms: sent_ms,
            cum_hazard: 0.0,
            threshold: -u.ln(),
            u_revert,
            accepted_ms: None,
        }
    }

    /// Hazard per millisecond at `frac` of the TTL elapsed, given the
    /// option's current fair value.
    pub fn hazard_per_ms(&self, q: &LiveQuote, frac: f64, current_fair: f64) -> f64 {
        let t = if q.rfq.is_put { &self.cfg.put } else { &self.cfg.call };
        let p_ref = t.accept_prob_at_ref.clamp(1e-6, 1.0 - 1e-6);
        let h0 = -(1.0 - p_ref).ln() / self.cfg.ttl_ms.max(1) as f64;
        let fl = self.cfg.front_load;
        let shape = if fl.abs() < 1e-9 { 1.0 } else { fl * (-fl * frac.clamp(0.0, 1.0)).exp() / (1.0 - (-fl).exp()) };
        let apy_mult = (q.displayed_apy.max(1e-4) / t.apy_ref.max(1e-4)).powf(t.apy_elasticity);
        let edge = if q.fair_at_quote > 0.0 { ((q.fair_at_quote - current_fair) / q.fair_at_quote).clamp(-1.0, 1.0) } else { 0.0 };
        let stale = (self.cfg.stale_edge_coef * edge).exp();
        let size = (self.cfg.size_coef * (q.rfq.offered_notional / self.cfg.size_ref_notional.max(1e-9)).max(1e-9).ln()).exp();
        let money = (self.cfg.moneyness_coef * q.moneyness_z.abs()).exp();
        h0 * shape * apy_mult * stale * size * money
    }

    /// Advance the quote to `now`. `current_fair` is the option's value
    /// at the current decision price (same size as `bid`).
    pub fn step(&self, q: &mut LiveQuote, now_ms: i64, current_fair: f64) -> Option<Outcome> {
        if self.is_instant() {
            return Some(Outcome::Filled(now_ms));
        }
        if let Some(acc) = q.accepted_ms {
            return self.resolve_inclusion(q, acc, now_ms);
        }
        let from = q.last_step_ms.max(q.sent_ms);
        let to = now_ms.min(q.valid_until_ms);
        if to > from {
            let mid = (from + to) / 2 - q.sent_ms;
            let frac = mid as f64 / self.cfg.ttl_ms.max(1) as f64;
            q.cum_hazard += self.hazard_per_ms(q, frac, current_fair) * (to - from) as f64;
            q.last_step_ms = to;
        }
        if q.cum_hazard >= q.threshold {
            q.accepted_ms = Some(now_ms);
            return self.resolve_inclusion(q, now_ms, now_ms);
        }
        if now_ms >= q.valid_until_ms {
            return Some(Outcome::Expired);
        }
        None
    }

    fn resolve_inclusion(&self, q: &LiveQuote, accepted_ms: i64, now_ms: i64) -> Option<Outcome> {
        if now_ms < accepted_ms + self.cfg.inclusion_latency_ms {
            return None;
        }
        if q.u_revert < self.cfg.revert_prob {
            Some(Outcome::Reverted)
        } else {
            Some(Outcome::Filled(now_ms))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow_gen::RfqKey;

    fn rfq(k: u32, is_put: bool) -> RfqEvent {
        RfqEvent {
            key: RfqKey { minute: 10, is_put, k },
            arrival_ms: 600_000,
            is_put,
            strike: 3.0,
            expiry_ms: 600_000 + 14 * crate::MS_PER_DAY,
            qty: 1_000.0,
            offered_notional: 3_000.0,
            alt_yield: 0.03,
        }
    }

    fn cfg() -> AcceptanceConfig {
        AcceptanceConfig { mode: "hazard".into(), inclusion_latency_ms: 0, revert_prob: 0.0, ..Default::default() }
    }

    /// Run `n` quotes through a full TTL with the fair value moving by
    /// `fair_drift` (fraction of the quoted fair) and return the accepted
    /// fraction.
    fn accepted_fraction(m: &AcceptanceModel, n: u32, bid_frac: f64, fair_drift: f64) -> f64 {
        let mut accepted = 0;
        for k in 0..n {
            let r = rfq(k, false);
            let fair = 190.0;
            let bid = fair * bid_frac;
            let mut q = m.open(r, r.arrival_ms, bid, bid, 3.0, fair, 0.8, 0.75, (0.5, 0.1, 1.0));
            let mut now = r.arrival_ms;
            loop {
                now += 60_000;
                match m.step(&mut q, now, fair * (1.0 + fair_drift)) {
                    Some(Outcome::Filled(_)) => {
                        accepted += 1;
                        break;
                    }
                    Some(_) => break,
                    None => {}
                }
            }
        }
        accepted as f64 / n as f64
    }

    #[test]
    fn wider_bids_reduce_acceptance() {
        let m = AcceptanceModel::new(cfg(), 11);
        let tight = accepted_fraction(&m, 400, 0.95, 0.0);
        let wide = accepted_fraction(&m, 400, 0.70, 0.0);
        assert!(tight > wide + 0.1, "tight {tight} wide {wide}");
    }

    #[test]
    fn favorable_stale_quotes_accepted_more_and_unfavorable_expire_more() {
        let m = AcceptanceModel::new(cfg(), 12);
        let flat = accepted_fraction(&m, 400, 0.9, 0.0);
        // Option worth less now than when quoted: the writer sells rich.
        let favorable = accepted_fraction(&m, 400, 0.9, -0.15);
        // Option worth more now: the writer walks and the quote expires.
        let unfavorable = accepted_fraction(&m, 400, 0.9, 0.15);
        assert!(favorable > flat + 0.05, "favorable {favorable} flat {flat}");
        assert!(unfavorable < flat - 0.05, "unfavorable {unfavorable} flat {flat}");
    }

    #[test]
    fn reference_quote_accepts_at_the_stated_prior_and_same_seed_is_identical() {
        let c = cfg();
        let m = AcceptanceModel::new(c.clone(), 5);
        // A quote at exactly apy_ref, no drift: p ≈ accept_prob_at_ref.
        let r = rfq(0, false);
        let tenor = 14.0 / 365.0;
        let net = c.call.apy_ref * r.qty * 3.0 * tenor;
        let mut hits = 0;
        for k in 0..600 {
            let mut rr = r;
            rr.key.k = k;
            let mut q = m.open(rr, rr.arrival_ms, net, net, 3.0, net, 0.8, 0.75, (0.5, 0.0, 0.0));
            let mut now = rr.arrival_ms;
            loop {
                now += 60_000;
                match m.step(&mut q, now, net) {
                    Some(Outcome::Filled(_)) => {
                        hits += 1;
                        break;
                    }
                    Some(_) => break,
                    None => {}
                }
            }
        }
        let p = hits as f64 / 600.0;
        assert!((p - c.call.accept_prob_at_ref).abs() < 0.07, "{p} vs {}", c.call.accept_prob_at_ref);
        let a = m.open(r, r.arrival_ms, 100.0, 100.0, 3.0, 110.0, 0.8, 0.75, (0.5, 0.0, 0.0));
        let b = AcceptanceModel::new(c, 5).open(r, r.arrival_ms, 100.0, 100.0, 3.0, 110.0, 0.8, 0.75, (0.5, 0.0, 0.0));
        assert_eq!(a.threshold, b.threshold);
        assert_eq!(a.u_revert, b.u_revert);
    }

    #[test]
    fn premium_stays_reserved_until_the_terminal_event() {
        let c = AcceptanceConfig { mode: "hazard".into(), inclusion_latency_ms: 120_000, revert_prob: 1.0, ..Default::default() };
        let m = AcceptanceModel::new(c.clone(), 3);
        let r = rfq(0, true);
        let mut q = m.open(r, r.arrival_ms, 100.0, 100.0, 3.0, 100.0, 0.8, 0.75, (-0.5, 0.0, 0.0));
        q.threshold = 0.0; // accept at the first step
        let t1 = r.arrival_ms + 60_000;
        assert_eq!(m.step(&mut q, t1, 100.0), None, "accepted but not yet included");
        assert!(q.accepted_ms.is_some());
        assert_eq!(m.step(&mut q, t1 + 60_000, 100.0), None);
        assert_eq!(m.step(&mut q, t1 + 120_000, 100.0), Some(Outcome::Reverted));
        // Instant mode fills on the first step.
        let inst = AcceptanceModel::new(AcceptanceConfig::default(), 3);
        let mut q2 = inst.open(r, r.arrival_ms, 100.0, 100.0, 3.0, 100.0, 0.8, 0.75, (-0.5, 0.0, 0.0));
        assert_eq!(inst.step(&mut q2, r.arrival_ms, 100.0), Some(Outcome::Filled(r.arrival_ms)));
        assert!((displayed_apy(true, 100.0, 1_000.0, 3.0, 2.5, 0.5) - 0.08).abs() < 1e-12);
    }
}
