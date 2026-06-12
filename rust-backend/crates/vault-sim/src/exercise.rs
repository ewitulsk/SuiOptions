//! Holder exercise policies (doc 06 §4). Rational-at-expiry full exercise
//! is the conservative anchor (front-of-queue bound, doc 06 §2.1); the
//! other policies quantify the *upside* of behavioral under- or early
//! exercise — they never defend the downside.

use crate::paths::Bar;

pub trait ExercisePolicy {
    /// Called each bar while the option lives. Returns the *incremental*
    /// exercised fraction of the vault's written range in [0, 1]; the
    /// engine accumulates and clamps the total at 1. `t_left == 0.0`
    /// marks the expiry bar. `k` is in the same units as `bar.close`.
    fn step(&mut self, bar: &Bar, k: f64, t_left: f64) -> f64;
}

/// φ_T = 1{S_T > K}: everyone exercises rationally at expiry, nobody
/// before. The worst case for the vault.
pub struct RationalExpiry;

impl ExercisePolicy for RationalExpiry {
    fn step(&mut self, bar: &Bar, k: f64, t_left: f64) -> f64 {
        if t_left <= 0.0 && bar.close > k {
            1.0
        } else {
            0.0
        }
    }
}

/// Everyone exercises the moment intrinsic exceeds `thresh_bps` of the
/// strike (S/K − 1 > thresh). Early exercise forfeits remaining time value
/// to the vault — weakly better than `RationalExpiry` (doc 06 §2.1).
pub struct EarlyIntrinsic {
    pub thresh_bps: u64,
}

impl ExercisePolicy for EarlyIntrinsic {
    fn step(&mut self, bar: &Bar, k: f64, t_left: f64) -> f64 {
        let early_trigger = bar.close / k - 1.0 > self.thresh_bps as f64 / 10_000.0;
        let expiry_itm = t_left <= 0.0 && bar.close > k;
        if early_trigger || expiry_itm {
            1.0 // engine clamps cumulative at 1
        } else {
            0.0
        }
    }
}

/// φ_T = frac × 1{S_T > K}: a fixed share of holders never exercises
/// (lost keys, dust positions, inattentive holders).
pub struct Partial {
    pub frac: f64,
}

impl ExercisePolicy for Partial {
    fn step(&mut self, bar: &Bar, k: f64, t_left: f64) -> f64 {
        if t_left <= 0.0 && bar.close > k {
            self.frac
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(close: f64) -> Bar {
        Bar::flat(0, close)
    }

    #[test]
    fn rational_fires_only_at_itm_expiry() {
        let mut p = RationalExpiry;
        assert_eq!(p.step(&bar(150.0), 100.0, 0.01), 0.0); // ITM, not expiry
        assert_eq!(p.step(&bar(90.0), 100.0, 0.0), 0.0); // expiry, OTM
        assert_eq!(p.step(&bar(150.0), 100.0, 0.0), 1.0); // expiry, ITM
        assert_eq!(p.step(&bar(100.0), 100.0, 0.0), 0.0); // exactly ATM: no
    }

    #[test]
    fn early_intrinsic_triggers_on_threshold() {
        let mut p = EarlyIntrinsic { thresh_bps: 500 };
        assert_eq!(p.step(&bar(104.0), 100.0, 0.01), 0.0); // +4% < 5%
        assert_eq!(p.step(&bar(106.0), 100.0, 0.01), 1.0); // +6% > 5%
        // Still rational at expiry even if the threshold never hit.
        assert_eq!(p.step(&bar(101.0), 100.0, 0.0), 1.0);
    }

    #[test]
    fn partial_exercises_a_fraction_at_expiry() {
        let mut p = Partial { frac: 0.6 };
        assert_eq!(p.step(&bar(150.0), 100.0, 0.01), 0.0);
        assert_eq!(p.step(&bar(150.0), 100.0, 0.0), 0.6);
        assert_eq!(p.step(&bar(90.0), 100.0, 0.0), 0.0);
    }
}
