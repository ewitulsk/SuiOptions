//! Bucket write/exercise/redeem mechanics, mirrored from
//! `contracts/sources/bucket.move` with identical integer math. Any change
//! to the Move module's `apply_strike`, cursor assignment, or redeem split
//! must be reflected here (and vice versa) — the localnet golden-file diff
//! (doc 06 §9.3) enforces it.

use crate::types::{SettleAmt, UnderlyingAmt};

/// Mirror of `bucket.move::MAX_STRIKE_SCALE`.
pub const MAX_STRIKE_SCALE: u8 = 38;

/// Mirror of `bucket.move::pow10`. Panics above `MAX_STRIKE_SCALE`, as the
/// Move version aborts.
pub fn pow10(exp: u8) -> u128 {
    assert!(exp <= MAX_STRIKE_SCALE, "strike scale too large");
    let mut result: u128 = 1;
    for _ in 0..exp {
        result *= 10;
    }
    result
}

/// Mirror of `bucket.move::apply_strike`:
/// settlement = round_half_up((amount × strike) / 10^strike_scale).
/// Panics if the result exceeds u64::MAX, as the Move cast aborts.
pub fn apply_strike(amount: u128, strike: u128, strike_scale: u8) -> u64 {
    let divisor = pow10(strike_scale);
    let numerator = amount.checked_mul(strike).expect("apply_strike overflow");
    let half = divisor / 2;
    u64::try_from((numerator + half) / divisor).expect("settlement exceeds u64")
}

/// A minted position's assignment range `[start, end)` on the bucket's
/// write cursor (mirror of `position.move`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PositionRange {
    pub start: u128,
    pub end: u128,
}

impl PositionRange {
    pub fn amount(&self) -> u128 {
        self.end - self.start
    }
}

/// In-memory mirror of one `Bucket`'s balances and cursors.
#[derive(Clone, Debug)]
pub struct SimBucket {
    pub strike: u128,
    pub strike_scale: u8,
    pub total_written: u128,
    pub exercise_cursor: u128,
    pub underlying_balance: u64,
    pub settlement_balance: u64,
}

impl SimBucket {
    pub fn new(strike: u128, strike_scale: u8) -> Self {
        assert!(strike_scale <= MAX_STRIKE_SCALE, "strike scale too large");
        Self {
            strike,
            strike_scale,
            total_written: 0,
            exercise_cursor: 0,
            underlying_balance: 0,
            settlement_balance: 0,
        }
    }

    /// The strike ratio at scale 0 (settlement smallest-units per
    /// underlying smallest-unit), for f64 pricing contexts.
    pub fn strike_scale_zero(&self) -> f64 {
        self.strike as f64 / pow10(self.strike_scale) as f64
    }

    /// Mirror of `bucket.move::do_write`: escrow underlying, advance the
    /// write cursor, return the minted range.
    pub fn write(&mut self, amount: UnderlyingAmt) -> PositionRange {
        assert!(amount.0 > 0, "zero amount");
        self.underlying_balance += amount.0;
        let start = self.total_written;
        let end = start + amount.0 as u128;
        self.total_written = end;
        PositionRange { start, end }
    }

    /// Mirror of `bucket.move::exercise`: burn `amount` calls, pay the
    /// strike, receive underlying. Returns (underlying out, settlement
    /// paid in).
    pub fn exercise(&mut self, amount: UnderlyingAmt) -> (UnderlyingAmt, SettleAmt) {
        assert!(amount.0 > 0, "zero amount");
        assert!(
            self.exercise_cursor + amount.0 as u128 <= self.total_written,
            "cursor overflow"
        );
        let required = apply_strike(amount.0 as u128, self.strike, self.strike_scale);
        self.settlement_balance += required;
        self.exercise_cursor += amount.0 as u128;
        self.underlying_balance -= amount.0;
        (amount, SettleAmt(required))
    }

    /// Mirror of `bucket.move::redeem_position`: split a position's range
    /// against the final cursor. Returns (underlying back, settlement
    /// back).
    pub fn redeem(&mut self, range: PositionRange) -> (UnderlyingAmt, SettleAmt) {
        let cursor = self.exercise_cursor;
        let exercised: u128 = if cursor <= range.start {
            0
        } else if cursor >= range.end {
            range.end - range.start
        } else {
            cursor - range.start
        };
        let unexercised = (range.end - range.start) - exercised;

        let underlying_amount = u64::try_from(unexercised).expect("range exceeds u64");
        let settlement_amount = apply_strike(exercised, self.strike, self.strike_scale);

        self.underlying_balance -= underlying_amount;
        self.settlement_balance -= settlement_amount;
        (UnderlyingAmt(underlying_amount), SettleAmt(settlement_amount))
    }

    /// The exercised fraction of `range`, φ ∈ [0, 1] (doc 06 §2.1).
    pub fn exercised_fraction(&self, range: PositionRange) -> f64 {
        if range.amount() == 0 {
            return 0.0;
        }
        let cursor = self.exercise_cursor;
        let exercised: u128 = if cursor <= range.start {
            0
        } else if cursor >= range.end {
            range.amount()
        } else {
            cursor - range.start
        };
        exercised as f64 / range.amount() as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pow10_matches_move_table() {
        assert_eq!(pow10(0), 1);
        assert_eq!(pow10(1), 10);
        assert_eq!(pow10(6), 1_000_000);
        assert_eq!(pow10(38), 10u128.pow(38));
    }

    #[test]
    #[should_panic(expected = "strike scale too large")]
    fn pow10_above_max_panics() {
        pow10(39);
    }

    #[test]
    fn apply_strike_scale_zero_is_plain_multiply() {
        // Mirrors bucket_tests::test_apply_strike_scale_zero_is_plain_multiply.
        assert_eq!(apply_strike(40, 50_000, 0), 2_000_000);
    }

    #[test]
    fn apply_strike_round_half_up_boundaries() {
        // Mirrors bucket_tests::test_apply_strike_round_half_up_boundaries:
        // strike 75 at scale 1 = 7.5 settlement per underlying unit.
        assert_eq!(apply_strike(1, 75, 1), 8); // 7.5 → 8 (half rounds up)
        assert_eq!(apply_strike(2, 75, 1), 15); // 15.0 → 15
        assert_eq!(apply_strike(3, 75, 1), 23); // 22.5 → 23
        // strike 74 at scale 1 = 7.4 → 7.4 rounds down.
        assert_eq!(apply_strike(1, 74, 1), 7);
    }

    #[test]
    fn apply_strike_sub_dollar_at_15_cents() {
        // Mirrors bucket_tests::test_apply_strike_sub_dollar_same_decimals:
        // TMICRO/TUSDC both 6 decimals, $0.15 ⇒ ratio 0.15 = 15 × 10^-2.
        // 1 TMICRO = 1_000_000 units → 150_000 settlement units ($0.15).
        assert_eq!(apply_strike(1_000_000, 15, 2), 150_000);
    }

    #[test]
    fn fifo_assignment_two_writers_partial_exercise() {
        // Mirrors bucket_tests::test_fifo_assignment_two_writers_partial_exercise.
        let mut b = SimBucket::new(50_000, 0);
        let p1 = b.write(UnderlyingAmt(100)); // [0, 100)
        let p2 = b.write(UnderlyingAmt(50)); // [100, 150)
        assert_eq!(p1, PositionRange { start: 0, end: 100 });
        assert_eq!(p2, PositionRange { start: 100, end: 150 });

        // Exercise 120: consumes all of p1 and 20 of p2.
        let (u, s) = b.exercise(UnderlyingAmt(120));
        assert_eq!(u, UnderlyingAmt(120));
        assert_eq!(s, SettleAmt(120 * 50_000));

        assert!((b.exercised_fraction(p1) - 1.0).abs() < 1e-12);
        assert!((b.exercised_fraction(p2) - 0.4).abs() < 1e-12);

        let (u1, s1) = b.redeem(p1);
        assert_eq!(u1, UnderlyingAmt(0));
        assert_eq!(s1, SettleAmt(100 * 50_000));
        let (u2, s2) = b.redeem(p2);
        assert_eq!(u2, UnderlyingAmt(30));
        assert_eq!(s2, SettleAmt(20 * 50_000));

        assert_eq!(b.underlying_balance, 0);
        assert_eq!(b.settlement_balance, 0);
    }

    #[test]
    fn redeem_conserves_units_on_round_half_up_path() {
        // One position, scaled strike 7.5: a single 3-unit exercise pays
        // round(22.5) = 23 and the redeem returns exactly 23.
        let mut b = SimBucket::new(75, 1);
        let p = b.write(UnderlyingAmt(10));
        let (_, paid) = b.exercise(UnderlyingAmt(3));
        assert_eq!(paid, SettleAmt(23));
        let (u, s) = b.redeem(p);
        assert_eq!(u, UnderlyingAmt(7));
        assert_eq!(s, SettleAmt(23));
        assert_eq!(b.underlying_balance, 0);
        assert_eq!(b.settlement_balance, 0);
    }

    #[test]
    #[should_panic(expected = "cursor overflow")]
    fn exercise_beyond_written_panics() {
        let mut b = SimBucket::new(50_000, 0);
        b.write(UnderlyingAmt(10));
        b.exercise(UnderlyingAmt(11));
    }
}
