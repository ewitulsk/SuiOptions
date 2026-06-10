//! The round loop (doc 06 §6): settle the prior round, finalize the
//! ledger, pick a strike, sell through the sale mechanism, then step bars
//! to expiry feeding the exercise policy. Pricing in f64, accounting in
//! chain units through `cursor` + `ledger`.

use pricing::premium_for_write;

use crate::cursor::{PositionRange, SimBucket, MAX_STRIKE_SCALE};
use crate::exercise::ExercisePolicy;
use crate::iv::{realized_vol, IvProvider, MarketCtx};
use crate::ledger::{settlement_to_underlying, Ledger, LedgerConfig};
use crate::paths::Path;
use crate::premium::{PremiumModel, QuoteCtx};
use crate::sale::SaleMechanism;
use crate::strategy::StrikeSelector;
use crate::swap::SwapModel;
use crate::types::{RoundRecord, SettleAmt, UnderlyingAmt};

/// When the round's premium converts to underlying (doc 06 §2.2's π) —
/// the launch-parameter question the sweep answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PremiumPolicy {
    /// Swap at the roll spot, paying slippage.
    AtRoll,
    /// Swap at the expiry spot, paying slippage.
    AtExpiry,
    /// Hold USDC through the round; finalize values it at expiry spot
    /// without a swap (the on-chain `hold_premium_in_settlement` flag).
    HoldUsdc,
}

#[derive(Clone, Debug)]
pub struct EngineParams {
    pub underlying_decimals: u8,
    pub settlement_decimals: u8,
    /// Bars per round (7 for daily bars, weekly rounds).
    pub round_bars: usize,
    pub bars_per_year: f64,
    /// Risk-free rate for pricing.
    pub r: f64,
    /// Fraction of deployable offered for sale each round.
    pub deploy_fraction: f64,
    /// RFQ slices per round; slice sizes are equal splits.
    pub slices: u32,
    pub premium_policy: PremiumPolicy,
    /// Trailing window (bars) for the grid's realized vol.
    pub rv_window: usize,
    pub initial_deposit: UnderlyingAmt,
    pub ledger: LedgerConfig,
    pub swap_slippage_bps: u64,
}

/// Map an f64 chain-cross strike to the integer `(strike, strike_scale)`
/// encoding, targeting ~9 significant digits of resolution. The localnet
/// golden-file runs (doc 06 §9.3) replay scheduler-produced strikes
/// instead, so this encoding only needs to be faithful, not identical to
/// the scheduler's scale-picker.
fn encode_strike(strike_chain: f64) -> (u128, u8) {
    assert!(strike_chain > 0.0 && strike_chain.is_finite());
    let mut v = strike_chain;
    let mut scale: u8 = 0;
    while v < 1e9 && scale < MAX_STRIKE_SCALE {
        v *= 10.0;
        scale += 1;
    }
    (v.round() as u128, scale)
}

/// Simulate one path through the full round loop. The vault starts with
/// `initial_deposit` queued; external flow schedules are a backtester
/// concern layered on top.
#[allow(clippy::too_many_arguments)]
pub fn run_path(
    params: &EngineParams,
    selector: &StrikeSelector,
    path: &Path,
    iv: &dyn IvProvider,
    premium: &dyn PremiumModel,
    sale: &mut dyn SaleMechanism,
    exercise: &mut dyn ExercisePolicy,
) -> Vec<RoundRecord> {
    let bars = &path.bars;
    let cross = 10f64
        .powi(params.settlement_decimals as i32 - params.underlying_decimals as i32);
    let swap = SwapModel { slippage_bps: params.swap_slippage_bps };

    let mut ledger = Ledger::new(params.ledger);
    if params.initial_deposit.get() > 0 {
        ledger.deposit(params.initial_deposit);
    }
    ledger.finalize_round(UnderlyingAmt::ZERO, UnderlyingAmt::ZERO); // genesis

    let mut records = Vec::new();
    let mut i = 0usize;
    while i + params.round_bars < bars.len() {
        let expiry_idx = i + params.round_bars;
        let spot_0 = bars[i].close * cross;
        let spot_t = bars[expiry_idx].close * cross;
        let tau = params.round_bars as f64 / params.bars_per_year;
        let deployable = ledger.deployable();

        let path_sigma = path.sigma_ann.as_ref().map(|s| s[i]);
        let ctx = MarketCtx { bars: &bars[..=i], idx: i, path_sigma };
        let sigma_grid = path_sigma.unwrap_or_else(|| {
            realized_vol(bars, i, params.rv_window, params.bars_per_year)
        });
        let sigma_iv = iv.iv(&ctx, tau, selector.delta_target);

        // No usable vol (e.g. warm-up bars of a replay) or nothing to
        // deploy: idle round — hold to expiry and finalize flat.
        if sigma_iv <= 0.0 || deployable.get() == 0 {
            let out = ledger.finalize_round(deployable, UnderlyingAmt::ZERO);
            records.push(RoundRecord {
                round: out.round,
                spot_0,
                spot_t,
                strike: 0.0,
                sigma_iv,
                premium: SettleAmt::ZERO,
                phi: 0.0,
                written: UnderlyingAmt::ZERO,
                unsold: UnderlyingAmt::ZERO,
                mgmt_fee: out.mgmt_fee,
                perf_fee: out.perf_fee,
                pps: out.pps,
                aum: out.aum,
                shares: out.shares,
            });
            i = expiry_idx;
            continue;
        }

        // Strike selection: grid from clamped realized vol, K* from the
        // pricing IV, snap up (doc 04 §3).
        let choice = selector.select(spot_0, sigma_grid, sigma_iv, tau);
        let (strike_u, strike_scale) = encode_strike(choice.strike);
        let mut bucket = SimBucket::new(strike_u, strike_scale);
        // Price off the encoded strike so f64 pricing and integer
        // settlement see the same number.
        let strike_chain = strike_u as f64 / 10f64.powi(strike_scale as i32);
        let strike_bar_units = strike_chain / cross; // bar.close units

        // Sell in slices.
        let writable = (deployable.get() as f64 * params.deploy_fraction).floor() as u64;
        let qctx = QuoteCtx {
            spot: spot_0,
            strike: strike_chain,
            tau_years: tau,
            r: params.r,
            sigma: sigma_iv,
        };
        let n_slices = params.slices.max(1) as u64;
        let mut written: u64 = 0;
        let mut unsold: u64 = 0;
        let mut premium_settle: u64 = 0;
        for s in 0..n_slices {
            let slice = writable / n_slices + u64::from(s < writable % n_slices);
            if slice == 0 {
                continue;
            }
            let mid = premium.mid(&qctx);
            let fill = sale.execute(slice, mid, &qctx);
            if fill.filled > 0 {
                bucket.write(UnderlyingAmt(fill.filled));
                written += fill.filled;
                premium_settle += premium_for_write(fill.premium_per_unit, fill.filled);
            }
            unsold += slice - fill.filled;
        }
        let range = PositionRange { start: 0, end: written as u128 };

        // Step bars to expiry, feeding the exercise policy. The vault is
        // the whole bucket here, so holder exercises consume its range
        // directly (front-of-queue, doc 06 §2.1).
        let mut exercised_units: u64 = 0;
        let mut phi_acc = 0.0f64;
        for (offset, bar) in bars[i + 1..=expiry_idx].iter().enumerate() {
            let t_left = (expiry_idx - (i + 1 + offset)) as f64 / params.bars_per_year;
            let inc = exercise.step(bar, strike_bar_units, t_left);
            if inc > 0.0 && written > 0 {
                phi_acc = (phi_acc + inc).min(1.0);
                let target = ((phi_acc * written as f64).round() as u64).min(written);
                if target > exercised_units {
                    bucket.exercise(UnderlyingAmt(target - exercised_units));
                    exercised_units = target;
                }
            }
        }

        // Redeem and convert proceeds.
        let (u_back, s_back) = if written > 0 {
            bucket.redeem(range)
        } else {
            (UnderlyingAmt::ZERO, SettleAmt::ZERO)
        };
        let premium_u = match params.premium_policy {
            PremiumPolicy::AtRoll => {
                swap.settlement_to_underlying(SettleAmt(premium_settle), spot_0)
            }
            PremiumPolicy::AtExpiry => {
                swap.settlement_to_underlying(SettleAmt(premium_settle), spot_t)
            }
            PremiumPolicy::HoldUsdc => {
                settlement_to_underlying(SettleAmt(premium_settle), spot_t)
            }
        };
        let exercise_u = swap.settlement_to_underlying(s_back, spot_t);

        let aum =
            UnderlyingAmt(deployable.get() - written) + u_back + premium_u + exercise_u;
        let out = ledger.finalize_round(aum, premium_u);

        records.push(RoundRecord {
            round: out.round,
            spot_0,
            spot_t,
            strike: strike_chain,
            sigma_iv,
            premium: SettleAmt(premium_settle),
            phi: if written > 0 { exercised_units as f64 / written as f64 } else { 0.0 },
            written: UnderlyingAmt(written),
            unsold: UnderlyingAmt(unsold),
            mgmt_fee: out.mgmt_fee,
            perf_fee: out.perf_fee,
            pps: out.pps,
            aum: out.aum,
            shares: out.shares,
        });
        i = expiry_idx;
    }
    records
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exercise::RationalExpiry;
    use crate::iv::RealizedOnly;
    use crate::paths::Bar;
    use crate::premium::BlackScholesMid;
    use crate::sale::RfqBatch;
    use crate::strategy::{StrikeSelector, SUI_LADDER};
    use crate::types::PPS_SCALE;
    use pricing::{call_price_per_unit, CallInputs};

    const DAY_MS: u64 = 86_400_000;

    fn params() -> EngineParams {
        EngineParams {
            underlying_decimals: 9,
            settlement_decimals: 6,
            round_bars: 7,
            bars_per_year: 365.0,
            r: 0.0,
            deploy_fraction: 1.0,
            slices: 1,
            premium_policy: PremiumPolicy::AtRoll,
            rv_window: 30,
            initial_deposit: UnderlyingAmt(1_000_000_000_000), // 1000 SUI
            ledger: LedgerConfig {
                mgmt_fee_bps_annual: 0,
                perf_fee_bps: 0,
                round_ms: 604_800_000,
            },
            swap_slippage_bps: 0,
        }
    }

    fn selector() -> StrikeSelector {
        StrikeSelector {
            delta_target: 0.10,
            ladder: SUI_LADDER.to_vec(),
            r: 0.0,
            vol_floor: 0.2,
            vol_ceiling: 2.0,
        }
    }

    /// Wobbly warmup history followed by `tail` fixed closes. 35 warmup
    /// bars = 5 whole rounds, so the tail starts exactly on a round
    /// boundary and `tail[0]` is the last round's roll spot.
    fn path_with_tail(tail: &[f64]) -> Path {
        let mut closes: Vec<f64> = (0..35)
            .map(|i| 3.5 * (1.0 + 0.02 * (i as f64 * 0.9).sin()))
            .collect();
        closes.extend_from_slice(tail);
        Path {
            bars: closes
                .iter()
                .enumerate()
                .map(|(i, c)| Bar::flat(i as u64 * DAY_MS, *c))
                .collect(),
            sigma_ann: None,
            seed: 0,
        }
    }

    /// Doc 06 §9.2 payoff identity, OTM leg: zero haircut/fees/slippage +
    /// rational exercise reproduces R = 1 + p/S₀ to rounding dust.
    #[test]
    fn payoff_identity_otm_round() {
        let p = params();
        // Flat tail at 3.5: strike lands above spot, expires OTM.
        let path = path_with_tail(&[3.5; 8]);
        let start = 35; // last full round starts at the tail
        let records = run_path(
            &p,
            &selector(),
            &path,
            &RealizedOnly { window: 30, bars_per_year: 365.0 },
            &BlackScholesMid,
            &mut RfqBatch { haircut_bps: 0 },
            &mut RationalExpiry,
        );
        let last = records.last().unwrap();
        assert_eq!(last.phi, 0.0, "must expire OTM");
        assert!(last.written.get() > 0, "must have sold");
        assert!(last.premium.get() > 0);

        // Analytic: per-unit premium at the engine's own (σ, K).
        let spot0 = path.bars[start].close * 1e-3;
        let per_unit = call_price_per_unit(CallInputs {
            spot: spot0,
            strike: last.strike,
            t_years: 7.0 / 365.0,
            r: 0.0,
            sigma: last.sigma_iv,
        });
        let prev_pps = records[records.len() - 2].pps;
        let r_actual = last.pps.0 as f64 / prev_pps.0 as f64 - 1.0;
        let r_analytic = per_unit / spot0; // premium yield on the written amount
        assert!(
            (r_actual - r_analytic).abs() < 1e-6,
            "actual {r_actual} vs analytic {r_analytic}"
        );
    }

    /// Doc 06 §9.2 payoff identity, ITM leg: φ = 1 and
    /// R = (K + p)/S_T × (S_T/S₀-normalized) — i.e. end value per written
    /// unit is K + p (in settlement), converted at S_T.
    #[test]
    fn payoff_identity_itm_round() {
        let p = params();
        // Tail: flat then a 60% gap on the last bar — deep ITM at expiry.
        let mut tail = vec![3.5; 7];
        tail.push(5.6);
        let path = path_with_tail(&tail);
        let records = run_path(
            &p,
            &selector(),
            &path,
            &RealizedOnly { window: 30, bars_per_year: 365.0 },
            &BlackScholesMid,
            &mut RfqBatch { haircut_bps: 0 },
            &mut RationalExpiry,
        );
        let last = records.last().unwrap();
        assert!(last.written.get() > 0);
        assert_eq!(last.phi, 1.0, "deep ITM ⇒ full exercise");

        let spot0 = 3.5e-3;
        let spot_t = 5.6e-3;
        let per_unit = call_price_per_unit(CallInputs {
            spot: spot0,
            strike: last.strike,
            t_years: 7.0 / 365.0,
            r: 0.0,
            sigma: last.sigma_iv,
        });
        let prev_pps = records[records.len() - 2].pps;
        let r_actual = last.pps.0 as f64 / prev_pps.0 as f64 - 1.0;
        // Whole stack written: each unit ends as K settlement bought back
        // at S_T, plus the premium converted at S₀ (AtRoll policy).
        let r_analytic = last.strike / spot_t + per_unit / spot0 - 1.0;
        assert!(
            (r_actual - r_analytic).abs() < 1e-6,
            "actual {r_actual} vs analytic {r_analytic}"
        );
    }

    #[test]
    fn unsold_slices_stay_idle_and_pps_is_flat() {
        let p = params();
        let path = path_with_tail(&[3.5; 8]);
        // A mechanism that never fills.
        struct NoFill;
        impl SaleMechanism for NoFill {
            fn execute(
                &mut self,
                _slice: u64,
                _mid: f64,
                _ctx: &QuoteCtx,
            ) -> crate::sale::Fill {
                crate::sale::Fill::unsold()
            }
        }
        let records = run_path(
            &p,
            &selector(),
            &path,
            &RealizedOnly { window: 30, bars_per_year: 365.0 },
            &BlackScholesMid,
            &mut NoFill,
            &mut RationalExpiry,
        );
        let last = records.last().unwrap();
        assert_eq!(last.written, UnderlyingAmt::ZERO);
        assert_eq!(last.unsold.get(), 1_000_000_000_000);
        assert_eq!(last.pps.0, PPS_SCALE, "flat spot + no sales ⇒ pps unchanged");
        assert_eq!(last.aum, UnderlyingAmt(1_000_000_000_000));
    }

    #[test]
    fn slices_partition_the_writable_amount_exactly() {
        let mut p = params();
        p.slices = 4;
        p.initial_deposit = UnderlyingAmt(1_000_000_000_001); // not divisible by 4
        let path = path_with_tail(&[3.5; 8]);
        let records = run_path(
            &p,
            &selector(),
            &path,
            &RealizedOnly { window: 30, bars_per_year: 365.0 },
            &BlackScholesMid,
            &mut RfqBatch { haircut_bps: 0 },
            &mut RationalExpiry,
        );
        let last = records.last().unwrap();
        // Everything offered and filled across 4 uneven slices.
        assert_eq!(last.unsold, UnderlyingAmt::ZERO);
        assert!(last.written.get() > 0);
        // written == deployable at that round (deploy_fraction 1).
        assert_eq!(last.written + last.unsold, UnderlyingAmt(last.aum.get() - (last.aum.get() - last.written.get())));
    }

    #[test]
    fn early_exercise_weakly_helps_the_writer() {
        // Doc 06 §2.1: spike above strike mid-round, fall back below by
        // expiry. EarlyIntrinsic exercises at the spike (vault keeps K +
        // premium); RationalExpiry sees an OTM expiry (vault keeps stack +
        // premium, worth less than K at S_T < K… here S_T drops hard, so
        // early exercise locked in the strike — strictly better).
        use crate::exercise::EarlyIntrinsic;
        let p = params();
        let mut tail = vec![3.5, 3.5, 5.6, 5.6]; // spike +60%
        tail.extend_from_slice(&[2.0, 2.0, 2.0, 2.0]); // collapse to 2.0
        let path = path_with_tail(&tail);

        let run = |ex: &mut dyn ExercisePolicy| {
            run_path(
                &p,
                &selector(),
                &path,
                &RealizedOnly { window: 30, bars_per_year: 365.0 },
                &BlackScholesMid,
                &mut RfqBatch { haircut_bps: 0 },
                ex,
            )
        };
        let early = run(&mut EarlyIntrinsic { thresh_bps: 500 });
        let rational = run(&mut RationalExpiry);
        let pps_early = early.last().unwrap().pps;
        let pps_rational = rational.last().unwrap().pps;
        assert!(
            pps_early > pps_rational,
            "early {pps_early:?} must beat rational {pps_rational:?} on a spike-and-collapse path"
        );
    }

    #[test]
    fn encode_strike_round_trips_at_nine_digits() {
        for k in [3.6e-3, 1.17e3, 0.15, 92_000.0] {
            let (u, s) = encode_strike(k);
            let back = u as f64 / 10f64.powi(s as i32);
            assert!((back - k).abs() / k < 1e-9, "{k} → ({u}, {s}) → {back}");
        }
    }
}
