//! Uniform strike grid for `exchange create-buckets`.
//!
//! The spot- and vol-driven grid builders that used to live here went away
//! with the option-scheduler's roll loop: strikes are listed by api-service's
//! ladder and created on demand now, so nothing derives a grid from market
//! data any more. What remains is the explicit arithmetic grid this admin
//! command takes straight from its CLI args.
//!
//! Strikes are in **scaled chain units**: the real settlement-smallest-units
//! owed per underlying smallest-unit is `strike / 10^strike_scale`.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrikeGrid {
    pub start_strike: u128,
    pub strike_interval: u128,
    pub count: u64,
    pub strike_scale: u8,
}

impl StrikeGrid {
    /// Expand the uniform grid into explicit per-bucket strikes.
    pub fn strikes(&self) -> Vec<u128> {
        (0..self.count)
            .map(|i| self.start_strike + (i as u128) * self.strike_interval)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_to_evenly_spaced_strikes() {
        let grid = StrikeGrid {
            start_strike: 50_000,
            strike_interval: 5_000,
            count: 4,
            strike_scale: 2,
        };
        assert_eq!(grid.strikes(), vec![50_000, 55_000, 60_000, 65_000]);
    }

    #[test]
    fn a_single_strike_grid_is_just_the_start() {
        let grid = StrikeGrid {
            start_strike: 630,
            strike_interval: 0,
            count: 1,
            strike_scale: 0,
        };
        assert_eq!(grid.strikes(), vec![630]);
    }
}
