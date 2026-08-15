//! Spot-anchored strike ladder for `GET /buckets` (SO-400).
//!
//! Before the any-strike overhaul the bucket catalog *was* whatever the (now
//! decommissioned) option-scheduler had rolled, so `/buckets` could simply
//! mirror the indexer. Any-strike creation removed the roll step and the
//! service with it: a bucket now springs into existence the moment someone
//! writes at a strike, which means the catalog has to advertise the strikes
//! that *could* exist rather than only the ones that already do.
//!
//! This module supplies the two synthetic axes:
//!
//! - [`expiry_board`] — the listed expiries (active week, next week, and the
//!   next two month-ends), which replaced the old roll cadence.
//! - [`ladder_strikes`] — the per-series strike lattice, delegating the math
//!   to [`pricing::grid::lattice_strikes`] so the board, the vault keeper and
//!   the backtester all quantise strikes identically.
//!
//! Everything here is pure; the caller supplies spot and σ (see
//! [`crate::handlers::buckets`], which fetches them from oracle-service and
//! degrades to real-buckets-only when either is unavailable).

use chrono::{DateTime, Datelike, TimeZone, Utc};
use serde::Deserialize;

/// Epoch-aligned weekly cadence. Unix epoch was a Thursday, so every
/// boundary is Thursday 00:00 UTC — the same alignment the retired roll
/// cadence used, which keeps already-created buckets (e.g. the 2026-08-27
/// series) on the board rather than orphaned beside it.
pub const WEEK_MS: i64 = 604_800_000;

/// How many month-end expiries the board lists beyond the two weeklies.
const MONTH_ENDS: usize = 2;

/// Bound on the forward walk that finds month-ends. Two month-ends are never
/// more than ~10 weeks out; this is purely a runaway guard.
const MAX_WEEKS_SCAN: i64 = 60;

/// One configured series family — which (underlying, settlement, kind) the
/// board lists, and how its lattice is shaped.
#[derive(Debug, Clone, Deserialize)]
pub struct LadderPair {
    /// Catalog ticker (`"TBTC"`) or a full coin type.
    pub underlying: String,
    pub settlement: String,
    /// `"call"` (default) | `"put"`.
    #[serde(default = "default_option_type")]
    pub option_type: String,
    /// Target tick as a fraction of spot, before snapping to a board level.
    /// 0.025 on a $63k underlying yields the 1 000 tick.
    #[serde(default = "default_tick_pct")]
    pub tick_pct: f64,
    /// Half-width of the listed window in standard deviations: the ladder
    /// spans `spot · exp(±z_width · σ · √τ)`.
    #[serde(default = "default_z_width")]
    pub z_width: f64,
    /// Realized-vol lookback requested from oracle-service.
    #[serde(default = "default_vol_window_days")]
    pub vol_window_days: u32,
    /// σ used when oracle-service can't serve realized vol. Without it a vol
    /// outage would silently empty the board.
    #[serde(default = "default_fallback_sigma")]
    pub fallback_sigma: f64,
}

fn default_option_type() -> String {
    "call".to_string()
}
fn default_tick_pct() -> f64 {
    0.025
}
fn default_z_width() -> f64 {
    2.5
}
fn default_vol_window_days() -> u32 {
    30
}
fn default_fallback_sigma() -> f64 {
    0.60
}

impl LadderPair {
    pub fn is_put(&self) -> bool {
        self.option_type.eq_ignore_ascii_case("put")
    }
}

/// The listed expiries, ascending: the active week, the next week, and the
/// next [`MONTH_ENDS`] month-end expiries.
///
/// "Month-end" is the last weekly boundary *within* a calendar month rather
/// than the calendar's last day, so the whole board sits on one cadence and a
/// month-end that coincides with a listed weekly collapses into it instead of
/// producing a duplicate series.
pub fn expiry_board(now_ms: i64) -> Vec<i64> {
    let active = next_weekly_after(now_ms);
    let next = active + WEEK_MS;

    let mut board = vec![active, next];
    let mut t = next;
    let mut found = 0;
    for _ in 0..MAX_WEEKS_SCAN {
        t += WEEK_MS;
        if found == MONTH_ENDS {
            break;
        }
        if is_last_weekly_of_month(t) {
            board.push(t);
            found += 1;
        }
    }

    board.sort_unstable();
    board.dedup();
    board
}

/// First epoch-aligned weekly boundary strictly after `now_ms`.
fn next_weekly_after(now_ms: i64) -> i64 {
    (now_ms.div_euclid(WEEK_MS) + 1) * WEEK_MS
}

/// True when the next weekly boundary lands in a different calendar month —
/// i.e. `t` is the last weekly expiry its month lists.
fn is_last_weekly_of_month(t: i64) -> bool {
    let (a, b) = (to_utc(t), to_utc(t + WEEK_MS));
    (a.year(), a.month()) != (b.year(), b.month())
}

fn to_utc(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().unwrap_or_else(Utc::now)
}

/// The lattice of listed strikes for one series, in display (USD) units.
///
/// Returns empty when spot is unusable — the caller then serves only the
/// buckets that genuinely exist rather than failing the request.
pub fn ladder_strikes(pair: &LadderPair, spot: f64, sigma: f64, tau_years: f64) -> Vec<f64> {
    if !(spot.is_finite() && spot > 0.0)
        || !(sigma.is_finite() && sigma > 0.0)
        || !(tau_years.is_finite() && tau_years > 0.0)
    {
        return Vec::new();
    }
    pricing::grid::lattice_strikes(spot, sigma, tau_years, pair.tick_pct, pair.z_width)
}

/// Years between `now_ms` and `expiry_ms`, floored just above zero so an
/// about-to-expire series still lists a (very tight) ladder instead of
/// tripping the positive-τ guard.
pub fn tau_years(now_ms: i64, expiry_ms: i64) -> f64 {
    const YEAR_MS: f64 = 365.0 * 86_400_000.0;
    (((expiry_ms - now_ms) as f64) / YEAR_MS).max(1.0 / 365.0 / 24.0)
}

/// Exact display-strike → `(strike_raw, strike_scale)`.
///
/// Mirrors the frontend's `strikeDisplayToRaw` (`frontend/src/tx/anystrike.ts`)
/// so a strike this endpoint advertises encodes to the same option-coin type
/// the browser then creates. The f64 is rendered at a precision derived from
/// the tick and parsed digit-exactly, keeping the conversion off the float
/// path where the ratio math would drift.
pub fn strike_to_raw(strike: f64, under_dec: u8, settle_dec: u8) -> Option<(u128, u8)> {
    if !strike.is_finite() || strike <= 0.0 {
        return None;
    }
    let rendered = format!("{strike:.*}", decimals_for(strike));
    let (int_part, frac_part) = match rendered.split_once('.') {
        Some((i, f)) => (i, f.trim_end_matches('0')),
        None => (rendered.as_str(), ""),
    };
    let digits = format!("{int_part}{frac_part}");
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return None;
    }
    let mut value: u128 = digits.parse().ok()?;

    // display = value × 10^(−frac_len); the bucket ratio is settlement
    // smallest-units per underlying smallest-unit, so shift by the decimals
    // difference as well.
    let mut scale = frac_part.len() as i32 - (settle_dec as i32 - under_dec as i32);
    if scale < 0 {
        value = value.checked_mul(10u128.checked_pow((-scale) as u32)?)?;
        scale = 0;
    }
    if scale > 38 {
        return None;
    }
    Some((value, scale as u8))
}

/// Decimal places that render `strike` to [`SIGNIFICANT_DIGITS`] significant
/// digits.
///
/// Rendering to a fixed *significant*-digit budget (rather than a fixed
/// number of decimals) does two jobs at once: it keeps every realistic strike
/// exact, and it erases the float noise a lattice strike picks up from
/// `index × tick` — 27 × 0.025 lands on 0.67500000000000003747 in f64, which
/// would otherwise encode as an absurd 17-digit significand. The budget also
/// sits just under the 13-digit ceiling the u40 significand field imposes, so
/// a rendered strike is always encodable.
fn decimals_for(strike: f64) -> usize {
    let decade = strike.abs().log10().floor() as i32;
    (SIGNIFICANT_DIGITS - 1 - decade).clamp(0, 17) as usize
}

/// Significant digits a strike is rendered to before digit-exact parsing.
const SIGNIFICANT_DIGITS: i32 = 12;

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-08-15T00:00:00Z — the day the ladder work landed, and a Saturday,
    /// so the active week is the following Thursday.
    const NOW: i64 = 1_786_838_400_000;

    fn iso(ms: i64) -> String {
        to_utc(ms).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    #[test]
    fn board_lists_two_weeklies_and_two_month_ends() {
        let board = expiry_board(NOW);
        let got: Vec<String> = board.iter().map(|t| iso(*t)).collect();
        assert_eq!(
            got,
            vec![
                "2026-08-20T00:00:00Z", // active week
                "2026-08-27T00:00:00Z", // next week (also August's last weekly)
                "2026-09-24T00:00:00Z", // September month-end
                "2026-10-29T00:00:00Z", // October month-end
            ]
        );
    }

    #[test]
    fn board_is_sorted_deduped_and_entirely_in_the_future() {
        let board = expiry_board(NOW);
        assert!(board.windows(2).all(|w| w[1] > w[0]), "{board:?}");
        assert!(board.iter().all(|t| *t > NOW), "{board:?}");
    }

    /// A month-end colliding with a listed weekly must collapse, not
    /// duplicate the series. On 2026-08-15 August's last weekly (08-27) *is*
    /// the "next week" entry.
    #[test]
    fn colliding_month_end_collapses_into_the_weekly() {
        let board = expiry_board(NOW);
        assert_eq!(board.len(), 4, "expected a collapsed board, got {board:?}");
    }

    #[test]
    fn weeklies_are_thursdays() {
        for t in expiry_board(NOW) {
            assert_eq!(to_utc(t).weekday(), chrono::Weekday::Thu, "{}", iso(t));
        }
    }

    #[test]
    fn board_rolls_forward_as_the_active_week_expires() {
        let board = expiry_board(NOW);
        // One second after the active expiry, it drops off and everything
        // shifts up by one.
        let rolled = expiry_board(board[0] + 1);
        assert!(!rolled.contains(&board[0]), "expired week still listed");
        assert_eq!(rolled[0], board[1]);
    }

    /// Collapse to the canonical `(sig, exp)` the on-chain encoding uses —
    /// the same trailing-zero strip as `option_coin::normalize_strike`.
    fn normalized(raw: u128, scale: u8) -> (u128, u8) {
        let (mut sig, mut exp) = (raw, scale);
        while sig % 10 == 0 && exp > 0 {
            sig /= 10;
            exp -= 1;
        }
        (sig, exp)
    }

    #[test]
    fn strike_to_raw_matches_the_frontend_encoding() {
        // TBTC(8)/TUSDC(6): the bucket ratio is strike × 10^(6−8).
        // `strikeDisplayToRaw("63000", 8, 6)` yields (63000n, 2) — the scale
        // absorbs the decimals difference rather than pre-normalising.
        assert_eq!(strike_to_raw(63_000.0, 8, 6), Some((63_000, 2)));
        // 98 765.43 is the odd manually-created staging strike.
        assert_eq!(strike_to_raw(98_765.43, 8, 6), Some((9_876_543, 4)));
        // Sub-dollar asset against a same-decimal stablecoin keeps resolution.
        assert_eq!(strike_to_raw(0.675, 6, 6), Some((675, 3)));
    }

    /// Two encodings of one strike must land on the same normalized spec —
    /// that's what decides the option-coin type, so a mismatch here would
    /// have the board advertise a different coin than the browser creates.
    #[test]
    fn strike_encodings_normalize_to_one_canonical_spec() {
        let (raw, scale) = strike_to_raw(63_000.0, 8, 6).unwrap();
        assert_eq!(normalized(raw, scale), (630, 0));
        // The staging bucket's own on-chain pair normalizes identically.
        assert_eq!(normalized(9_876_543, 4), (9_876_543, 4));
    }

    /// A lattice strike arrives as `index × tick` in f64, which is not
    /// generally the clean decimal it looks like. The renderer must erase
    /// that noise instead of encoding a 17-digit significand.
    #[test]
    fn strike_to_raw_erases_float_noise_from_the_lattice() {
        let noisy = 27.0 * 0.025; // 0.67500000000000003747…
        assert_eq!(strike_to_raw(noisy, 6, 6), Some((675, 3)));

        let pair = LadderPair {
            underlying: "TSUI".into(),
            settlement: "TUSDC".into(),
            option_type: "call".into(),
            tick_pct: 0.05,
            z_width: 2.5,
            vol_window_days: 30,
            fallback_sigma: 0.85,
        };
        for k in ladder_strikes(&pair, 0.6827, 0.85, 7.0 / 365.0) {
            let (raw, scale) = strike_to_raw(k, 6, 6).expect("lattice strike encodable");
            let (sig, _) = normalized(raw, scale);
            assert!(sig <= 0xFF_FFFF_FFFF, "{k} → significand {sig} overflows u40");
        }
    }

    #[test]
    fn strike_to_raw_round_trips_through_the_usd_projection() {
        for (strike, ud, sd) in [
            (63_000.0, 8u8, 6u8),
            (0.675, 6, 6),
            (1.05, 9, 6),
            (0.025, 6, 6),
        ] {
            let (raw, scale) = strike_to_raw(strike, ud, sd).expect("encodable");
            let back = raw as f64 * 10f64.powi(ud as i32 - sd as i32 - scale as i32);
            assert!(
                (back - strike).abs() / strike < 1e-9,
                "{strike} → ({raw}, {scale}) → {back}"
            );
        }
    }

    #[test]
    fn strike_to_raw_rejects_nonsense() {
        assert_eq!(strike_to_raw(0.0, 8, 6), None);
        assert_eq!(strike_to_raw(-1.0, 8, 6), None);
        assert_eq!(strike_to_raw(f64::NAN, 8, 6), None);
    }

    #[test]
    fn ladder_strikes_degrade_to_empty_on_bad_inputs() {
        let pair = LadderPair {
            underlying: "TBTC".into(),
            settlement: "TUSDC".into(),
            option_type: "call".into(),
            tick_pct: 0.025,
            z_width: 2.5,
            vol_window_days: 30,
            fallback_sigma: 0.6,
        };
        assert!(ladder_strikes(&pair, 0.0, 0.5, 0.02).is_empty());
        assert!(ladder_strikes(&pair, 63_000.0, 0.0, 0.02).is_empty());
        assert!(ladder_strikes(&pair, 63_000.0, 0.5, 0.0).is_empty());
        assert!(!ladder_strikes(&pair, 63_000.0, 0.5, 0.02).is_empty());
    }

    #[test]
    fn tau_stays_positive_for_an_expiring_series() {
        assert!(tau_years(NOW, NOW) > 0.0);
        assert!(tau_years(NOW, NOW - WEEK_MS) > 0.0);
        let week = tau_years(NOW, NOW + WEEK_MS);
        assert!((week - 7.0 / 365.0).abs() < 1e-9, "{week}");
    }
}
