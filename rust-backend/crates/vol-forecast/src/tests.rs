//! Gate tests for doc 09 §2.5 on synthetic paths (`synthetic.rs`).

use super::*;
use crate::synthetic::{add_microstructure_noise, inject_return, sv_jump_path, SvJumpParams};

const DAY: u64 = MS_PER_DAY;

fn base_params() -> SvJumpParams {
    SvJumpParams {
        days: 900,
        interval_ms: 300_000,
        jumps_per_day: 0.03,
        jump_size: 0.06,
        ..Default::default()
    }
}

/// One out-of-sample evaluation point.
struct Eval {
    sigma_fc: f64,
    q30: f64,
    realized: f64,
    trailing: f64,
}

/// Walk forward over the last `oos_days` of `history`: refit every
/// `refit_days`, forecast daily, score against the realized vol over the
/// next `h` days. Also returns the trailing-`h`-day RV baseline.
fn walk_forward(
    history: &[(u64, f64)],
    cfg: &ForecastConfig,
    h: usize,
    oos_days: usize,
    refit_days: usize,
) -> Vec<Eval> {
    let end = history.last().unwrap().0;
    let horizon = Horizon::from_days(h as f64);
    let mut out = Vec::new();
    let mut cal: Option<Calibration> = None;
    let mut since_fit = refit_days;
    for d in (h..=oos_days).rev() {
        let origin = end - d as u64 * DAY;
        let n = history.partition_point(|s| s.0 <= origin);
        let input = ForecastInput {
            asset: "SYN",
            history: &history[..n],
        };
        if since_fit >= refit_days || cal.is_none() {
            cal = Some(fit(cfg, &input, horizon));
            since_fit = 0;
        }
        since_fit += 1;
        let c = cal.as_ref().unwrap();
        let fc = forecast(c, &input, origin);
        assert!(fc.is_usable() && fc.calibrated, "{fc:?}");
        let interval = c.sample_interval_ms;
        out.push(Eval {
            sigma_fc: fc.sigma_mean,
            q30: fc.quantile(0.30),
            realized: realized_vol_between(history, origin, origin + h as u64 * DAY, interval),
            trailing: realized_vol_between(history, origin - h as u64 * DAY, origin, interval),
        });
    }
    out
}

fn qlike(fc: f64, real: f64) -> f64 {
    let r = (real * real) / (fc * fc);
    r - r.ln() - 1.0
}

#[test]
fn gate1_forecast_bias_is_near_zero_out_of_sample() {
    // Bias = mean(realized − forecast) over each option's life, pooled
    // across seeds so the gate measures systematic bias rather than one
    // path's vol regime (a 21-day horizon has ~14 independent windows
    // per 300 out-of-sample days).
    for h in [7usize, 21] {
        let mut bias = 0.0;
        let mut level = 0.0;
        let mut n = 0usize;
        for seed in [11u64, 23, 31] {
            let path = sv_jump_path(seed, &base_params());
            let ev = walk_forward(&path.history, &ForecastConfig::default(), h, 300, 30);
            bias += ev.iter().map(|e| e.realized - e.sigma_fc).sum::<f64>();
            level += ev.iter().map(|e| e.realized).sum::<f64>();
            n += ev.len();
        }
        let (bias, level) = (bias / n as f64, level / n as f64);
        assert!(
            bias.abs() < 0.04 * level,
            "h={h} bias {bias} on level {level}"
        );
    }
}

#[test]
fn gate2_thirtieth_percentile_is_exceeded_about_seventy_percent_of_the_time() {
    let path = sv_jump_path(23, &base_params());
    let ev = walk_forward(&path.history, &ForecastConfig::default(), 7, 450, 30);
    let hit = ev.iter().filter(|e| e.realized > e.q30).count() as f64 / ev.len() as f64;
    assert!((0.58..=0.82).contains(&hit), "hit rate {hit}");
}

#[test]
fn gate3_qlike_beats_trailing_rv_on_every_fold() {
    let path = sv_jump_path(5, &base_params());
    let ev = walk_forward(&path.history, &ForecastConfig::default(), 7, 450, 30);
    let folds: Vec<&[Eval]> = ev.chunks(150).filter(|c| c.len() >= 75).collect();
    assert!(folds.len() >= 3);
    for (i, fold) in folds.iter().enumerate() {
        let har = fold
            .iter()
            .map(|e| qlike(e.sigma_fc, e.realized))
            .sum::<f64>()
            / fold.len() as f64;
        let naive = fold
            .iter()
            .map(|e| qlike(e.trailing, e.realized))
            .sum::<f64>()
            / fold.len() as f64;
        assert!(har < naive, "fold {i}: HAR QLIKE {har} vs trailing {naive}");
    }
}

#[test]
fn gate4_post_shock_forecast_reverts_faster_than_a_24h_window() {
    // 2025-10-10: a −0.55 one-minute log return, in the data, stays in the
    // data. 1-minute cadence so the wick is one grid return.
    let p = SvJumpParams {
        days: 130,
        interval_ms: 60_000,
        jumps_per_day: 0.03,
        jump_size: 0.05,
        ..Default::default()
    };
    let path = sv_jump_path(3, &p);
    let end = path.end_ms();
    let shock = end - 10 * DAY + 12 * 3_600_000;
    let mut hist = path.history.clone();
    inject_return(&mut hist, shock, -0.55);
    let cfg = ForecastConfig::default();
    let horizon = Horizon::from_days(7.0);

    // Calibrate the day before the shock, as the live desk would have.
    let n_pre = hist.partition_point(|s| s.0 <= shock - DAY);
    let cal = fit(
        &cfg,
        &ForecastInput {
            asset: "SYN",
            history: &hist[..n_pre],
        },
        horizon,
    );
    assert!(cal.fitted);
    let at = |t: u64| {
        let n = hist.partition_point(|s| s.0 <= t);
        let input = ForecastInput {
            asset: "SYN",
            history: &hist[..n],
        };
        let fc = forecast(&cal, &input, t);
        let trailing_24h = realized_vol_between(&hist[..n], t - DAY, t, cal.sample_interval_ms);
        (fc, trailing_24h)
    };
    let (pre, _) = at(shock - 3_600_000);
    let (f1, t1) = at(shock + 3_600_000);
    let (f8, t8) = at(shock + 8 * 3_600_000);
    let (f16, _) = at(shock + 16 * 3_600_000);
    let (f23, t23) = at(shock + 23 * 3_600_000);

    // The wick is detected and labels the regime, then the label clears
    // well inside 24h.
    assert_eq!(f1.regime, Regime::PostShock, "{f1:?}");
    assert_eq!(f8.regime, Regime::PostShock, "{f8:?}");
    assert_ne!(f23.regime, Regime::PostShock, "{f23:?}");

    // A 24h trailing window prices the wick for a full day (σ ≈ 10);
    // the forecast never does.
    assert!(t1 > 8.0 && t23 > 8.0, "trailing {t1} {t8} {t23}");
    for (f, t) in [(&f1, t1), (&f8, t8), (&f23, t23)] {
        assert!(
            f.sigma_mean < 0.5 * t,
            "forecast {} vs trailing {t}",
            f.sigma_mean
        );
    }
    // The forecast's excess over its pre-shock level decays within hours.
    let ex1 = f1.sigma_mean - pre.sigma_mean;
    let ex16 = f16.sigma_mean - pre.sigma_mean;
    let ex23 = f23.sigma_mean - pre.sigma_mean;
    assert!(
        ex16 <= 0.5 * ex1.max(0.0) + 0.05,
        "excess +1h {ex1} +16h {ex16}"
    );
    assert!(
        ex23 <= 0.35 * ex1.max(0.0) + 0.05,
        "excess +1h {ex1} +23h {ex23}"
    );
    // The trailing window's excess has not decayed at all.
    assert!(t23 > 0.9 * t1);
    // The jump is charged explicitly, not smeared into the continuous leg.
    assert!(f1.sigma_continuous < 3.0 * pre.sigma_continuous, "{f1:?}");
}

#[test]
fn gate5_derived_interval_reproduces_the_doc07_signature() {
    let p = SvJumpParams {
        days: 60,
        interval_ms: 60_000,
        eta: 0.0,
        ..Default::default()
    };
    let path = sv_jump_path(9, &p);
    // 10 bps iid noise: 1m RV inflated ~50%, 5m ~13%, 15m ~4%, 1h ~1%.
    let noisy = add_microstructure_noise(&path.history, 1.0e-3, 1);
    let cfg = ForecastConfig::default();
    let cal = fit(
        &cfg,
        &ForecastInput {
            asset: "SUI-like",
            history: &noisy,
        },
        Horizon::from_days(7.0),
    );
    let vol = |ms: u64| {
        cal.signature
            .iter()
            .find(|s| s.interval_ms == ms)
            .unwrap()
            .annualized_vol
    };
    let one_hour = vol(3_600_000);
    let r1m = vol(60_000) / one_hour;
    let r15m = vol(900_000) / one_hour;
    assert!((1.35..=1.7).contains(&r1m), "1m/1h {r1m}");
    assert!(
        vol(300_000) / one_hour > 1.08,
        "5m/1h {}",
        vol(300_000) / one_hour
    );
    assert!((0.95..=1.08).contains(&r15m), "15m/1h {r15m}");
    assert!(cal.interval_derived);
    assert_eq!(cal.sample_interval_ms, 900_000, "{:?}", cal.signature);

    // BTC-like: flat signature → the finest interval the data supports.
    let cal = fit(
        &cfg,
        &ForecastInput {
            asset: "BTC-like",
            history: &path.history,
        },
        Horizon::from_days(7.0),
    );
    assert_eq!(cal.sample_interval_ms, 60_000, "{:?}", cal.signature);

    // A 5-minute live sampler can never be asked for 1-minute bars.
    let five: Vec<(u64, f64)> = path.history.iter().step_by(5).copied().collect();
    let cal = fit(
        &cfg,
        &ForecastInput {
            asset: "BTC-like",
            history: &five,
        },
        Horizon::from_days(7.0),
    );
    assert_eq!(cal.sample_interval_ms, 300_000);
}

#[test]
fn gate6_identical_inputs_serialize_byte_identically() {
    let p = base_params();
    let a = sv_jump_path(42, &p);
    let b = sv_jump_path(42, &p);
    let cfg = ForecastConfig::default();
    let now = a.end_ms() + 1_000;
    let (ca, fa) = fit_and_forecast(
        &cfg,
        &ForecastInput {
            asset: "SYN",
            history: &a.history,
        },
        Horizon::from_days(21.0),
        now,
    );
    let (cb, fb) = fit_and_forecast(
        &cfg,
        &ForecastInput {
            asset: "SYN",
            history: &b.history,
        },
        Horizon::from_days(21.0),
        now,
    );
    assert_eq!(
        serde_json::to_string(&fa).unwrap(),
        serde_json::to_string(&fb).unwrap()
    );
    assert_eq!(
        serde_json::to_string(&ca).unwrap(),
        serde_json::to_string(&cb).unwrap()
    );
    assert_eq!(fa.staleness_ms, 1_000);
    // A persisted calibration replays the same forecast. (Not asserted
    // byte-identical: serde_json's default float parser is not correctly
    // rounded, so a JSON round trip can move a weight by one ulp.)
    let cr: Calibration = serde_json::from_str(&serde_json::to_string(&ca).unwrap()).unwrap();
    let fr = forecast(
        &cr,
        &ForecastInput {
            asset: "SYN",
            history: &a.history,
        },
        now,
    );
    assert!((fr.sigma_mean / fa.sigma_mean - 1.0).abs() < 1e-12);
    assert_eq!(fr.regime, fa.regime);
    assert_eq!(fr.residuals.len(), fa.residuals.len());
}

#[test]
fn gate7_no_oracle_provider_is_named_in_the_crate() {
    let sources = [
        include_str!("lib.rs"),
        include_str!("rv.rs"),
        include_str!("har.rs"),
        include_str!("signature.rs"),
        include_str!("history.rs"),
        include_str!("synthetic.rs"),
        include_str!("norm.rs"),
        include_str!("tests.rs"),
    ];
    // Assembled so this file does not itself contain the names.
    let needles = [
        ["py", "th"].concat(),
        ["switch", "board"].concat(),
        ["her", "mes"].concat(),
        ["bench", "mark"].concat(),
    ];
    for src in sources {
        let lower = src.to_lowercase();
        for n in &needles {
            assert!(!lower.contains(n.as_str()), "found {n:?} in crate source");
        }
    }
}

#[test]
fn horizon_changes_the_fit() {
    let path = sv_jump_path(77, &base_params());
    let cfg = ForecastConfig::default();
    let input = ForecastInput {
        asset: "SYN",
        history: &path.history,
    };
    let short = fit(&cfg, &input, Horizon::from_days(7.0));
    let long = fit(&cfg, &input, Horizon::from_days(60.0));
    assert_eq!(short.horizon_days, 7);
    assert_eq!(long.horizon_days, 60);
    assert_ne!(short.weights.beta, long.weights.beta);
    // Longer horizons lean on the slower components.
    assert!(
        long.weights.beta[1] < short.weights.beta[1],
        "short {:?} long {:?}",
        short.weights,
        long.weights
    );
    let now = path.end_ms();
    let fs = forecast(&short, &input, now);
    let fl = forecast(&long, &input, now);
    assert_ne!(fs.sigma_mean, fl.sigma_mean);
    assert!(Horizon::from_days(0.2).days(60) == 1 && Horizon::from_days(90.0).days(60) == 60);
}

#[test]
fn short_history_is_cold_with_fixed_weights_and_lognormal_quantiles() {
    let p = SvJumpParams {
        days: 12,
        ..base_params()
    };
    let path = sv_jump_path(1, &p);
    let cfg = ForecastConfig::default();
    let (cal, fc) = fit_and_forecast(
        &cfg,
        &ForecastInput {
            asset: "SYN",
            history: &path.history,
        },
        Horizon::from_days(7.0),
        path.end_ms(),
    );
    assert!(!cal.fitted && cal.residuals.is_empty());
    assert_eq!(fc.regime, Regime::Cold);
    assert!(fc.is_usable() && !fc.calibrated);
    assert!(
        (fc.sigma_mean / 0.87 - 1.0).abs() < 0.35,
        "{}",
        fc.sigma_mean
    );
    assert!((fc.quantile(0.5) - fc.sigma_mean).abs() < 1e-9);
    assert!(fc.quantile(0.3) < fc.sigma_mean && fc.quantile(0.7) > fc.sigma_mean);
    assert!(fc.coverage > 0.3 && fc.coverage <= 1.0);

    // Empty and single-sample inputs are unusable, never NaN.
    let (_, fc) = fit_and_forecast(
        &cfg,
        &ForecastInput {
            asset: "SYN",
            history: &[],
        },
        Horizon::from_days(7.0),
        0,
    );
    assert!(!fc.is_usable() && fc.regime == Regime::Cold);
    assert!(serde_json::to_string(&fc).is_ok());
    let (_, fc) = fit_and_forecast(
        &cfg,
        &ForecastInput {
            asset: "SYN",
            history: &[(5, 1.0)],
        },
        Horizon::from_days(7.0),
        10,
    );
    assert!(!fc.is_usable() && fc.staleness_ms == 5);
}

#[test]
fn elevated_regime_flags_a_vol_step_without_a_jump() {
    let p = SvJumpParams {
        days: 200,
        ..Default::default()
    };
    let a = sv_jump_path(4, &p);
    // Splice a 3× vol tail onto a calm path: same generator, scaled returns.
    let end = a.end_ms();
    let mut hist = a.history.clone();
    let split = hist.partition_point(|s| s.0 <= end - 2 * DAY);
    let orig: Vec<f64> = hist.iter().map(|s| s.1).collect();
    let mut lp = orig[split - 1].ln();
    for i in split..hist.len() {
        lp += (orig[i] / orig[i - 1]).ln() * 3.0;
        hist[i].1 = lp.exp();
    }
    let cfg = ForecastConfig::default();
    let cal = fit(
        &cfg,
        &ForecastInput {
            asset: "SYN",
            history: &hist[..split],
        },
        Horizon::from_days(7.0),
    );
    assert!(cal.fitted);
    let before = forecast(
        &cal,
        &ForecastInput {
            asset: "SYN",
            history: &hist[..split],
        },
        end - 2 * DAY,
    );
    let fc = forecast(
        &cal,
        &ForecastInput {
            asset: "SYN",
            history: &hist,
        },
        end,
    );
    assert_eq!(before.regime, Regime::Calm, "{before:?}");
    assert_eq!(fc.regime, Regime::Elevated, "{fc:?}");
    assert!(
        fc.sigma_mean > 1.2 * before.sigma_mean,
        "{} vs {}",
        fc.sigma_mean,
        before.sigma_mean
    );
    // Kurtosis / intensity are exposed for the surface's convexity.
    assert!(fc.excess_kurtosis.is_finite());
    assert!(fc.jump_intensity_per_day >= 0.0);
}

#[test]
fn unsorted_history_is_handled() {
    let path = sv_jump_path(
        8,
        &SvJumpParams {
            days: 90,
            ..base_params()
        },
    );
    let mut rev = path.history.clone();
    rev.reverse();
    let cfg = ForecastConfig::default();
    let now = path.end_ms();
    let (_, a) = fit_and_forecast(
        &cfg,
        &ForecastInput {
            asset: "SYN",
            history: &path.history,
        },
        Horizon::from_days(7.0),
        now,
    );
    let (_, b) = fit_and_forecast(
        &cfg,
        &ForecastInput {
            asset: "SYN",
            history: &rev,
        },
        Horizon::from_days(7.0),
        now,
    );
    assert_eq!(a, b);
}
