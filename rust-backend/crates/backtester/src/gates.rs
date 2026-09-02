//! Doc 08 §9.6 P5 gate tests (PR O). The put and mixed fixtures are
//! hand-checked against Black-Scholes closed forms, not against the
//! engine's own pricer; the doc 07 reproduction runs against the lake
//! mirror when `DESK_LAKE_MIRROR` points at it (release build, ~1 min).

#[cfg(test)]
mod tests {
    use crate::data::{Bar, FundingRow};
    use crate::engine;
    use crate::scenario::Scenario;
    use crate::study::Metric;

    fn flat_bars(days: i64, start_ms: i64, px: f64) -> Vec<Bar> {
        (0..days * 1440).map(|i| Bar { ts_ms: start_ms + i * 60_000, open: px, high: px, low: px, close: px, volume: 1.0 }).collect()
    }

    /// Black-Scholes with zero rates and carry: `(price, |delta|)`.
    fn bs(is_put: bool, spot: f64, strike: f64, sigma: f64, t: f64) -> (f64, f64) {
        let n = |x: f64| 0.5 * (1.0 + erf(x / 2f64.sqrt()));
        let d1 = ((spot / strike).ln() + 0.5 * sigma * sigma * t) / (sigma * t.sqrt());
        let d2 = d1 - sigma * t.sqrt();
        if is_put { (strike * n(-d2) - spot * n(-d1), n(-d1)) } else { (spot * n(d1) - strike * n(d2), n(d1)) }
    }

    /// The V1 bid by hand: fair at `σ − base spread` minus the expected
    /// hedge cost of the delta trade — 7 bp of hedge notional + 0.03 flat,
    /// plus, for the put's LONG hedge, the funding it is expected to pay
    /// over the 21-day expected holding period at the annualized rate of
    /// the latest settlement (0.0001 per 8 h = 10.95%/yr); a short hedge's
    /// funding income is credited at zero (doc 09 G2).
    fn hand_bid(is_put: bool, spot: f64, strike: f64, qty: f64) -> f64 {
        let (px, delta) = bs(is_put, spot, strike, 0.75, 1.0 / 365.0);
        let notional = delta * qty * spot;
        let funding = if is_put { 0.0001 * 8760.0 / 8.0 * notional * 21.0 / 365.0 } else { 0.0 };
        px * qty - (notional * 7e-4 + 0.03) - funding
    }

    fn erf(x: f64) -> f64 {
        // Abramowitz-Stegun 7.1.26, |ε| < 1.5e-7.
        let t = 1.0 / (1.0 + 0.3275911 * x.abs());
        let y = 1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * (-x * x).exp();
        if x >= 0.0 { y } else { -y }
    }

    type Fixture = (Scenario, Vec<Bar>, Vec<FundingRow>, Vec<(i64, f64)>);

    #[allow(clippy::field_reassign_with_default)]
    fn fixture(call_share: f64, nav0: f64, notional: f64) -> Fixture {
        let mut s = Scenario::default();
        s.from = "2025-01-01".into();
        s.to = "2025-01-01".into();
        s.nav0 = nav0;
        s.flow.source = "constant".into();
        s.flow.mode = "daily".into();
        s.flow.notional_per_day = notional;
        s.flow.hour_utc = 0;
        s.flow.call_share = call_share;
        s.flow.tenor_days = 1.0;
        s.flow.use_expiry_board = false;
        s.estimator.kind = "vol_index".into();
        s.estimator.risk_premium = 0.0;
        s.bid.size_penalty_volpts_per_pct_nav = 0.0;
        s.bid.inventory_penalty_max_volpts = 0.0;
        s.limits.per_expiry_max = 1.0;
        s.limits.call_premium_max = 1.0;
        s.limits.put_premium_max = 1.0;
        s.limits.premium_budget_hard = 1.0;
        s.hurdle.max_drawdown = 1.0;
        s.latency = crate::latency::LatencyConfig::zero();
        s.oracle.latency_ms = 0;
        s.exercise.gas_per_rebalance = 0.0;
        s.revalue_interval_min = 15;
        let start = crate::data::date_start_ms(&s.from).unwrap();
        let bars = flat_bars(2, start, 3.0);
        let funding: Vec<FundingRow> = (0..6).map(|n| FundingRow { ts_ms: start + n * 8 * 3_600_000, rate: 0.0001, interval_hours: 8.0 }).collect();
        (s, bars, funding, vec![(start - 1, 80.0)])
    }

    /// Put fixture, by hand: one 1-day put on 10 000 units at spot 3 on
    /// the nearest lattice strike, struck at σ = 0.80 − 0.05 spread = 0.75
    /// ⇒ premium = BS_put(3, K, 0.75, 1/365) × 10 000 − expected hedge
    /// cost; the desk goes long ≈ |Δ| × 10 000 units of perp, paying 3.5
    /// bp fee + 0.03 flat + 3.5 bp slippage on that notional and 3 ×
    /// 0.0001 × notional of funding; nothing is exercised on the flat path.
    #[test]
    fn put_fixture_matches_the_hand_calculation() {
        let (s, bars, funding, vi) = fixture(0.0, 50_000.0, 30_000.0);
        let out = engine::run(&s, &bars, &funding, &vi).unwrap();
        let l = &out.ledger.lines;
        assert_eq!(l.fills, 1);
        assert_eq!(out.stats.fills_put, 1);
        let strike = out.ledger.options.values().next().expect("open put").spec.strike;
        assert!((strike / 3.0 - 1.0).abs() < 0.03, "lattice strike {strike}");
        let premium_hand = hand_bid(true, 3.0, strike, 10_000.0);
        assert!((l.premium_paid / premium_hand - 1.0).abs() < 0.02, "premium {} vs hand {premium_hand} (strike {strike})", l.premium_paid);
        let pos = out.nav_path.iter().map(|p| p.perp_position).fold(0.0, f64::max);
        let (_, delta) = bs(true, 3.0, strike, 0.80, 1.0 / 365.0);
        assert!((pos / (delta * 10_000.0) - 1.0).abs() < 0.03, "long hedge {pos} vs |Δ|·qty {}", delta * 10_000.0);
        let notional = pos * 3.0;
        let fees_hand = notional * 3.5e-4 + 0.03;
        let slip_hand = notional * 3.5e-4;
        assert!((l.hedge_fees - fees_hand).abs() < 0.05, "fees {} vs {fees_hand}", l.hedge_fees);
        assert!((l.hedge_slippage - slip_hand).abs() < 0.05, "slippage {} vs {slip_hand}", l.hedge_slippage);
        // The settlement at minute 0 precedes the hedge fill: two of the
        // day's three rows charge the long.
        let funding_hand = 2.0 * 0.0001 * notional;
        assert!((l.funding_paid - funding_hand).abs() < 0.05, "funding {} vs {funding_hand}", l.funding_paid);
        assert_eq!(l.option_payoff, 0.0, "ATM on a flat path is never exercised");
        // The put is still open one minute before its expiry: its mark is
        // the last sliver of time value, added back for the cash identity.
        let marks = out.ledger.option_marks();
        assert!(marks < 0.15 * premium_hand, "residual mark {marks}");
        let nav_hand = 50_000.0 - premium_hand - fees_hand - slip_hand - funding_hand;
        assert!((out.nav_end - marks - nav_hand).abs() < 0.02 * premium_hand, "nav {} (marks {marks}) vs hand {nav_hand}", out.nav_end);
        // Attribution: the whole option leg is exit-vs-mark + mtm; the
        // model edge is the 5-point spread ≈ premium × (0.80/0.75 − 1).
        let a = crate::attribution::report(&s, &out).unwrap();
        assert!(a.cumulative.reconciliation_gap.abs() < 1e-6, "{}", a.cumulative.reconciliation_gap);
        assert!(a.option_identity_gap.abs() < 1e-6 && a.perp_identity_gap.abs() < 1e-6);
        let edge_hand = bs(true, 3.0, strike, 0.80, 1.0 / 365.0).0 * 10_000.0 - premium_hand;
        assert!((a.lines.model_edge_put / edge_hand - 1.0).abs() < 0.05, "edge {} vs {edge_hand}", a.lines.model_edge_put);
        assert!(a.lines.funding_paid_long > 0.0 && a.lines.funding_paid_short == 0.0);
        let m = Metric::from_run(&s, &out);
        assert!(m.labels.iter().any(|l| l == "margin_model=isolated(bluefin_rules)"));
    }

    /// Mixed fixture, by hand: a call and a put of 5 000 units each net to
    /// ≈ zero delta inside the band, so no hedge trades at all; NAV falls
    /// by exactly the two premiums and both expire worthless.
    #[test]
    fn mixed_fixture_nets_delta_and_matches_the_hand_calculation() {
        let (s, bars, funding, vi) = fixture(0.5, 50_000.0, 30_000.0);
        let out = engine::run(&s, &bars, &funding, &vi).unwrap();
        let l = &out.ledger.lines;
        assert_eq!((out.stats.fills_call, out.stats.fills_put), (1, 1));
        let strike_of = |is_put: bool| out.ledger.options.values().find(|p| p.spec.kind.is_put() == is_put).expect("open").spec.strike;
        let (kc, kp) = (strike_of(false), strike_of(true));
        let premium_hand = hand_bid(false, 3.0, kc, 5_000.0) + hand_bid(true, 3.0, kp, 5_000.0);
        assert!((l.premium_paid / premium_hand - 1.0).abs() < 0.02, "premium {} vs hand {premium_hand}", l.premium_paid);
        assert_eq!(l.hedge_fills, 0, "net delta inside the band: no hedge");
        assert_eq!(l.funding_paid, 0.0);
        let marks = out.ledger.option_marks();
        assert!((out.nav_end - marks - (50_000.0 - premium_hand)).abs() < 0.02 * premium_hand, "{} (marks {marks})", out.nav_end);
        let a = crate::attribution::report(&s, &out).unwrap();
        assert!(a.cumulative.reconciliation_gap.abs() < 1e-6);
        assert_eq!(a.by_type.len(), 2);
        assert_eq!((a.by_type[0].fills, a.by_type[1].fills), (1, 1));
        assert!(a.by_type[0].model_edge_at_entry_non_realized > 0.0 && a.by_type[1].model_edge_at_entry_non_realized > 0.0);
        // Put-heavy and call-heavy variants of the same fixture hedge in
        // opposite directions.
        let (p, ..) = fixture(0.2, 50_000.0, 30_000.0);
        let (c, ..) = fixture(0.8, 50_000.0, 30_000.0);
        let op = engine::run(&p, &bars, &funding, &vi).unwrap();
        let oc = engine::run(&c, &bars, &funding, &vi).unwrap();
        let pos = |o: &engine::RunOutput| o.nav_path.iter().map(|x| x.perp_position).fold(0.0, |a: f64, b| if b.abs() > a.abs() { b } else { a });
        assert!(pos(&op) > 0.0, "put-heavy hedges long: {}", pos(&op));
        assert!(pos(&oc) < 0.0, "call-heavy hedges short: {}", pos(&oc));
    }

    /// Hedge-cost identity on the synthetic path: fees = Σ notional × taker
    /// bps + fills × flat, slippage = Σ notional × slippage bps (taker
    /// only), and the doc 07 §5 shape — a tighter band turns over more.
    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn band_turnover_and_cost_identity_on_the_synthetic_path() {
        let mut s = Scenario::default();
        s.from = "2025-01-01".into();
        s.to = "2025-03-11".into();
        s.limits.per_expiry_max = 0.30;
        s.limits.call_premium_max = 0.30;
        s.bid.size_penalty_volpts_per_pct_nav = 0.0;
        s.latency = crate::latency::LatencyConfig::zero();
        s.margin.enabled = false;
        let start = crate::data::date_start_ms(&s.from).unwrap();
        let bars = crate::synth::synthetic_bars(70, start);
        let mut turnovers = Vec::new();
        for band in [5.0, 20.0] {
            let mut sb = s.clone();
            sb.hedge.band_pct_nav = band;
            sb.hedge.band_wide_pct_nav = band * 1.5;
            let out = engine::run(&sb, &bars, &[], &[]).unwrap();
            let l = &out.ledger.lines;
            let fees_hand = l.hedge_turnover_notional * 3.5e-4 + l.hedge_fills as f64 * 0.03;
            // Fees are charged on the fill price, the turnover line on the
            // reference mark: within the slippage of each other.
            assert!((l.hedge_fees / fees_hand - 1.0).abs() < 1e-3, "band {band}: fees {} vs {fees_hand}", l.hedge_fees);
            let slip_hand = l.hedge_turnover_notional * 3.5e-4;
            assert!((l.hedge_slippage / slip_hand - 1.0).abs() < 1e-3, "band {band}: slippage {} vs {slip_hand}", l.hedge_slippage);
            let m = Metric::from_run(&sb, &out);
            assert!(m.labels.iter().any(|x| x == "margin_model=none(doc07_reproduction)"));
            assert_eq!(m.liquidations, 0);
            assert!(m.reconciliation_gap.abs() < 1e-6, "{}", m.reconciliation_gap);
            turnovers.push(m.hedge_turnover_nav_per_30d);
        }
        assert!(turnovers[0] > turnovers[1] * 1.3, "5% band {} should turn over more than 20% band {}", turnovers[0], turnovers[1]);
    }

    /// Doc 07 §5 / doc 10 §2 reproduction on the real SUI year (Aug 2025
    /// → Jul 2026, band 20, doc 07's no-margin assumption), gated on the
    /// lake mirror (`DESK_LAKE_MIRROR=/path`); ~1 min in a release build.
    #[test]
    fn doc07_call_turnover_reproduces_within_tolerance_on_the_lake_mirror() {
        let Ok(mirror) = std::env::var("DESK_LAKE_MIRROR") else {
            eprintln!("DESK_LAKE_MIRROR unset: skipping the doc 07 reproduction");
            return;
        };
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios/sui_doc07_calls.toml");
        let mut s = Scenario::load(&path).unwrap();
        s.margin.enabled = false;
        let store = crate::data::open_store(&format!("file://{mirror}")).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (bars, funding) = rt.block_on(async {
            let bars = crate::data::load_bars(&store, &s.spot_exchange, &s.spot_symbol, "2025-07-24", &s.to).await.unwrap();
            let funding = crate::data::load_funding(&store, &s.funding_exchange, &s.funding_symbol, &s.from, &s.to).await.unwrap();
            (bars, funding)
        });
        let out = engine::run(&s, &bars, &funding, &[]).unwrap();
        let m = Metric::from_run(&s, &out);
        let (_, d07, _, d10) = crate::results::DOC07_REFERENCE[4];
        let t = m.hedge_turnover_nav_per_30d;
        eprintln!("doc07 reproduction (Aug 2025 – Jul 2026, band 20): turnover {t:.2} vs doc07 {d07} / doc10 {d10}, fills {}, nav_end {:.0}", m.fills, m.nav_end);
        assert!((t / d07 - 1.0).abs() <= crate::results::DOC07_TURNOVER_TOL, "turnover {t} vs doc 07 {d07}");
        assert!((t / d10 - 1.0).abs() <= crate::results::DOC10_TURNOVER_TOL, "turnover {t} vs doc 10 {d10}");
        assert_eq!(m.liquidations, 0);
    }
}
