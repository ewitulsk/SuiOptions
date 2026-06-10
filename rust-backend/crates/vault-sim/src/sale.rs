//! Sale mechanisms (doc 06 §4): what fraction of mid the vault actually
//! realizes when it sells a slice, under different venue assumptions.

use rand::Rng;
use rand_chacha::rand_core::SeedableRng;
use rand_chacha::ChaCha8Rng;

use crate::premium::QuoteCtx;

/// Outcome of offering one slice for sale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fill {
    /// Executed premium per underlying smallest-unit (settlement
    /// smallest-units). Meaningless when `filled == 0`.
    pub premium_per_unit: f64,
    /// Underlying smallest-units actually sold (≤ the offered slice).
    pub filled: u64,
}

impl Fill {
    pub fn unsold() -> Self {
        Self { premium_per_unit: 0.0, filled: 0 }
    }
}

pub trait SaleMechanism {
    /// Premium realized for a slice of `slice` units given the model mid.
    fn execute(&mut self, slice: u64, mid: f64, ctx: &QuoteCtx) -> Fill;
}

/// Competitive venue: fills the whole slice at mid × (1 − haircut). Models
/// the signed-quote RFQ and a deep on-chain auction equally.
pub struct RfqBatch {
    pub haircut_bps: u64,
}

impl SaleMechanism for RfqBatch {
    fn execute(&mut self, slice: u64, mid: f64, _ctx: &QuoteCtx) -> Fill {
        Fill {
            premium_per_unit: mid * (1.0 - self.haircut_bps as f64 / 10_000.0),
            filled: slice,
        }
    }
}

/// The doc 02 on-chain auction model: each arriving bidder's max bid is
/// mid × (1 − markdown_i); the auction clears at the second-highest max
/// plus one increment (capped at the winner's max), floored at the
/// vault's reserve. No bidders ⇒ unsold slice; a lone bidder pays the
/// reserve (the griefing-table worst case).
pub struct OnchainAuction {
    /// Mean bidder count (Poisson).
    pub n_bidders_mean: f64,
    /// Mean bid markdown vs mid, bps; per-bidder draw is U(0, 2×mean).
    pub markdown_bps: u64,
    /// Reserve as bps of spot notional (doc 03 §6's floor).
    pub reserve_bps: u64,
    /// Chance an arriving bidder doesn't show (RFQ ignored).
    pub no_show_prob: f64,
    /// Auction min-increment over the standing best, bps.
    pub min_increment_bps: u64,
    rng: ChaCha8Rng,
}

impl OnchainAuction {
    pub fn new(
        n_bidders_mean: f64,
        markdown_bps: u64,
        reserve_bps: u64,
        no_show_prob: f64,
        min_increment_bps: u64,
        seed: u64,
    ) -> Self {
        Self {
            n_bidders_mean,
            markdown_bps,
            reserve_bps,
            no_show_prob,
            min_increment_bps,
            rng: ChaCha8Rng::seed_from_u64(seed),
        }
    }

    fn poisson(&mut self, lambda: f64) -> u32 {
        // Knuth's method; lambda is small (single-digit bidder counts).
        let l = (-lambda).exp();
        let mut k = 0u32;
        let mut p = 1.0;
        loop {
            p *= self.rng.gen_range(0.0..1.0);
            if p <= l {
                return k;
            }
            k += 1;
        }
    }
}

impl SaleMechanism for OnchainAuction {
    fn execute(&mut self, slice: u64, mid: f64, ctx: &QuoteCtx) -> Fill {
        let reserve_per_unit = ctx.spot * self.reserve_bps as f64 / 10_000.0;

        let arrivals = self.poisson(self.n_bidders_mean);
        let mut maxes: Vec<f64> = Vec::with_capacity(arrivals as usize);
        for _ in 0..arrivals {
            if self.rng.gen_bool(self.no_show_prob) {
                continue;
            }
            let markdown = self.rng.gen_range(0.0..(2.0 * self.markdown_bps as f64).max(f64::MIN_POSITIVE));
            let max = mid * (1.0 - markdown / 10_000.0);
            // A bidder whose max is below the reserve never bids.
            if max >= reserve_per_unit {
                maxes.push(max);
            }
        }

        if maxes.is_empty() {
            return Fill::unsold();
        }
        maxes.sort_by(|a, b| b.partial_cmp(a).unwrap());
        let clearing = if maxes.len() == 1 {
            reserve_per_unit
        } else {
            let second_plus_inc =
                maxes[1] * (1.0 + self.min_increment_bps as f64 / 10_000.0);
            second_plus_inc.min(maxes[0]).max(reserve_per_unit)
        };
        Fill { premium_per_unit: clearing, filled: slice }
    }
}

/// Stress scenario: thin venue that sometimes ignores the auction entirely
/// and demands a deep discount when it does show.
pub struct Pessimist {
    pub fill_prob: f64,
    pub haircut_bps: u64,
    rng: ChaCha8Rng,
}

impl Pessimist {
    pub fn new(fill_prob: f64, haircut_bps: u64, seed: u64) -> Self {
        Self { fill_prob, haircut_bps, rng: ChaCha8Rng::seed_from_u64(seed) }
    }
}

impl SaleMechanism for Pessimist {
    fn execute(&mut self, slice: u64, mid: f64, _ctx: &QuoteCtx) -> Fill {
        if !self.rng.gen_bool(self.fill_prob) {
            return Fill::unsold();
        }
        Fill {
            premium_per_unit: mid * (1.0 - self.haircut_bps as f64 / 10_000.0),
            filled: slice,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> QuoteCtx {
        QuoteCtx { spot: 100.0, strike: 130.0, tau_years: 7.0 / 365.0, r: 0.0, sigma: 0.6 }
    }

    #[test]
    fn rfq_batch_fills_at_haircut_mid() {
        let f = RfqBatch { haircut_bps: 500 }.execute(1_000, 2.0, &ctx());
        assert_eq!(f.filled, 1_000);
        assert!((f.premium_per_unit - 1.9).abs() < 1e-12);
    }

    #[test]
    fn rfq_batch_zero_haircut_is_mid() {
        let f = RfqBatch { haircut_bps: 0 }.execute(1, 2.0, &ctx());
        assert_eq!(f.premium_per_unit, 2.0);
    }

    #[test]
    fn auction_no_bidders_means_unsold() {
        let mut a = OnchainAuction::new(0.0, 300, 10, 0.0, 50, 1);
        assert_eq!(a.execute(1_000, 2.0, &ctx()), Fill::unsold());
    }

    #[test]
    fn auction_lone_bidder_pays_reserve() {
        // Huge lambda + certain no-shows except... force exactly one
        // bidder by using lambda so small that ≥2 is vanishingly unlikely
        // across draws — instead, deterministically: mean 1e-9 won't ever
        // arrive. Use a direct construction: n=5 arrivals, no_show 0, but
        // mid below everyone's reserve except... Simplest determinism:
        // markdown 0 (all maxes == mid) with min_increment 0 clears at
        // second-highest == mid; lone-bidder needs arrivals == 1, so loop
        // seeds until Poisson(1.0) gives exactly 1 arrival.
        for seed in 0..100 {
            let mut a = OnchainAuction::new(1.0, 0, 10, 0.0, 0, seed);
            let f = a.execute(1_000, 2.0, &ctx());
            if f.filled > 0 && f.premium_per_unit < 2.0 - 1e-9 {
                // A clearing below mid with markdown 0 can only be the
                // lone-bidder reserve case.
                let reserve = 100.0 * 10.0 / 10_000.0; // 0.1
                assert!((f.premium_per_unit - reserve).abs() < 1e-12);
                return;
            }
        }
        panic!("no lone-bidder draw in 100 seeds");
    }

    #[test]
    fn auction_competition_clears_near_mid() {
        // Many bidders, zero markdown: second-highest max == mid, clearing
        // == mid (increment capped at the winner's max).
        let mut a = OnchainAuction::new(20.0, 0, 10, 0.0, 50, 3);
        let f = a.execute(1_000, 2.0, &ctx());
        assert_eq!(f.filled, 1_000);
        assert!((f.premium_per_unit - 2.0).abs() < 1e-12);
    }

    #[test]
    fn auction_respects_reserve_filter() {
        // Mid below the reserve floor: every bidder's max < reserve, so
        // the slice never sells — the reserve is the price-safety floor.
        let mut a = OnchainAuction::new(10.0, 0, 10_000, 0.0, 50, 4);
        // reserve = spot × 100% = 100; mid = 2.
        assert_eq!(a.execute(1_000, 2.0, &ctx()), Fill::unsold());
    }

    #[test]
    fn pessimist_is_bernoulli() {
        let mut p = Pessimist::new(0.5, 1000, 9);
        let fills: Vec<bool> =
            (0..200).map(|_| p.execute(100, 2.0, &ctx()).filled > 0).collect();
        let rate = fills.iter().filter(|x| **x).count() as f64 / 200.0;
        assert!((rate - 0.5).abs() < 0.15, "fill rate {rate}");
        // When it fills, the haircut applies.
        let mut p = Pessimist::new(1.0, 1000, 9);
        let f = p.execute(100, 2.0, &ctx());
        assert!((f.premium_per_unit - 1.8).abs() < 1e-12);
    }
}
