//! Strike selection — the verbatim port of the Sui keeper's `strike.rs`:
//! compute the delta-target strike K* from the IV estimate, then pick the
//! **smallest in-band candidate strike ≥ K*** (snap up ⇒ delta ≤ target ⇒
//! conservative). No candidate ≥ K*? Take the highest in-band strike and
//! flag `grid_coverage_miss` — that log line feeds the scheduler-grid
//! alert.
//!
//! The chain-units decode is identical: `strike / 10^(scale + s_dec −
//! u_dec)` — `options_math::apply_strike` maps underlying smallest-units
//! to settlement smallest-units exactly like the Move contracts did.

use pricing::{call_delta, call_price_per_unit, strike_for_delta, CallInputs};
use solana_sdk::pubkey::Pubkey;

use options_vault::state::VaultConfig;

const MS_PER_YEAR: f64 = 365.0 * 86_400_000.0;

/// One live bucket from the indexer, strike decoded to the USD cross.
#[derive(Debug, Clone, PartialEq)]
pub struct BucketCandidate {
    pub bucket: Pubkey,
    pub strike_raw: u128,
    pub strike_scale: u8,
    pub expiry_ms: u64,
}

impl BucketCandidate {
    /// Chain units → USD cross: `strike_raw / 10^(scale + s_dec − u_dec)`.
    pub fn strike_usd(&self, underlying_decimals: u8, settlement_decimals: u8) -> f64 {
        let exp = self.strike_scale as i32 + settlement_decimals as i32
            - underlying_decimals as i32;
        self.strike_raw as f64 * 10f64.powi(-exp)
    }
}

impl StrikePick {
    /// Whether the snapped strike's fair value can clear the reserve the
    /// on-chain `open_rfq` will set (`min_reserve_premium_bps` of notional,
    /// i.e. `spot × bps/1e4` per unit). The model premium is the ceiling on
    /// any plausible bid, so `false` means the auction can only expire unsold
    /// — the keeper skips the round instead of churning it.
    pub fn clears_reserve(&self, spot_usd_cross: f64, min_reserve_premium_bps: u64) -> bool {
        let reserve_per_unit = spot_usd_cross * min_reserve_premium_bps as f64 / 10_000.0;
        self.model_premium_usd > reserve_per_unit
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StrikePick {
    pub bucket: Pubkey,
    pub strike_usd: f64,
    pub expiry_ms: u64,
    /// The unsnapped delta-target strike.
    pub k_star_usd: f64,
    /// Black-Scholes delta of the snapped strike at the IV estimate.
    pub model_delta: f64,
    /// Black-Scholes per-underlying-unit call price of the snapped strike at
    /// the IV estimate, in settlement-per-underlying (USD cross) units — the
    /// ceiling on any plausible bid. Compared against the reserve to skip
    /// structurally-unsellable rounds.
    pub model_premium_usd: f64,
    /// True when no candidate ≥ K* existed (GridCoverageMiss).
    pub grid_coverage_miss: bool,
}

/// Pick the round's bucket. `sigma_iv` is the IV estimate (realized σ ×
/// iv_ratio); the band/lead filters mirror what the on-chain
/// `select_bucket` enforces so the keeper never submits a doomed
/// candidate. Candidates at several expiries: the earliest in-window
/// expiry wins (weekly cadence), strikes compete within it.
#[allow(clippy::too_many_arguments)]
pub fn pick_bucket(
    candidates: &[BucketCandidate],
    spot_usd_cross: f64,
    sigma_iv: f64,
    now_ms: u64,
    cfg: &VaultConfig,
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
        bucket: candidate.bucket,
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
        model_premium_usd: call_price_per_unit(CallInputs {
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

    fn cfg() -> VaultConfig {
        VaultConfig {
            mgmt_fee_bps_annual: 200,
            perf_fee_bps: 2_000,
            round_ms: 7 * 86_400_000,
            selling_window_ms: 12 * 3_600_000,
            min_strike_bps_over_spot: 300,
            max_strike_bps_over_spot: 6_000,
            min_expiry_lead_ms: 3 * 86_400_000,
            max_expiry_lead_ms: 9 * 86_400_000,
            min_reserve_premium_bps: 10,
            max_slice_amount: u64::MAX,
            max_open_rfqs: 4,
            rfq_duration_ms: 600_000,
            rfq_snipe_window_ms: 60_000,
            rfq_snipe_extension_ms: 120_000,
            rfq_max_extension_ms: 600_000,
            rfq_min_increment_bps: 500,
            hold_premium_in_settlement: false,
            max_swap_slippage_bps: 100,
            underlying_feed_id: [1u8; 32],
            settlement_feed_id: [2u8; 32],
            max_price_age_secs: 3_600,
            max_conf_bps: 500,
            underlying_decimals: 9,
            settlement_decimals: 6,
        }
    }

    fn id(n: u8) -> Pubkey {
        Pubkey::new_from_array([n; 32])
    }

    /// SUI(9)/USDC(6)-shaped family — $3.47 spot, strike_scale 7, chain
    /// strikes [34750, 37500, 40500, 43750, 47000] ⇒ USD
    /// [3.475, 3.75, 4.05, 4.375, 4.70]. Same worked example as the Sui
    /// twin so the golden expectations carry over.
    fn sui_family(expiry_ms: u64) -> Vec<BucketCandidate> {
        [34_750u128, 37_500, 40_500, 43_750, 47_000]
            .iter()
            .enumerate()
            .map(|(i, k)| BucketCandidate {
                bucket: id(i as u8 + 1),
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
        // σ=0.85 weekly: K* ≈ 4.063 — just past the 4.05 gridpoint, so
        // the snap lands on 4.375.
        let pick = pick_bucket(&family, 3.47, 0.85, now, &cfg(), 9, 6, 0.10).unwrap();
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
        // Calm-regime family: every strike below K* at σ=2.0.
        let family = sui_family(now + 7 * 86_400_000);
        let pick = pick_bucket(&family, 3.47, 2.0, now, &cfg(), 9, 6, 0.10).unwrap();
        assert!(pick.grid_coverage_miss);
        assert!((pick.strike_usd - 4.70).abs() < 1e-9, "{pick:?}");
    }

    #[test]
    fn filters_band_and_expiry_window() {
        let now = 1_700_000_000_000u64;
        let mut family = sui_family(now + 7 * 86_400_000);
        // ATM strike (below the +3% floor) must never be picked: out of band.
        family[0].strike_raw = 34_700; // exactly spot ⇒ below min band
        // An expiry outside the lead window is invisible.
        family[4].expiry_ms = now + 86_400_000; // 1 day — too soon
        let pick = pick_bucket(&family, 3.47, 0.85, now, &cfg(), 9, 6, 0.10).unwrap();
        assert_ne!(pick.bucket, family[0].bucket);
        assert_ne!(pick.bucket, family[4].bucket);

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

    /// Hourly tenor: the +3% floor forces a far-OTM strike whose fair value is
    /// ~0, far under the 10-bps reserve — `clears_reserve` must reject it so
    /// the keeper idles the round instead of churning unsellable auctions.
    #[test]
    fn clears_reserve_rejects_worthless_short_dated_strike() {
        let now = 1_700_000_000_000u64;
        let hourly = VaultConfig {
            min_expiry_lead_ms: 50 * 60_000,
            max_expiry_lead_ms: 90 * 60_000,
            ..cfg()
        };
        let expiry = now + 73 * 60_000;
        // spot 0.708; strikes 0.709 (below +3% band), 0.780, 0.850 (scale 8).
        let family: Vec<_> = [70_900u128, 78_000, 85_000]
            .iter()
            .enumerate()
            .map(|(i, k)| BucketCandidate {
                bucket: id(i as u8 + 1),
                strike_raw: *k,
                strike_scale: 8,
                expiry_ms: expiry,
            })
            .collect();
        let pick = pick_bucket(&family, 0.708, 0.83, now, &hourly, 9, 6, 0.20).unwrap();
        assert!((pick.strike_usd - 0.780).abs() < 1e-9, "{pick:?}");
        assert!(pick.model_premium_usd < 1e-6, "fair value ~0: {pick:?}");
        assert!(!pick.clears_reserve(0.708, 10), "{pick:?}");
    }

    /// A normal weekly ~0.10Δ strike is worth far more than the 10-bps
    /// reserve, so the round proceeds.
    #[test]
    fn clears_reserve_passes_normal_weekly_strike() {
        let now = 1_700_000_000_000u64;
        let family = sui_family(now + 7 * 86_400_000);
        let pick = pick_bucket(&family, 3.47, 0.85, now, &cfg(), 9, 6, 0.10).unwrap();
        assert!(pick.model_premium_usd > 0.0, "{pick:?}");
        assert!(pick.clears_reserve(3.47, 10), "{pick:?}");
    }

    /// `clears_reserve` compares fair value strictly against `bps × spot`.
    #[test]
    fn clears_reserve_threshold_is_bps_of_spot() {
        let now = 1_700_000_000_000u64;
        let family = sui_family(now + 7 * 86_400_000);
        let mut pick = pick_bucket(&family, 3.47, 0.85, now, &cfg(), 9, 6, 0.10).unwrap();
        // reserve at spot 3.47, 10 bps = 0.00347.
        pick.model_premium_usd = 0.004;
        assert!(pick.clears_reserve(3.47, 10));
        pick.model_premium_usd = 0.003;
        assert!(!pick.clears_reserve(3.47, 10));
    }
}
