//! Venue equity sources for the trading-vault equity-poster crank
//! (SO-299): where the keeper learns what an external account is worth
//! before stepping the on-chain `EquityBook` entry toward it.
//!
//! The keeper's wallet must be an admin-allowlisted poster
//! (`equity_oracle::add_poster`) — a denied post aborts E_NOT_POSTER (1)
//! and is classified as retry (alerting), not benign.
//!
//! Shipped impls are `Disabled` (no posting) and `Fixed` (a per-vault
//! target map from keeper config `[external.equity_posts]` — an
//! operator/testing source). Real venue readers (Bluefin account equity,
//! DeepBook-Margin manager equity) are follow-ups and plug in behind the
//! same trait.

use std::collections::BTreeMap;

use sui_types::base_types::{ObjectID, SuiAddress};

/// Answers "what is this vault's external account worth right now?", in
/// deposit-asset units. `None` ⇒ no opinion, the keeper posts nothing.
pub trait VenueEquitySource: Send + Sync {
    fn equity_for(&self, vault_id: ObjectID, external_account: SuiAddress) -> Option<u64>;
}

/// Never posts.
pub struct Disabled;

impl VenueEquitySource for Disabled {
    fn equity_for(&self, _vault_id: ObjectID, _external_account: SuiAddress) -> Option<u64> {
        None
    }
}

/// Fixed per-vault targets from keeper config (`[external.equity_posts]`).
pub struct Fixed {
    targets: BTreeMap<ObjectID, u64>,
}

impl Fixed {
    pub fn new(targets: BTreeMap<ObjectID, u64>) -> Self {
        Self { targets }
    }
}

impl VenueEquitySource for Fixed {
    fn equity_for(&self, vault_id: ObjectID, _external_account: SuiAddress) -> Option<u64> {
        self.targets.get(&vault_id).copied()
    }
}

/// One guardrail-respecting step from `previous` toward `target`: the
/// on-chain `post_equity` aborts (E_DELTA_TOO_LARGE) when
/// `delta * 10_000 > previous * max_delta_bps`, so the step is capped at
/// `floor(previous * max_delta_bps / 10_000)` and never overshoots the
/// target. A `previous` of zero is immovable (bps-of-zero) — callers
/// must skip and surface that admin `seed_equity` is required.
pub fn clamp_step(previous: u64, target: u64, max_delta_bps: u64) -> u64 {
    let max_delta = ((previous as u128) * (max_delta_bps as u128) / 10_000) as u64;
    if target > previous {
        target.min(previous.saturating_add(max_delta))
    } else {
        target.max(previous.saturating_sub(max_delta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_step_bounds_each_direction() {
        // 20% cap: 1_000_000 may move at most ±200_000 per step.
        assert_eq!(clamp_step(1_000_000, 1_100_000, 2_000), 1_100_000); // within cap
        assert_eq!(clamp_step(1_000_000, 2_000_000, 2_000), 1_200_000); // capped up
        assert_eq!(clamp_step(1_000_000, 100, 2_000), 800_000); // capped down
        assert_eq!(clamp_step(1_000_000, 900_000, 2_000), 900_000); // within cap down
        assert_eq!(clamp_step(1_000_000, 1_000_000, 2_000), 1_000_000); // no-op
    }

    #[test]
    fn clamp_step_never_violates_the_onchain_guardrail() {
        // floor() rounding: the clamped delta always satisfies
        // delta * 10_000 <= previous * max_delta_bps.
        for (previous, target, bps) in [
            (3u64, 100u64, 2_500u64), // floor(3*2500/10000) = 0
            (7, 0, 3_333),
            (999_999, u64::MAX, 1),
            (u64::MAX / 2, u64::MAX, 10_000),
        ] {
            let stepped = clamp_step(previous, target, bps);
            let delta = stepped.abs_diff(previous);
            assert!(
                (delta as u128) * 10_000 <= (previous as u128) * (bps as u128),
                "guardrail violated: prev={previous} target={target} bps={bps} stepped={stepped}"
            );
        }
    }

    #[test]
    fn clamp_step_zero_previous_is_immovable() {
        assert_eq!(clamp_step(0, 5_000, 2_000), 0);
    }

    #[test]
    fn fixed_source_answers_only_mapped_vaults() {
        let vault = ObjectID::from_hex_literal("0xabc").unwrap();
        let other = ObjectID::from_hex_literal("0xdef").unwrap();
        let src = Fixed::new([(vault, 42u64)].into_iter().collect());
        let acct = SuiAddress::ZERO;
        assert_eq!(src.equity_for(vault, acct), Some(42));
        assert_eq!(src.equity_for(other, acct), None);
        assert_eq!(Disabled.equity_for(vault, acct), None);
    }
}
