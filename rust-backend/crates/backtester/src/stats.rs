//! Per-run counters the flow generator and solver need (doc 08 §8.1
//! volume definitions, §8.6 outputs, §8.7 per-result counts). Never a
//! bare "volume": every result carries all six definitions.

use serde::Serialize;

/// The six volume definitions of doc 08 §8.1, settlement units.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Volumes {
    /// Underlying spot notional submitted by writers.
    pub offered_earn_notional: f64,
    /// Offered notional that passed eligibility and received a bid.
    pub quoted_earn_notional: f64,
    /// Notional actually bought by the vault (filled).
    pub accepted_earn_notional: f64,
    /// Settlement premium paid to writers (gross; the writer nets the
    /// protocol fee wedge).
    pub premium_turnover: f64,
    /// Absolute perp notional traded, rebalances included.
    pub hedge_turnover: f64,
    /// Underlying bought or sold during exercise.
    pub exercise_spot_turnover: f64,
}

/// Declined notional by reason.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Declined {
    pub capacity: f64,
    pub priced_zero: f64,
    pub stale: f64,
    pub count_capacity: u64,
    pub count_priced_zero: u64,
    pub count_stale: u64,
    /// Which premium cap tripped (a decline may trip several).
    pub count_total_cap: u64,
    pub count_call_cap: u64,
    pub count_put_cap: u64,
    pub count_expiry_cap: u64,
}

/// A 24-hour trailing-minimum tracker (monotone deque) for the
/// "maximum required margin top-up in 24h" line.
#[derive(Clone, Debug, Default)]
pub struct TrailingMin {
    window_ms: i64,
    deque: std::collections::VecDeque<(i64, f64)>,
}

impl TrailingMin {
    pub fn new(window_ms: i64) -> Self {
        Self { window_ms, deque: Default::default() }
    }

    /// Push a sample and return `value - min over the trailing window`.
    pub fn push(&mut self, ts_ms: i64, value: f64) -> f64 {
        while self.deque.back().is_some_and(|(_, v)| *v >= value) {
            self.deque.pop_back();
        }
        self.deque.push_back((ts_ms, value));
        while self.deque.front().is_some_and(|(t, _)| ts_ms - *t > self.window_ms) {
            self.deque.pop_front();
        }
        value - self.deque.front().map(|(_, v)| *v).unwrap_or(value)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RunStats {
    pub volumes: Volumes,
    pub declined: Declined,
    pub rfqs_offered: u64,
    pub rfqs_call: u64,
    pub rfqs_put: u64,
    pub quotes_sent: u64,
    pub quotes_accepted: u64,
    pub quotes_expired: u64,
    pub quotes_reverted: u64,
    pub fills_call: u64,
    pub fills_put: u64,
    pub expiries_settled: u64,
    pub exercised_call: u64,
    pub exercised_put: u64,
    /// Flash/venue-capacity assumptions (labeled until PR M lands).
    pub exercise_laddered: u64,
    pub exercise_failed: u64,
    pub flash_cap_hits: u64,
    pub venue_cap_hits: u64,
    /// Premium reserved by live quotes: peak and time-average.
    pub peak_reserved: f64,
    pub avg_reserved: f64,
    /// Premium at risk = option marks + live reservations.
    pub peak_premium_at_risk_total: f64,
    pub peak_premium_at_risk_call: f64,
    pub peak_premium_at_risk_put: f64,
    pub peak_expiry_premium_at_risk: f64,
    /// Free settlement = cash − reservations; the cash gate.
    pub min_free_settlement: f64,
    pub initial_hedge_margin: f64,
    pub peak_hedge_margin: f64,
    pub peak_24h_margin_topup: f64,
    pub min_margin_headroom: f64,
    pub liquidations: u64,
    /// Peak of marks + reservations + hedge margin.
    pub peak_capital_deployed: f64,
    pub resales: u64,
    pub resale_pnl: f64,
    /// Mean displayed writer-net APY over quotes sent, by type.
    pub displayed_apy_call_mean: Option<f64>,
    pub displayed_apy_put_mean: Option<f64>,
    #[serde(skip)]
    pub reserved_sum: f64,
    #[serde(skip)]
    pub reserved_samples: u64,
    #[serde(skip)]
    pub apy_n: [u64; 2],
}

impl Default for RunStats {
    fn default() -> Self {
        Self {
            volumes: Volumes::default(),
            declined: Declined::default(),
            rfqs_offered: 0,
            rfqs_call: 0,
            rfqs_put: 0,
            quotes_sent: 0,
            quotes_accepted: 0,
            quotes_expired: 0,
            quotes_reverted: 0,
            fills_call: 0,
            fills_put: 0,
            expiries_settled: 0,
            exercised_call: 0,
            exercised_put: 0,
            exercise_laddered: 0,
            exercise_failed: 0,
            flash_cap_hits: 0,
            venue_cap_hits: 0,
            peak_reserved: 0.0,
            avg_reserved: 0.0,
            peak_premium_at_risk_total: 0.0,
            peak_premium_at_risk_call: 0.0,
            peak_premium_at_risk_put: 0.0,
            peak_expiry_premium_at_risk: 0.0,
            min_free_settlement: f64::INFINITY,
            initial_hedge_margin: 0.0,
            peak_hedge_margin: 0.0,
            peak_24h_margin_topup: 0.0,
            min_margin_headroom: f64::INFINITY,
            liquidations: 0,
            peak_capital_deployed: 0.0,
            resales: 0,
            resale_pnl: 0.0,
            displayed_apy_call_mean: None,
            displayed_apy_put_mean: None,
            reserved_sum: 0.0,
            reserved_samples: 0,
            apy_n: [0, 0],
        }
    }
}

impl RunStats {
    pub fn sample_reserved(&mut self, reserved: f64) {
        self.reserved_sum += reserved;
        self.reserved_samples += 1;
        self.peak_reserved = self.peak_reserved.max(reserved);
        self.avg_reserved = self.reserved_sum / self.reserved_samples as f64;
    }

    pub fn sample_apy(&mut self, is_put: bool, apy: f64) {
        let slot = is_put as usize;
        self.apy_n[slot] += 1;
        let n = self.apy_n[slot] as f64;
        let mean = if is_put { &mut self.displayed_apy_put_mean } else { &mut self.displayed_apy_call_mean };
        let m = mean.unwrap_or(0.0);
        *mean = Some(m + (apy - m) / n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_min_tracks_topup_over_the_window() {
        let mut t = TrailingMin::new(100);
        assert_eq!(t.push(0, 10.0), 0.0);
        assert_eq!(t.push(50, 5.0), 0.0);
        assert_eq!(t.push(90, 12.0), 7.0);
        // The 10 at t=0 has fallen out; min is 5 at t=50.
        assert_eq!(t.push(160, 20.0), 8.0);
        assert_eq!(t.push(200, 20.0), 0.0);
    }
}
