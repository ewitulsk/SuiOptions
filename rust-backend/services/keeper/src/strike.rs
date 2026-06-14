//! Strike selection (README §5): compute the 0.10Δ target strike K*
//! from the IV estimate, then pick the **smallest in-band candidate
//! strike ≥ K*** (snap up ⇒ delta ≤ target ⇒ conservative). No
//! candidate ≥ K*? Take the highest in-band strike and flag
//! `grid_coverage_miss` — that log line feeds the scheduler-grid alert.
//!
//! Behaviorally identical to `vault_sim::strategy::StrikeSelector`'s
//! snap rule (the equivalence is what makes the Ribbon validation
//! transferable); the golden test in `tests/strike_goldens.rs` runs
//! both on shared vectors.

use pricing::{call_delta, strike_for_delta, CallInputs};
use sui_types::base_types::ObjectID;

use crate::state::VaultConfigView;

const MS_PER_YEAR: f64 = 365.0 * 86_400_000.0;

/// One live bucket from the indexer, strike decoded to the USD cross.
#[derive(Debug, Clone, PartialEq)]
pub struct BucketCandidate {
    pub bucket_id: ObjectID,
    pub call_type: String,
    pub strike_raw: u128,
    pub strike_scale: u8,
    pub expiry_ms: u64,
}

impl BucketCandidate {
    /// Chain units → USD cross: `strike_raw / 10^(scale + s_dec − u_dec)`
    /// (inverse of the scheduler's `build_z_ladder_for_pair` mapping).
    pub fn strike_usd(&self, underlying_decimals: u8, settlement_decimals: u8) -> f64 {
        let exp = self.strike_scale as i32 + settlement_decimals as i32
            - underlying_decimals as i32;
        self.strike_raw as f64 * 10f64.powi(-exp)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StrikePick {
    pub bucket_id: ObjectID,
    pub call_type: String,
    pub strike_usd: f64,
    pub expiry_ms: u64,
    /// The unsnapped delta-target strike.
    pub k_star_usd: f64,
    /// Black-Scholes delta of the snapped strike at the IV estimate.
    pub model_delta: f64,
    /// True when no candidate ≥ K* existed (doc 04 §3 GridCoverageMiss).
    pub grid_coverage_miss: bool,
}

/// Pick the round's bucket. `sigma_iv` is the IV estimate (realized σ ×
/// iv_ratio); the band/lead filters mirror what `vault::select_bucket`
/// enforces on-chain so the keeper never submits a doomed candidate.
/// Candidates at several expiries: the earliest in-window expiry wins
/// (weekly cadence), strikes compete within it.
pub fn pick_bucket(
    candidates: &[BucketCandidate],
    spot_usd_cross: f64,
    sigma_iv: f64,
    now_ms: u64,
    cfg: &VaultConfigView,
    underlying_decimals: u8,
    settlement_decimals: u8,
    target_delta: f64,
) -> Option<StrikePick> {
    let min_strike = spot_usd_cross * (1.0 + cfg.min_strike_bps_over_spot as f64 / 10_000.0);
    let max_strike = spot_usd_cross * (1.0 + cfg.max_strike_bps_over_spot as f64 / 10_000.0);

    let in_window: Vec<(&BucketCandidate, f64)> = candidates
        .iter()
        .filter(|c| {
            c.expiry_ms >= now_ms + cfg.min_expiry_lead_ms
                && c.expiry_ms <= now_ms + cfg.max_expiry_lead_ms
        })
        .map(|c| (c, c.strike_usd(underlying_decimals, settlement_decimals)))
        .filter(|(_, k)| *k >= min_strike && *k <= max_strike)
        .collect();

    let expiry = in_window.iter().map(|(c, _)| c.expiry_ms).min()?;
    let tau_years = (expiry - now_ms) as f64 / MS_PER_YEAR;
    let k_star = strike_for_delta(spot_usd_cross, sigma_iv, tau_years, 0.0, target_delta);

    let at_expiry = in_window.iter().filter(|(c, _)| c.expiry_ms == expiry);
    let snapped = at_expiry
        .clone()
        .filter(|(_, k)| *k >= k_star)
        .min_by(|(_, a), (_, b)| a.total_cmp(b));
    let (candidate, strike_usd, miss) = match snapped {
        Some((c, k)) => (*c, *k, false),
        None => {
            let (c, k) = at_expiry.max_by(|(_, a), (_, b)| a.total_cmp(b))?;
            (*c, *k, true)
        }
    };

    Some(StrikePick {
        bucket_id: candidate.bucket_id,
        call_type: candidate.call_type.clone(),
        strike_usd,
        expiry_ms: expiry,
        k_star_usd: k_star,
        model_delta: call_delta(CallInputs {
            spot: spot_usd_cross,
            strike: strike_usd,
            t_years: tau_years,
            r: 0.0,
            sigma: sigma_iv,
        }),
        grid_coverage_miss: miss,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::VaultConfigView;

    fn cfg() -> VaultConfigView {
        VaultConfigView {
            min_strike_bps_over_spot: 300,
            max_strike_bps_over_spot: 6000,
            min_expiry_lead_ms: 3 * 86_400_000,
            max_expiry_lead_ms: 9 * 86_400_000,
            max_slice_amount: u64::MAX,
            max_open_rfqs: 4,
            round_ms: 7 * 86_400_000,
            hold_premium_in_settlement: false,
            underlying_feed_id: vec![0u8; 32],
            settlement_feed_id: vec![0u8; 32],
            underlying_decimals: 9,
            settlement_decimals: 6,
        }
    }

    fn id(n: u8) -> ObjectID {
        ObjectID::from_single_byte(n)
    }

    /// SUI(9)/USDC(6): the scheduler's worked example — $3.47 spot,
    /// strike_scale 7, chain strikes [34750, 37500, 40500, 43750, 47000]
    /// ⇒ USD [3.475, 3.75, 4.05, 4.375, 4.70].
    fn sui_family(expiry_ms: u64) -> Vec<BucketCandidate> {
        [34_750u128, 37_500, 40_500, 43_750, 47_000]
            .iter()
            .enumerate()
            .map(|(i, k)| BucketCandidate {
                bucket_id: id(i as u8 + 1),
                call_type: format!("0xc::call_{i}::CALL_{i}"),
                strike_raw: *k,
                strike_scale: 7,
                expiry_ms,
            })
            .collect()
    }

    #[test]
    fn chain_units_decode_to_usd() {
        let c = &sui_family(0)[0];
        // 34750 / 10^(7 + 6 − 9) = 3.475
        assert!((c.strike_usd(9, 6) - 3.475).abs() < 1e-12);
    }

    #[test]
    fn snaps_up_to_smallest_strike_at_or_above_k_star() {
        let now = 1_700_000_000_000u64;
        let family = sui_family(now + 7 * 86_400_000);
        // σ=0.85 weekly: K* = spot × e^{(½σ² − Φ⁻¹(0.10)·σ/√τ)·τ} ≈ 4.063
        // — just past the 4.05 gridpoint, so the snap lands on 4.375.
        let pick =
            pick_bucket(&family, 3.47, 0.85, now, &cfg(), 9, 6, 0.10).unwrap();
        assert!(pick.k_star_usd > 4.05 && pick.k_star_usd < 4.375, "{pick:?}");
        assert!((pick.strike_usd - 4.375).abs() < 1e-9, "{pick:?}");
        assert!(!pick.grid_coverage_miss);
        assert!(pick.strike_usd >= pick.k_star_usd);
        // Snap-up ⇒ realized delta at most the target (+ rounding slack).
        assert!(pick.model_delta <= 0.11, "{pick:?}");
    }

    #[test]
    fn falls_back_to_highest_in_band_on_coverage_miss() {
        let now = 1_700_000_000_000u64;
        // Family built for a calm regime: every strike below K* at σ=2.0.
        let family = sui_family(now + 7 * 86_400_000);
        let pick = pick_bucket(&family, 3.47, 2.0, now, &cfg(), 9, 6, 0.10).unwrap();
        assert!(pick.grid_coverage_miss);
        assert!((pick.strike_usd - 4.70).abs() < 1e-9, "{pick:?}");
    }

    #[test]
    fn filters_band_and_expiry_window() {
        let now = 1_700_000_000_000u64;
        let mut family = sui_family(now + 7 * 86_400_000);
        // ATM strike (below the +3% floor) must never be picked even
        // when it's the only one ≥ … below K*: it's out of band.
        family[0].strike_raw = 34_700; // exactly spot ⇒ below min band
        // An expiry outside the lead window is invisible.
        family[4].expiry_ms = now + 86_400_000; // 1 day — too soon
        let pick = pick_bucket(&family, 3.47, 0.85, now, &cfg(), 9, 6, 0.10).unwrap();
        assert_ne!(pick.bucket_id, family[0].bucket_id);
        assert_ne!(pick.bucket_id, family[4].bucket_id);

        // Nothing in window at all ⇒ None.
        let stale: Vec<_> = sui_family(now + 86_400_000);
        assert!(pick_bucket(&stale, 3.47, 0.85, now, &cfg(), 9, 6, 0.10).is_none());
    }

    #[test]
    fn earliest_in_window_expiry_wins() {
        let now = 1_700_000_000_000u64;
        let near = sui_family(now + 5 * 86_400_000);
        let far = sui_family(now + 8 * 86_400_000);
        let all: Vec<_> = near.iter().cloned().chain(far.iter().cloned()).collect();
        let pick = pick_bucket(&all, 3.47, 0.85, now, &cfg(), 9, 6, 0.10).unwrap();
        assert_eq!(pick.expiry_ms, now + 5 * 86_400_000);
    }
}
