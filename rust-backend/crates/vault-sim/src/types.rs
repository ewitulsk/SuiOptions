//! Chain-unit newtypes and per-round records.
//!
//! Amounts are in chain *smallest units* (e.g. MIST for SUI, 1e-8 for wBTC,
//! 1e-6 for USDC). The newtypes exist so underlying- and settlement-
//! denominated quantities cannot be mixed silently — the same bug class the
//! `Bucket`'s phantom type parameters prevent on-chain.

use std::ops::{Add, AddAssign, Sub, SubAssign};

use serde::{Deserialize, Serialize};

/// Price-per-share fixed-point scale (doc 03 §4): pps of `PPS_SCALE` means
/// 1 share == 1 underlying smallest-unit.
pub const PPS_SCALE: u128 = 1_000_000_000_000;

macro_rules! amount_newtype {
    ($(#[$doc:meta])* $name:ident, $inner:ty) => {
        $(#[$doc])*
        #[derive(
            Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord,
            Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub $inner);

        impl $name {
            pub const ZERO: Self = Self(0);

            pub fn get(self) -> $inner {
                self.0
            }
        }

        impl Add for $name {
            type Output = Self;
            fn add(self, rhs: Self) -> Self {
                Self(self.0.checked_add(rhs.0).expect("amount overflow"))
            }
        }

        impl Sub for $name {
            type Output = Self;
            fn sub(self, rhs: Self) -> Self {
                Self(self.0.checked_sub(rhs.0).expect("amount underflow"))
            }
        }

        impl AddAssign for $name {
            fn add_assign(&mut self, rhs: Self) {
                *self = *self + rhs;
            }
        }

        impl SubAssign for $name {
            fn sub_assign(&mut self, rhs: Self) {
                *self = *self - rhs;
            }
        }

        impl std::iter::Sum for $name {
            fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
                iter.fold(Self::ZERO, |acc, x| acc + x)
            }
        }
    };
}

amount_newtype!(
    /// Underlying smallest-units (SUI MIST, wBTC sats, …).
    UnderlyingAmt,
    u64
);

amount_newtype!(
    /// Settlement smallest-units (USDC 1e-6, …).
    SettleAmt,
    u64
);

amount_newtype!(
    /// Vault share smallest-units (share coin decimals == underlying
    /// decimals, doc 03 §3).
    ShareAmt,
    u64
);

/// Price per share in `PPS_SCALE` fixed point.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Pps(pub u128);

impl Pps {
    /// Genesis price: 1 share per underlying smallest-unit.
    pub const ONE: Self = Self(PPS_SCALE);

    pub fn get(self) -> u128 {
        self.0
    }

    /// Underlying owed for `shares` at this price: floor(shares × pps /
    /// PPS_SCALE). Floor matches `complete_withdraw` (doc 03 §5.4).
    pub fn shares_to_underlying(self, shares: ShareAmt) -> UnderlyingAmt {
        UnderlyingAmt(((shares.0 as u128 * self.0) / PPS_SCALE) as u64)
    }

    /// Shares minted for `amount` at this price: floor(amount × PPS_SCALE
    /// / pps). Floor matches `claim_shares` (doc 03 §5.2) — dust favors
    /// the vault.
    pub fn underlying_to_shares(self, amount: UnderlyingAmt) -> ShareAmt {
        ShareAmt(((amount.0 as u128 * PPS_SCALE) / self.0) as u64)
    }
}

/// One row per simulated round (doc 06 §6). Pricing context in f64,
/// accounting outcomes in chain units.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoundRecord {
    pub round: u64,
    /// Spot at roll (settlement units per underlying unit, f64 scale-0).
    pub spot_0: f64,
    /// Spot at expiry.
    pub spot_t: f64,
    /// Strike actually written (scale-0 ratio), 0 if nothing was sold.
    pub strike: f64,
    /// Annualized IV used to price the round's premium.
    pub sigma_iv: f64,
    /// Net premium collected over the round, settlement units.
    pub premium: SettleAmt,
    /// Exercised fraction of the vault's written range, [0, 1].
    pub phi: f64,
    /// Underlying written into the round's bucket.
    pub written: UnderlyingAmt,
    /// Underlying offered but never sold (failed slices).
    pub unsold: UnderlyingAmt,
    pub mgmt_fee: UnderlyingAmt,
    pub perf_fee: UnderlyingAmt,
    /// pps locked at this round's finalize.
    pub pps: Pps,
    /// Deployable AUM at finalize (pre-fee, pre-queue).
    pub aum: UnderlyingAmt,
    /// Live shares (supply + queued) the round's P&L accrued to.
    pub shares: ShareAmt,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pps_conversions_floor() {
        // pps = 1.05 × scale: 100 underlying → floor(100/1.05) = 95 shares.
        let pps = Pps(PPS_SCALE / 100 * 105);
        assert_eq!(pps.underlying_to_shares(UnderlyingAmt(100)), ShareAmt(95));
        // 95 shares back → floor(95 × 1.05) = 99 underlying: floor dust
        // stays with the vault on the round trip.
        assert_eq!(pps.shares_to_underlying(ShareAmt(95)), UnderlyingAmt(99));
    }

    #[test]
    fn pps_one_is_identity() {
        assert_eq!(
            Pps::ONE.underlying_to_shares(UnderlyingAmt(12_345)),
            ShareAmt(12_345)
        );
        assert_eq!(
            Pps::ONE.shares_to_underlying(ShareAmt(12_345)),
            UnderlyingAmt(12_345)
        );
    }

    #[test]
    #[should_panic(expected = "amount underflow")]
    fn amount_subtraction_underflow_panics() {
        let _ = UnderlyingAmt(1) - UnderlyingAmt(2);
    }
}
