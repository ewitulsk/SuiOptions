//! Strike-grid math, shared by api-service's `/buckets` ladder, the vault
//! keeper's strike selection, and the vault-sim backtester
//! (docs/vault-implementation-guide/05-offchain-services.md §4.2). One
//! implementation so the sim sees exactly the strikes production creates.

/// Per-pair z-ladders: ATM is always present and z = 1.30 ≈ the vault's
/// 0.1-delta target is always on-grid.
pub const SUI_LADDER: [f64; 5] = [0.0, 0.65, 1.30, 1.95, 2.60];
pub const BTC_LADDER: [f64; 7] = [-0.65, 0.0, 0.65, 1.30, 1.95, 2.60, 3.25];

/// Round to a "nice" exchange-style increment: the largest of
/// 10^(⌊log10 x⌋ − 2) × {1, 2.5, 5} (or a power-of-ten multiple) that
/// stays below 1% of `x`, then snap to the nearest multiple.
pub fn round_nice(x: f64) -> f64 {
    assert!(x > 0.0 && x.is_finite());
    let base = 10f64.powi(x.log10().floor() as i32 - 2);
    let mut increment = base;
    for mult in [1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0] {
        let candidate = base * mult;
        if candidate <= 0.01 * x {
            increment = candidate;
        } else {
            break;
        }
    }
    (x / increment).round() * increment
}

/// Ticks a listed board is allowed to use, as a mantissa on a power of ten.
const TICK_MANTISSAS: [f64; 4] = [1.0, 2.0, 2.5, 5.0];

/// Upper bound on how many strikes one series may list. A high-vol long-dated
/// window can span several hundred ticks; past this we coarsen the tick rather
/// than serve an unusable wall of strikes.
const MAX_LATTICE_STRIKES: usize = 40;

/// Largest `{1, 2, 2.5, 5} × 10ⁿ` value that is still `<= target`.
///
/// This is the *tick* picker, distinct from [`round_nice`]'s snapping
/// increment: it deliberately lands on the coarse, human-readable levels a
/// listed board quotes (1000, 2500, 0.05) rather than the ~0.5%-precision
/// increment `round_nice` uses.
pub fn nice_tick(target: f64) -> f64 {
    assert!(target > 0.0 && target.is_finite());
    let decade = 10f64.powi(target.log10().floor() as i32);
    // 1 × decade is <= target by construction, so `best` always initialises
    // to a valid tick.
    let mut best = decade;
    for m in TICK_MANTISSAS {
        let candidate = m * decade;
        if candidate <= target && candidate > best {
            best = candidate;
        }
    }
    best
}

/// A **fixed-lattice, vol-sized** strike ladder: strikes sit on absolute
/// multiples of a tick, and only the *window* around spot breathes with the
/// vol regime and the tenor.
///
/// The distinction from [`build_z_ladder`] matters for anything that polls:
/// a z-ladder recomputes every strike whenever spot or σ move, so a strike a
/// user selected can shift or vanish between polls. Here the tick is a
/// function of spot's decade alone, so a strike stays at the same absolute
/// level across polls (a BTC 65 000 stays 65 000) and the ladder changes only
/// by gaining or losing entries at the edges.
///
/// - tick   = [`nice_tick`]`(tick_pct × spot)`, coarsened if the window would
///            otherwise list more than [`MAX_LATTICE_STRIKES`] strikes
/// - window = `spot · exp(±z_width · σ · √τ)`
///
/// Returns strikes ascending. Empty only if the window is degenerate (it
/// always contains at least the ticks bracketing spot for sane inputs).
pub fn lattice_strikes(
    spot: f64,
    sigma: f64,
    tau_years: f64,
    tick_pct: f64,
    z_width: f64,
) -> Vec<f64> {
    assert!(spot > 0.0 && spot.is_finite());
    assert!(sigma > 0.0 && tau_years > 0.0);
    assert!(tick_pct > 0.0 && z_width > 0.0);

    let half_width = z_width * sigma * tau_years.sqrt();
    let lo = spot * (-half_width).exp();
    let hi = spot * half_width.exp();

    // Coarsen up the mantissa ladder until the strike count fits. Each step is
    // a valid board tick, so the lattice stays on human-readable levels.
    let mut tick = nice_tick(tick_pct * spot);
    while ((hi - lo) / tick).floor() as usize + 1 > MAX_LATTICE_STRIKES {
        tick = next_tick_up(tick);
    }

    let first = (lo / tick).ceil();
    let last = (hi / tick).floor();
    if last < first {
        return Vec::new();
    }
    let mut out = Vec::with_capacity((last - first) as usize + 1);
    let mut i = first;
    while i <= last {
        out.push(i * tick);
        i += 1.0;
    }
    out
}

/// Next coarser board tick: step to the next mantissa, or roll into the next
/// decade at mantissa 1.
fn next_tick_up(tick: f64) -> f64 {
    let decade = 10f64.powi(tick.log10().floor() as i32);
    let mantissa = tick / decade;
    for m in TICK_MANTISSAS {
        // Mantissas are reconstructed through f64 division, so compare with a
        // tolerance rather than `>`.
        if m > mantissa * (1.0 + 1e-9) {
            return m * decade;
        }
    }
    10.0 * decade
}

/// Strikes at K_i = round_nice(S · exp(z_i · σ · √τ)), strictly increasing
/// after rounding (colliding strikes bump up). Spacing adapts to the vol
/// regime automatically: ~5% intervals in calm markets, ~12% in wild ones.
pub fn build_z_ladder(spot: f64, sigma: f64, tau_years: f64, ladder: &[f64]) -> Vec<f64> {
    assert!(spot > 0.0 && sigma > 0.0 && tau_years > 0.0);
    let sqrt_tau = tau_years.sqrt();
    let mut strikes: Vec<f64> = ladder
        .iter()
        .map(|z| round_nice(spot * (z * sigma * sqrt_tau).exp()))
        .collect();
    for i in 1..strikes.len() {
        if strikes[i] <= strikes[i - 1] {
            strikes[i] = round_nice(strikes[i - 1] * 1.01);
            // round_nice can round back down onto the collision; force
            // progress with the raw bump in that case.
            if strikes[i] <= strikes[i - 1] {
                strikes[i] = strikes[i - 1] * 1.01;
            }
        }
    }
    strikes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_nice_produces_exchange_style_strikes() {
        // 117_483.91: base 10³, largest increment ≤ 1% (1174.8) is 1000.
        assert_eq!(round_nice(117_483.91), 117_000.0);
        // 3.61 at 1% = 0.0361 → increment 0.025.
        assert!((round_nice(3.61) - 3.6).abs() < 1e-12);
        // Stays within half an increment ≤ 0.5% of x.
        for x in [0.17, 3.47, 92.3, 1_234.5, 117_483.91] {
            let r = round_nice(x);
            assert!((r - x).abs() / x <= 0.005 + 1e-12, "{x} → {r}");
        }
    }

    #[test]
    fn nice_tick_lands_on_board_levels() {
        // 2.5% of a $63k spot is 1577 → the 1000 tick.
        assert_eq!(nice_tick(1_577.0), 1_000.0);
        assert_eq!(nice_tick(2_000.0), 2_000.0);
        assert_eq!(nice_tick(2_400.0), 2_000.0);
        assert_eq!(nice_tick(2_500.0), 2_500.0);
        assert_eq!(nice_tick(9_999.0), 5_000.0);
        // Sub-dollar assets get sub-cent ticks from the same rule.
        assert!((nice_tick(0.0341) - 0.025).abs() < 1e-12);
        assert!((nice_tick(0.0012) - 0.001).abs() < 1e-12);
    }

    #[test]
    fn lattice_strikes_are_absolute_multiples_of_the_tick() {
        let ks = lattice_strikes(63_090.2, 0.35, 7.0 / 365.0, 0.025, 2.5);
        assert!(!ks.is_empty());
        for k in &ks {
            assert!((k / 1_000.0 - (k / 1_000.0).round()).abs() < 1e-9, "{k} off-tick");
        }
        for w in ks.windows(2) {
            assert!(w[1] > w[0], "not increasing: {ks:?}");
        }
    }

    /// The whole point of the fixed lattice: a strike keeps its absolute
    /// level as spot moves, so a polling client never sees a selected strike
    /// shift underneath it.
    #[test]
    fn lattice_strikes_are_stable_as_spot_moves() {
        let (sigma, tau) = (0.35, 7.0 / 365.0);
        let a = lattice_strikes(63_090.0, sigma, tau, 0.025, 2.5);
        let b = lattice_strikes(64_500.0, sigma, tau, 0.025, 2.5);
        let overlap: Vec<f64> = a.iter().copied().filter(|k| b.contains(k)).collect();
        // Both ladders quote the same absolute levels where they overlap; only
        // the window edges differ.
        assert!(overlap.len() >= a.len() - 2, "lattice shifted: {a:?} vs {b:?}");
    }

    #[test]
    fn lattice_window_widens_with_vol_and_tenor() {
        let width = |sigma: f64, tau: f64| {
            let ks = lattice_strikes(63_090.0, sigma, tau, 0.025, 2.5);
            ks.last().unwrap() - ks.first().unwrap()
        };
        let week = 7.0 / 365.0;
        assert!(width(0.90, week) > width(0.35, week), "vol must widen the window");
        assert!(width(0.35, 4.0 * week) > width(0.35, week), "tenor must widen it");
    }

    #[test]
    fn lattice_coarsens_rather_than_listing_hundreds_of_strikes() {
        // 200% vol over a year: the raw 2.5%-of-spot tick would list ~250
        // strikes, so the tick has to step up instead.
        let ks = lattice_strikes(63_090.0, 2.0, 1.0, 0.025, 2.5);
        assert!(ks.len() <= MAX_LATTICE_STRIKES, "listed {} strikes", ks.len());
        assert!(ks.len() > 5, "coarsened away to nothing: {ks:?}");
        let tick = ks[1] - ks[0];
        assert!(tick > 1_000.0, "tick never coarsened: {tick}");
    }

    #[test]
    fn next_tick_up_walks_the_mantissa_ladder_into_the_next_decade() {
        assert_eq!(next_tick_up(1_000.0), 2_000.0);
        assert_eq!(next_tick_up(2_000.0), 2_500.0);
        assert_eq!(next_tick_up(2_500.0), 5_000.0);
        assert_eq!(next_tick_up(5_000.0), 10_000.0);
    }

    #[test]
    fn lattice_brackets_spot_for_small_price_assets() {
        let ks = lattice_strikes(0.6827, 0.85, 7.0 / 365.0, 0.05, 2.5);
        assert!(ks.first().unwrap() < &0.6827 && ks.last().unwrap() > &0.6827, "{ks:?}");
    }

    #[test]
    fn ladder_is_strictly_increasing_and_hits_z_targets() {
        let (spot, sigma, tau) = (3.47, 0.85, 7.0 / 365.0);
        let grid = build_z_ladder(spot, sigma, tau, &SUI_LADDER);
        assert_eq!(grid.len(), 5);
        for w in grid.windows(2) {
            assert!(w[1] > w[0], "not increasing: {grid:?}");
        }
        for (k, z) in grid.iter().zip(SUI_LADDER) {
            let target = spot * (z * sigma * tau.sqrt()).exp();
            assert!((k - target).abs() / target <= 0.005 + 1e-12, "z={z}: {k} vs {target}");
        }
    }

    #[test]
    fn ladder_spacing_adapts_to_vol_regime() {
        let tau = 7.0 / 365.0;
        let calm = build_z_ladder(100.0, 0.30, tau, &SUI_LADDER);
        let wild = build_z_ladder(100.0, 1.20, tau, &SUI_LADDER);
        let calm_width = calm.last().unwrap() / calm.first().unwrap() - 1.0;
        let wild_width = wild.last().unwrap() / wild.first().unwrap() - 1.0;
        assert!(wild_width > calm_width * 3.0, "calm {calm_width}, wild {wild_width}");
    }

    #[test]
    fn collision_bumps_keep_strikes_increasing() {
        // Tiny sigma + short tenor: unrounded strikes nearly coincide.
        let grid = build_z_ladder(100.0, 0.01, 1.0 / 365.0, &SUI_LADDER);
        for w in grid.windows(2) {
            assert!(w[1] > w[0], "not increasing: {grid:?}");
        }
    }

    #[test]
    fn btc_ladder_has_itm_and_far_otm_wings() {
        let grid = build_z_ladder(117_000.0, 0.55, 7.0 / 365.0, &BTC_LADDER);
        assert_eq!(grid.len(), 7);
        assert!(grid[0] < 117_000.0, "first strike is ITM");
        assert!(*grid.last().unwrap() > 117_000.0 * 1.2, "far wing exists");
    }
}
