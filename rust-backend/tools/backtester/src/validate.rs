//! Engine validation against scraped ground truth (doc 06 §9.4).
//!
//! `validate-ribbon` (milestone 2): replay Ribbon's T-ETH-C / T-WBTC-C
//! round history (scraped from Ethereum logs by `fetch_ribbon.py`) and
//! compare two tracks round by round:
//!
//! - **realized** — Ribbon's actual strikes and auction premiums, marked
//!   against real spot: the ground-truth covered-call P&L.
//! - **modeled** — our strike selector (z-ladder + delta snap) and our
//!   DVOL-priced premium on the same dates, with the same fee rules: what
//!   the engine *would have predicted*.
//!
//! The milestone gate: modeled APY within ±2pts of realized, ITM-week
//! counts matching. The per-round premium comparison doubles as the
//! premium-model fit: the median implied-IV/DVOL ratio is the calibrated
//! `skew_bps`.
//!
//! `calibrate-skew` (milestone 3): from the free first-of-month Deribit
//! chain snapshots (`fetch_chain_snapshots.py`), measure where the ~10Δ
//! weekly call's mark IV sits relative to the DVOL index — the on-grid
//! wing premium our scenarios assume.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use pricing::{call_delta, call_price_per_unit, CallInputs};
use serde::Serialize;
use vault_sim::iv::realized_vol;
use vault_sim::paths::Bar;
use vault_sim::strategy::{StrikeSelector, BTC_LADDER};

use crate::data;

const YEAR_SECS: f64 = 365.0 * 86_400.0;

// Ribbon's fee schedule, mirrored by our vault: 2%/yr mgmt + 10%
// performance, profitable rounds only.
const MGMT_FEE_ANNUAL: f64 = 0.02;
const PERF_FEE: f64 = 0.10;

#[derive(Debug, Clone)]
struct RibbonRound {
    open_ts: u64,
    strike_usd: f64,
    expiry_ts: u64,
    /// Premium per option unit, in the vault asset (e.g. ETH/oToken).
    premium_per_unit: Option<f64>,
}

fn bidding_decimals(vault: &str) -> Result<f64> {
    Ok(match vault {
        "T-ETH-C" => 1e18,  // WETH
        "T-WBTC-C" => 1e8,  // WBTC
        other => bail!("unknown vault {other}"),
    })
}

fn load_ribbon_rounds(path: &Path, vault: &str) -> Result<Vec<RibbonRound>> {
    let bid_dec = bidding_decimals(vault)?;
    let mut rdr = csv::Reader::from_path(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut out = Vec::new();
    for row in rdr.deserialize::<BTreeMap<String, String>>() {
        let r = row?;
        if r["vault"] != vault || r["is_put"] == "1" {
            continue;
        }
        let premium_per_unit = match (r["premium_raw"].as_str(), r["otokens_sold_raw"].as_str()) {
            ("", _) | (_, "") => None,
            (p, s) => {
                let sold = s.parse::<f64>()? / 1e8;
                (sold > 0.0).then(|| p.parse::<f64>().unwrap_or(0.0) / bid_dec / sold)
            }
        };
        out.push(RibbonRound {
            open_ts: r["round_open_ts"].parse()?,
            strike_usd: r["strike_usd"].parse()?,
            expiry_ts: r["expiry_ts"].parse()?,
            premium_per_unit,
        });
    }
    if out.is_empty() {
        bail!("no rounds for {vault} in {}", path.display());
    }
    Ok(out)
}

/// Close at (or before) a unix timestamp, from daily bars.
fn close_at(bars: &[Bar], ts: u64) -> Option<(usize, f64)> {
    let ts_ms = ts * 1000;
    let idx = bars.partition_point(|b| b.ts_ms <= ts_ms);
    if idx == 0 {
        return None;
    }
    Some((idx - 1, bars[idx - 1].close))
}

/// Solve Black-Scholes for the IV that reproduces `price_usd` (bisection).
fn implied_vol(spot: f64, strike: f64, t_years: f64, price_usd: f64) -> Option<f64> {
    if price_usd <= 0.0 || t_years <= 0.0 {
        return None;
    }
    let (mut lo, mut hi) = (0.01f64, 5.0f64);
    let f = |sigma: f64| {
        call_price_per_unit(CallInputs { spot, strike, t_years, r: 0.0, sigma }) - price_usd
    };
    if f(lo) > 0.0 || f(hi) < 0.0 {
        return None;
    }
    for _ in 0..80 {
        let mid = 0.5 * (lo + hi);
        if f(mid) > 0.0 {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    Some(0.5 * (lo + hi))
}

/// One round's asset-denominated growth factor for a cash-settled covered
/// call, with the 2/10 fee gate applied.
fn round_growth(premium_per_unit: f64, strike: f64, spot_t: f64, t_years: f64) -> f64 {
    let payout = (spot_t - strike).max(0.0) / spot_t; // asset-settled short call
    let gross = 1.0 + premium_per_unit - payout;
    if gross > 1.0 {
        let mgmt = MGMT_FEE_ANNUAL * t_years;
        let perf = PERF_FEE * premium_per_unit;
        let fees = (mgmt + perf).min(gross - 1.0);
        gross - fees
    } else {
        gross
    }
}

#[derive(Debug, Default, Serialize)]
struct TrackStats {
    rounds: usize,
    apy: f64,
    itm_weeks: usize,
    max_drawdown: f64,
}

fn track_stats(growths: &[(f64, bool)], years: f64) -> TrackStats {
    let mut value = 1.0f64;
    let mut peak = 1.0f64;
    let mut max_dd = 0.0f64;
    let mut itm = 0;
    for (g, was_itm) in growths {
        value *= g;
        peak = peak.max(value);
        max_dd = max_dd.max(1.0 - value / peak);
        itm += usize::from(*was_itm);
    }
    TrackStats {
        rounds: growths.len(),
        apy: if years > 0.0 { value.powf(1.0 / years) - 1.0 } else { 0.0 },
        itm_weeks: itm,
        max_drawdown: max_dd,
    }
}

#[derive(Serialize)]
struct RoundComparison {
    date: String,
    spot_open: f64,
    spot_expiry: f64,
    ribbon_strike: f64,
    our_strike: f64,
    ribbon_premium_pct: Option<f64>,
    modeled_premium_pct: f64,
    dvol: f64,
    implied_iv: Option<f64>,
    itm: bool,
}

pub fn cmd_validate_ribbon(
    rounds_path: &Path,
    vault: &str,
    candles_path: &Path,
    dvol_path: &Path,
    out_dir: &Path,
    skew_bps: i64,
    haircut_bps: u64,
) -> Result<()> {
    let rounds = load_ribbon_rounds(rounds_path, vault)?;
    let bars = data::load_candles(candles_path)?;
    let dvol = data::load_vol_index_aligned(dvol_path, &bars)?;

    let selector = StrikeSelector {
        delta_target: 0.10,
        ladder: BTC_LADDER.to_vec(),
        r: 0.0,
        vol_floor: 0.2,
        vol_ceiling: 3.0,
    };

    let mut realized = Vec::new();
    let mut modeled = Vec::new();
    let mut modeled_matched = Vec::new();
    let mut haircut_estimates = Vec::new();
    let mut iv_ratios = Vec::new();
    let mut premium_ratios = Vec::new();
    let mut strike_ratios = Vec::new();
    let mut per_round = Vec::new();
    let (mut first_ts, mut last_ts) = (u64::MAX, 0u64);

    for r in &rounds {
        let Some((open_idx, s0)) = close_at(&bars, r.open_ts) else { continue };
        let Some((_, st)) = close_at(&bars, r.expiry_ts) else { continue };
        let tau = (r.expiry_ts.saturating_sub(r.open_ts)) as f64 / YEAR_SECS;
        if tau <= 0.0 || r.strike_usd <= 0.0 {
            continue;
        }
        first_ts = first_ts.min(r.open_ts);
        last_ts = last_ts.max(r.expiry_ts);
        let iv_index = dvol[open_idx];
        let itm = st > r.strike_usd;

        // Modeled track: our selector + the premium model under the
        // scenario parameters (skew on the index IV, haircut on the mid),
        // same dates. Defaults (0, 0) show the uncalibrated model.
        let sigma_model = iv_index * (1.0 + skew_bps as f64 / 10_000.0);
        let rv = realized_vol(&bars, open_idx, 30, 365.0);
        let choice = selector.select(s0, rv.max(0.05), sigma_model, tau);
        let modeled_premium_usd = call_price_per_unit(CallInputs {
            spot: s0,
            strike: choice.strike,
            t_years: tau,
            r: 0.0,
            sigma: sigma_model,
        }) * (1.0 - haircut_bps as f64 / 10_000.0);
        let modeled_premium = modeled_premium_usd / s0; // asset terms
        let modeled_round = (round_growth(modeled_premium, choice.strike, st, tau), st > choice.strike);
        modeled.push(modeled_round);
        strike_ratios.push(choice.strike / r.strike_usd);

        // Realized track: their strike + their premium (when auctioned).
        let mut implied = None;
        if let Some(p_asset) = r.premium_per_unit {
            realized.push((round_growth(p_asset, r.strike_usd, st, tau), itm));
            // Same round, our model: the matched-window APY comparison.
            modeled_matched.push(modeled_round);
            // Auction haircut: actual cleared premium vs the mark value at
            // the measured weekly wing IV (≈ 0.94 × DVOL per the snapshot
            // calibration) — separating "weekly wing prices under the
            // index" from "auctions clear under the mark".
            let mark_usd = call_price_per_unit(CallInputs {
                spot: s0,
                strike: r.strike_usd,
                t_years: tau,
                r: 0.0,
                sigma: iv_index * 0.94,
            });
            if mark_usd > 0.0 {
                haircut_estimates.push(1.0 - (p_asset * s0 / mark_usd).min(2.0));
            }
            // Their premium priced at their strike, for the model fit.
            let their_modeled_usd = call_price_per_unit(CallInputs {
                spot: s0,
                strike: r.strike_usd,
                t_years: tau,
                r: 0.0,
                sigma: iv_index,
            });
            if their_modeled_usd > 0.0 {
                premium_ratios.push(p_asset * s0 / their_modeled_usd);
            }
            implied = implied_vol(s0, r.strike_usd, tau, p_asset * s0);
            if let Some(iv) = implied {
                iv_ratios.push(iv / iv_index);
            }
        }

        per_round.push(RoundComparison {
            date: format_date(r.open_ts),
            spot_open: s0,
            spot_expiry: st,
            ribbon_strike: r.strike_usd,
            our_strike: choice.strike,
            ribbon_premium_pct: r.premium_per_unit.map(|p| p * 100.0),
            modeled_premium_pct: modeled_premium * 100.0,
            dvol: iv_index,
            implied_iv: implied,
            itm,
        });
    }

    if realized.is_empty() {
        bail!("no comparable rounds with premiums for {vault}");
    }
    let years = (last_ts - first_ts) as f64 / YEAR_SECS;
    // The realized track only exists where a Gnosis auction cleared;
    // compare like for like on those rounds, with the full-window modeled
    // run reported separately.
    let matched_years = years * realized.len() as f64 / modeled.len().max(1) as f64;
    let realized_stats = track_stats(&realized, matched_years);
    let modeled_matched_stats = track_stats(&modeled_matched, matched_years);
    let modeled_stats = track_stats(&modeled, years);

    let median = |v: &mut Vec<f64>| -> f64 {
        if v.is_empty() {
            return f64::NAN;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let med_iv_ratio = median(&mut iv_ratios.clone());
    let med_premium_ratio = median(&mut premium_ratios.clone());
    let med_strike_ratio = median(&mut strike_ratios.clone());
    let med_haircut = median(&mut haircut_estimates.clone());

    fs::create_dir_all(out_dir)?;
    let mut w = csv::Writer::from_path(out_dir.join("rounds.csv"))?;
    for r in &per_round {
        w.serialize(r)?;
    }
    w.flush()?;

    #[derive(Serialize)]
    struct Summary {
        vault: String,
        realized: TrackStats,
        modeled_matched: TrackStats,
        modeled_full_window: TrackStats,
        apy_gap_pts: f64,
        median_implied_iv_over_dvol: f64,
        suggested_skew_bps: i64,
        median_premium_actual_over_modeled: f64,
        median_auction_haircut: f64,
        median_strike_ours_over_ribbon: f64,
    }
    let summary = Summary {
        vault: vault.to_string(),
        apy_gap_pts: (modeled_matched_stats.apy - realized_stats.apy) * 100.0,
        realized: realized_stats,
        modeled_matched: modeled_matched_stats,
        modeled_full_window: modeled_stats,
        median_implied_iv_over_dvol: med_iv_ratio,
        suggested_skew_bps: ((med_iv_ratio - 1.0) * 10_000.0) as i64,
        median_premium_actual_over_modeled: med_premium_ratio,
        median_auction_haircut: med_haircut,
        median_strike_ours_over_ribbon: med_strike_ratio,
    };
    fs::write(out_dir.join("summary.json"), serde_json::to_string_pretty(&summary)?)?;

    println!("── {vault} validation (skew_bps {skew_bps}, haircut_bps {haircut_bps}) ──");
    println!(
        "realized  (their strikes+premiums): APY {:+.1}%  ITM {}/{}  maxDD {:.1}%",
        summary.realized.apy * 100.0,
        summary.realized.itm_weeks,
        summary.realized.rounds,
        summary.realized.max_drawdown * 100.0,
    );
    println!(
        "modeled   (same rounds, our model): APY {:+.1}%  ITM {}/{}  maxDD {:.1}%",
        summary.modeled_matched.apy * 100.0,
        summary.modeled_matched.itm_weeks,
        summary.modeled_matched.rounds,
        summary.modeled_matched.max_drawdown * 100.0,
    );
    println!(
        "modeled   (full window incl. bear): APY {:+.1}%  ITM {}/{}  maxDD {:.1}%",
        summary.modeled_full_window.apy * 100.0,
        summary.modeled_full_window.itm_weeks,
        summary.modeled_full_window.rounds,
        summary.modeled_full_window.max_drawdown * 100.0,
    );
    println!(
        "APY gap on matched rounds: {:+.1} pts (milestone gate: ±2)",
        summary.apy_gap_pts
    );
    println!(
        "auction haircut vs wing-IV mark: median {:.0} bps",
        summary.median_auction_haircut * 10_000.0
    );
    println!(
        "premium fit: actual/modeled median {:.2}; implied IV / DVOL median {:.3} → skew_bps ≈ {}",
        summary.median_premium_actual_over_modeled,
        summary.median_implied_iv_over_dvol,
        summary.suggested_skew_bps,
    );
    println!(
        "strike fit: ours/Ribbon median {:.3}",
        summary.median_strike_ours_over_ribbon
    );
    println!("wrote {}", out_dir.display());
    Ok(())
}

fn format_date(ts: u64) -> String {
    // Days since epoch → ISO date, no chrono needed.
    let days = ts / 86_400;
    let mut year = 1970u64;
    let mut remaining = days;
    loop {
        let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
        let len = if leap { 366 } else { 365 };
        if remaining < len {
            break;
        }
        remaining -= len;
        year += 1;
    }
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let months = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 1;
    for len in months {
        if remaining < len {
            break;
        }
        remaining -= len;
        month += 1;
    }
    format!("{year:04}-{month:02}-{:02}", remaining + 1)
}

// ─── calibrate-skew (milestone 3) ───

pub fn cmd_calibrate_skew(snapshots: &Path, dvol_path: &Path, currency: &str) -> Result<()> {
    let mut rdr = csv::Reader::from_path(snapshots)
        .with_context(|| format!("opening {}", snapshots.display()))?;
    // snapshot_date → (spot, chain rows).
    type ChainRow = (u32, f64, f64, f64); // (dte, strike, iv, mark)
    let mut by_day: BTreeMap<String, (f64, Vec<ChainRow>)> = BTreeMap::new();
    for row in rdr.deserialize::<BTreeMap<String, String>>() {
        let r = row?;
        if r["currency"] != currency {
            continue;
        }
        let entry = by_day
            .entry(r["snapshot_date"].clone())
            .or_insert_with(|| (r["spot"].parse().unwrap_or(0.0), Vec::new()));
        entry.1.push((
            r["days_to_expiry"].parse()?,
            r["strike"].parse()?,
            r["iv"].parse()?,
            r["mark_price"].parse()?,
        ));
    }

    // Align DVOL by date string.
    let mut dvol_by_day = BTreeMap::new();
    {
        let mut rdr = csv::Reader::from_path(dvol_path)?;
        for row in rdr.deserialize::<BTreeMap<String, String>>() {
            let r = row?;
            let ts_ms: u64 = r["ts_ms"].parse()?;
            dvol_by_day.insert(format_date(ts_ms / 1000), r["dvol"].parse::<f64>()? / 100.0);
        }
    }

    let mut ratios = Vec::new();
    let mut rows = 0usize;
    for (day, (spot, chain)) in &by_day {
        let Some(&index_iv) = dvol_by_day.get(day) else { continue };
        // The weekly expiry: smallest tenor in the 4–10 day band.
        let Some(&dte) = chain
            .iter()
            .map(|(d, ..)| d)
            .filter(|d| (4..=10).contains(*d))
            .min()
        else {
            continue;
        };
        let tau = dte as f64 / 365.0;
        // The ~10Δ call by each strike's own mark IV.
        let mut best: Option<(f64, f64)> = None; // (delta distance, iv)
        for (d, strike, iv, _) in chain {
            if *d != dte || *iv <= 0.0 {
                continue;
            }
            let delta = call_delta(CallInputs {
                spot: *spot,
                strike: *strike,
                t_years: tau,
                r: 0.0,
                sigma: *iv,
            });
            let dist = (delta - 0.10).abs();
            if best.is_none_or(|(b, _)| dist < b) {
                best = Some((dist, *iv));
            }
        }
        if let Some((dist, wing_iv)) = best {
            if dist < 0.05 {
                ratios.push(wing_iv / index_iv);
                rows += 1;
            }
        }
    }

    if ratios.is_empty() {
        bail!("no usable snapshots for {currency}");
    }
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| ratios[((p * (ratios.len() - 1) as f64).round() as usize).min(ratios.len() - 1)];
    println!(
        "{currency} 10Δ-weekly IV / DVOL over {rows} monthly snapshots: \
         p25 {:.3}  median {:.3}  p75 {:.3}",
        pct(0.25),
        pct(0.50),
        pct(0.75),
    );
    println!(
        "→ scenario skew_bps candidates: [{}, {}, {}]",
        ((pct(0.25) - 1.0) * 10_000.0) as i64,
        ((pct(0.50) - 1.0) * 10_000.0) as i64,
        ((pct(0.75) - 1.0) * 10_000.0) as i64,
    );
    Ok(())
}
