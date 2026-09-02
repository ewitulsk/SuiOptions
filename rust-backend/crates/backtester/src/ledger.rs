//! The v0 ledger: cash, open options (marked at model), the signed perp
//! (average-entry accounting, like the desk's paper venue), and the
//! attribution lines. NAV = cash + option marks + perp unrealized, every
//! minute. Exact-ledger invariants are doc 08 PR J; this is the simple
//! ledger doc 09 G4 asks for, and it is reconciled in tests.

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct Position {
    pub id: u64,
    pub is_put: bool,
    pub strike: f64,
    pub expiry_ms: i64,
    pub qty: f64,
    pub premium_paid: f64,
    /// Sigma the bid was struck at (after the vol discount).
    pub sigma_paid: f64,
    /// Surface (pre-discount) sigma at entry.
    pub sigma_surface: f64,
    pub opened_ms: i64,
    pub spot_open: f64,
    pub delta_open: f64,
    pub gamma_open: f64,
    pub vega_open: f64,
    /// Writer-side net premium after the protocol fee (the displayed APY
    /// base) — doc 09 G7.
    pub writer_net_premium: f64,
    pub mark: f64,
    /// Units at entry (exercise slices reduce `qty`).
    pub qty_open: f64,
    /// Units whose exercise PTB is in flight (doc 08 §7.5/§7.6).
    pub pending_units: f64,
    /// Last exits-cadence check (the daily check; the near-expiry sweep
    /// ignores it).
    pub last_check_ms: i64,
    /// Settlement realized by exercise slices so far.
    pub exercise_net: f64,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Perp {
    /// Signed units, long > 0.
    pub position: f64,
    pub avg_entry: f64,
    pub realized: f64,
    /// Cash assigned to the venue as isolated margin (doc 08 §7.3):
    /// part of NAV, not of `cash`.
    pub collateral: f64,
}

impl Perp {
    /// Apply a signed fill at `px`; returns realized P&L on any closed slice.
    pub fn fill(&mut self, units: f64, px: f64) -> f64 {
        if units == 0.0 {
            return 0.0;
        }
        let pos = self.position;
        let mut realized = 0.0;
        if pos == 0.0 || pos.signum() == units.signum() {
            let new = pos + units;
            self.avg_entry = (self.avg_entry * pos.abs() + px * units.abs()) / new.abs();
            self.position = new;
        } else {
            let close = units.abs().min(pos.abs());
            realized = (px - self.avg_entry) * close * pos.signum();
            let new = pos + units;
            if new.abs() <= 1e-12 {
                self.position = 0.0;
                self.avg_entry = 0.0;
            } else if new.signum() != pos.signum() {
                self.position = new;
                self.avg_entry = px;
            } else {
                self.position = new;
            }
        }
        self.realized += realized;
        realized
    }

    pub fn unrealized(&self, mark: f64) -> f64 {
        self.position * (mark - self.avg_entry)
    }
}

/// Cumulative attribution lines, settlement units. Costs are positive.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Lines {
    pub premium_paid: f64,
    /// Cash realized at expiry/exercise (payoff net of exercise costs).
    pub option_payoff: f64,
    pub exercise_costs: f64,
    pub hedge_realized: f64,
    pub funding_paid: f64,
    pub hedge_fees: f64,
    pub hedge_slippage: f64,
    pub gas: f64,
    pub hedge_turnover_notional: f64,
    pub hedge_fills: u64,
    pub exercise_turnover_notional: f64,
    pub fills: u64,
    pub declines_capacity: u64,
    pub declines_stale: u64,
    pub declines_priced_zero: u64,
    /// Venue rejections of hedge orders (doc 08 §7.2).
    pub hedge_rejects: u64,
    /// Doc 08 §7.2/§7.3 venue lifecycle and margin lines.
    pub maker_fees: f64,
    pub taker_fills: u64,
    pub passive_fills: u64,
    pub partial_fills: u64,
    pub cancels: u64,
    pub liquidations: u64,
    /// Margin forfeited to liquidations (penalty on top of mark P&L).
    pub liquidation_loss: f64,
    pub margin_topups: u64,
    pub topup_total: f64,
    /// Top-ups refused by the venue (outage).
    pub topup_rejects: u64,
    /// Top-ups the desk declined: 24 h cap or no free cash.
    pub topup_declines: u64,
    /// Hedge orders not sent because the entry margin could not be
    /// funded from free cash (no risk without margin).
    pub hedge_declines_margin: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Ledger {
    pub cash: f64,
    pub positions: Vec<Position>,
    pub perp: Perp,
    pub lines: Lines,
    pub next_id: u64,
}

impl Ledger {
    pub fn new(nav0: f64) -> Self {
        Self { cash: nav0, positions: Vec::new(), perp: Perp::default(), lines: Lines::default(), next_id: 0 }
    }

    pub fn option_marks(&self) -> f64 {
        self.positions.iter().map(|p| p.mark * p.qty).sum()
    }

    /// NAV = cash + option marks + perp collateral + perp unrealized at
    /// the venue mark (doc 08 §2.3).
    pub fn nav(&self, mark: f64) -> f64 {
        self.cash + self.option_marks() + self.perp.collateral + self.perp.unrealized(mark)
    }

    pub fn premium_deployed(&self) -> f64 {
        self.positions.iter().map(|p| p.mark * p.qty).sum()
    }

    pub fn premium_by_type(&self, is_put: bool) -> f64 {
        self.positions.iter().filter(|p| p.is_put == is_put).map(|p| p.mark * p.qty).sum()
    }

    pub fn premium_by_expiry(&self, expiry_ms: i64) -> f64 {
        self.positions.iter().filter(|p| p.expiry_ms == expiry_ms).map(|p| p.mark * p.qty).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perp_average_entry_realizes_on_reduce_and_reversal() {
        let mut p = Perp::default();
        assert_eq!(p.fill(-100.0, 10.0), 0.0);
        assert_eq!(p.fill(-100.0, 12.0), 0.0);
        assert!((p.avg_entry - 11.0).abs() < 1e-12);
        // Short 200 @11, cover 50 @9: realize (11−9)×50 = 100.
        assert!((p.fill(50.0, 9.0) - 100.0).abs() < 1e-12);
        // Reverse: buy 250 @10 closes 150 short (realize 150) and opens long 100 @10.
        assert!((p.fill(250.0, 10.0) - 150.0).abs() < 1e-12);
        assert_eq!(p.position, 100.0);
        assert_eq!(p.avg_entry, 10.0);
        assert!((p.unrealized(11.0) - 100.0).abs() < 1e-12);
        assert!((p.realized - 250.0).abs() < 1e-12);
    }

    #[test]
    fn nav_reconciles_cash_marks_and_perp() {
        let mut l = Ledger::new(1000.0);
        l.cash -= 100.0;
        l.positions.push(Position {
            id: 0, is_put: false, strike: 1.0, expiry_ms: 0, qty: 10.0, premium_paid: 100.0, sigma_paid: 0.5,
            sigma_surface: 0.55, opened_ms: 0, spot_open: 1.0, delta_open: 0.5, gamma_open: 0.0, vega_open: 0.0,
            writer_net_premium: 100.0, mark: 9.0, qty_open: 10.0, pending_units: 0.0, last_check_ms: 0, exercise_net: 0.0,
        });
        l.perp.fill(-5.0, 1.0);
        // cash 900 + marks 90 + perp short 5 @1 marked 1.1 → −0.5
        assert!((l.nav(1.1) - 989.5).abs() < 1e-9);
    }
}
