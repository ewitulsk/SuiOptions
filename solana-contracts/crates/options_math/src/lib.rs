//! Pure math shared by the options protocol programs.
//!
//! Ports the arithmetic of the Sui Move contracts bit-for-bit:
//! - `bucket.move`: `pow10`, `apply_strike` (round-half-up)
//! - `put_bucket.move`: `apply_strike_ceil` / `apply_strike_floor`
//! - `bucket.move::skim_fee`: floor fee in u128
//! - `rfq.move` / `swap_auction.move`: min-next-bid ceiling division
//! - `vault.move`: pps share math (floor both directions),
//!   `settlement_notional` (half-up) / `settlement_to_underlying` (floor)
//!
//! Every function is total over its Option/checked signature — callers map
//! `None` to their program's error code. No panics, no I/O, no deps beyond
//! a u256 type, so the crate audits as a standalone unit and its behavior
//! is locked by the golden vectors below (copied from the Move tests).

#![no_std]

use primitive_types::U256;

/// Maximum supported strike_scale: 10^38 is the largest power of ten that
/// fits in u128 (mirrors `bucket::MAX_STRIKE_SCALE`).
pub const MAX_STRIKE_SCALE: u8 = 38;

/// Price-per-share fixed point: pps of `PPS_SCALE` ⇒ 1 share == 1
/// underlying smallest-unit (mirrors `vault::PPS_SCALE`).
pub const PPS_SCALE: u128 = 1_000_000_000_000;

pub const BPS_DENOM: u128 = 10_000;

/// 10^exp for exp ∈ [0, MAX_STRIKE_SCALE]; `None` beyond the cap.
pub fn pow10(exp: u8) -> Option<u128> {
    if exp > MAX_STRIKE_SCALE {
        return None;
    }
    Some(10u128.pow(exp as u32))
}

/// settlement = round_half_up((amount × strike) / 10^strike_scale).
///
/// Round-half-up (not floor) so a tiny exercise rounds to the nearest
/// settlement smallest-unit instead of consistently truncating to zero in
/// the buyer's favor (mirrors `bucket::apply_strike`). `None` on
/// strike_scale > 38, u128 overflow in the multiply, or a result that
/// doesn't fit u64 (token amounts are u64).
pub fn apply_strike(amount: u128, strike: u128, strike_scale: u8) -> Option<u64> {
    let divisor = pow10(strike_scale)?;
    let numerator = amount.checked_mul(strike)?;
    let half = divisor / 2;
    u64::try_from(numerator.checked_add(half)? / divisor).ok()
}

/// ceil((amount × strike) / 10^strike_scale) — put collateral sizing
/// (mirrors `put_bucket::apply_strike_ceil`). Rounding UP on collateral-in
/// is what makes the put bucket provably solvent.
pub fn apply_strike_ceil(amount: u128, strike: u128, strike_scale: u8) -> Option<u64> {
    let divisor = pow10(strike_scale)?;
    let numerator = amount.checked_mul(strike)?;
    u64::try_from(numerator.checked_add(divisor - 1)? / divisor).ok()
}

/// floor((amount × strike) / 10^strike_scale) — every put cash payout
/// (mirrors `put_bucket::apply_strike_floor`).
pub fn apply_strike_floor(amount: u128, strike: u128, strike_scale: u8) -> Option<u64> {
    let divisor = pow10(strike_scale)?;
    let numerator = amount.checked_mul(strike)?;
    u64::try_from(numerator / divisor).ok()
}

/// Protocol fee = floor(gross × fee_bps / 10_000), computed in u128 —
/// matches `bucket::skim_fee` exactly.
pub fn fee_amount(gross: u64, fee_bps: u64) -> u64 {
    ((gross as u128 * fee_bps as u128) / BPS_DENOM) as u64
}

/// The minimum next bid that satisfies the auction increment rule:
/// ceil(previous × (10_000 + min_increment_bps) / 10_000). Ceiling division
/// so a non-zero increment always forces a real improvement (mirrors
/// `rfq::bid` / `swap_auction::bid`). Callers additionally enforce the
/// strict `> previous` (which handles min_increment_bps == 0) and the
/// reserve floor.
pub fn min_next_bid(previous: u64, min_increment_bps: u64) -> Option<u64> {
    let raw = (previous as u128)
        .checked_mul(BPS_DENOM + min_increment_bps as u128)?
        .checked_add(BPS_DENOM - 1)?
        / BPS_DENOM;
    u64::try_from(raw).ok()
}

/// Shares minted for a deposit at `pps`: floor(amount × PPS_SCALE / pps).
/// Floor favors the vault (mirrors `vault::claim_shares` / finalize mint).
pub fn shares_for_amount(amount: u64, pps: u128) -> Option<u64> {
    if pps == 0 {
        return None;
    }
    u64::try_from((amount as u128).checked_mul(PPS_SCALE)? / pps).ok()
}

/// Underlying owed for shares at `pps`: floor(shares × pps / PPS_SCALE).
/// Floor favors the vault (mirrors `vault::complete_withdraw` / finalize).
pub fn amount_for_shares(shares: u64, pps: u128) -> Option<u64> {
    u64::try_from((shares as u128).checked_mul(pps)? / PPS_SCALE).ok()
}

/// amount × spot / 10^spot_scale, round-half-up — `apply_strike`-style
/// settlement notional of an underlying amount, in u256 so a u64 amount ×
/// u128 spot cannot overflow (mirrors `vault::settlement_notional`).
pub fn settlement_notional(amount: u64, spot: u128, spot_scale: u8) -> Option<u64> {
    let divisor = U256::from(10u8).checked_pow(U256::from(spot_scale))?;
    let numerator = U256::from(amount) * U256::from(spot);
    let out = (numerator + divisor / 2) / divisor;
    u64::try_from(out).ok()
}

/// amount_s × 10^spot_scale / spot, floor — settlement valued in underlying
/// smallest-units; the conversion can only round against the holder
/// (mirrors `vault::settlement_to_underlying`).
pub fn settlement_to_underlying(amount_s: u64, spot: u128, spot_scale: u8) -> Option<u64> {
    if amount_s == 0 {
        return Some(0);
    }
    if spot == 0 {
        return None;
    }
    let mult = U256::from(10u8).checked_pow(U256::from(spot_scale))?;
    let out = U256::from(amount_s) * mult / U256::from(spot);
    u64::try_from(out).ok()
}

/// Exercised portion of a position range `[rs, re)` against the bucket's
/// exercise cursor — the FIFO assignment at the heart of the protocol
/// (mirrors the redeem logic in `bucket.move` / `put_bucket.move`).
pub fn exercised_in_range(cursor: u128, range_start: u128, range_end: u128) -> u128 {
    if cursor <= range_start {
        0
    } else if cursor >= range_end {
        range_end - range_start
    } else {
        cursor - range_start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Golden vectors copied verbatim from contracts/tests/bucket_tests.move ──

    #[test]
    fn apply_strike_scale_zero_is_plain_multiply() {
        assert_eq!(apply_strike(100, 50_000, 0), Some(5_000_000));
    }

    #[test]
    fn apply_strike_round_half_up_boundaries() {
        assert_eq!(apply_strike(1, 4, 1), Some(0));
        assert_eq!(apply_strike(1, 5, 1), Some(1));
        assert_eq!(apply_strike(1, 6, 1), Some(1));
        assert_eq!(apply_strike(1, 14, 1), Some(1));
        assert_eq!(apply_strike(1, 15, 1), Some(2));
    }

    #[test]
    fn apply_strike_tdeep_at_15_cents() {
        assert_eq!(apply_strike(1, 15_000, 5), Some(0));
        assert_eq!(apply_strike(10, 15_000, 5), Some(2));
        assert_eq!(apply_strike(100, 15_000, 5), Some(15));
        assert_eq!(apply_strike(1_000_000, 15_000, 5), Some(150_000));
    }

    // ── Golden vectors copied verbatim from put_bucket_tests.move ──

    #[test]
    fn apply_strike_ceil_floor_vectors() {
        assert_eq!(apply_strike_ceil(1, 4, 1), Some(1));
        assert_eq!(apply_strike_floor(1, 4, 1), Some(0));
        assert_eq!(apply_strike_ceil(1, 15, 1), Some(2));
        assert_eq!(apply_strike_floor(1, 15, 1), Some(1));
        assert_eq!(apply_strike_ceil(2, 5, 1), Some(1));
        assert_eq!(apply_strike_floor(2, 5, 1), Some(1));
        assert_eq!(apply_strike_ceil(100, 50_000, 0), Some(5_000_000));
        assert_eq!(apply_strike_floor(100, 50_000, 0), Some(5_000_000));
    }

    // ── pow10 bounds (bucket.move MAX_STRIKE_SCALE) ──

    #[test]
    fn pow10_bounds() {
        assert_eq!(pow10(0), Some(1));
        assert_eq!(pow10(12), Some(1_000_000_000_000));
        assert_eq!(pow10(38), Some(10u128.pow(38)));
        assert_eq!(pow10(39), None);
    }

    #[test]
    fn apply_strike_overflow_is_none() {
        // u128 multiply overflow
        assert_eq!(apply_strike(u128::MAX, 2, 0), None);
        // result exceeds u64
        assert_eq!(apply_strike(u64::MAX as u128, 2, 0), None);
    }

    // ── fee floor (bucket::skim_fee) ──

    #[test]
    fn fee_floor() {
        assert_eq!(fee_amount(10_000, 50), 50); // 0.5%
        assert_eq!(fee_amount(199, 50), 0); // floors to zero
        assert_eq!(fee_amount(0, 1000), 0);
        assert_eq!(fee_amount(u64::MAX, 10_000), u64::MAX);
    }

    // ── auction increment ceiling (rfq::bid) ──

    #[test]
    fn min_next_bid_ceils() {
        // 100 × 1.005 = 100.5 → 101 (ceiling forces a real improvement)
        assert_eq!(min_next_bid(100, 50), Some(101));
        assert_eq!(min_next_bid(100, 0), Some(100));
        assert_eq!(min_next_bid(10_000, 50), Some(10_050));
        assert_eq!(min_next_bid(1, 1), Some(2)); // 1.0001 → 2
    }

    // ── pps share math (vault.move, mirrors vault-sim::ledger) ──

    #[test]
    fn pps_floor_both_directions() {
        // pps == PPS_SCALE: identity
        assert_eq!(shares_for_amount(1_000, PPS_SCALE), Some(1_000));
        assert_eq!(amount_for_shares(1_000, PPS_SCALE), Some(1_000));
        // pps = 1.5×: deposits mint fewer shares (floor), shares withdraw
        // floor(shares × 1.5)
        let pps = PPS_SCALE * 3 / 2;
        assert_eq!(shares_for_amount(100, pps), Some(66));
        assert_eq!(amount_for_shares(67, pps), Some(100));
        assert_eq!(amount_for_shares(1, pps), Some(1)); // 1.5 floors to 1
        assert_eq!(shares_for_amount(1, pps), Some(0)); // 0.66 floors to 0
    }

    // ── vault spot conversions ──

    #[test]
    fn settlement_notional_half_up() {
        // Same shape as apply_strike vectors, u256 path.
        assert_eq!(settlement_notional(1, 4, 1), Some(0));
        assert_eq!(settlement_notional(1, 5, 1), Some(1));
        assert_eq!(settlement_notional(100, 50_000, 0), Some(5_000_000));
    }

    #[test]
    fn settlement_to_underlying_floors() {
        assert_eq!(settlement_to_underlying(0, 5, 1), Some(0));
        // 100 settlement at spot 0.4 (4/10^1) → 250 underlying exactly
        assert_eq!(settlement_to_underlying(100, 4, 1), Some(250));
        // 100 at spot 3 → 33.33 floors to 33
        assert_eq!(settlement_to_underlying(100, 3, 0), Some(33));
        assert_eq!(settlement_to_underlying(100, 0, 0), None);
    }

    // ── FIFO cursor assignment ──

    #[test]
    fn exercised_in_range_cases() {
        // cursor before range
        assert_eq!(exercised_in_range(5, 10, 20), 0);
        assert_eq!(exercised_in_range(10, 10, 20), 0); // half-open: == start
        // cursor inside range
        assert_eq!(exercised_in_range(15, 10, 20), 5);
        // cursor at/past end
        assert_eq!(exercised_in_range(20, 10, 20), 10);
        assert_eq!(exercised_in_range(100, 10, 20), 10);
    }
}
