//! The WS RFQ decision path (SO-447): every gate the serve loop in
//! `main.rs` applies to one `RFQBroadcast` — desk live → pair served →
//! spot fresh → spec admissible → per-spec rate budget → desk pricing —
//! factored out of the socket plumbing so the WIRED sequence is testable
//! in process. The decision and its RFQ-funnel row (SO-425) are built
//! together, so exactly one row per RFQ exists by construction; the
//! caller signs/sends and then records the row through [`record`].

use std::sync::Arc;

use protocol_types::bucket_spec::BucketSpec;
use protocol_types::sides::Side;
use pyth_client::SpotError;

use super::guards::{self, GuardConfig, SpecRateLimiter};
use super::history::{RfqOutcomeRow, RfqRecorder};
use super::quote::{Decision, RfqInputs};
use super::{Desk, QuoteKey};
use crate::pricing::serves_pair;

/// One incoming `RFQBroadcast`.
pub struct WsRfq<'a> {
    pub request_id: &'a str,
    pub side: Side,
    pub spec: &'a BucketSpec,
    pub write_amount: u64,
}

/// What the serve loop holds that the decision needs.
pub struct WsDeps<'a> {
    /// `None` = desk disabled (or its vault routing unresolved): every
    /// RFQ declines.
    pub desk: Option<&'a Desk>,
    pub settlement_coin_type: &'a str,
    pub guard_cfg: &'a GuardConfig,
    pub spec_limiter: &'a SpecRateLimiter,
    pub quote_ttl_ms: u64,
}

#[derive(Debug, PartialEq)]
pub enum WsOutcome {
    /// Sign and send. `nonce` is the one the caller offered and must now
    /// commit as its counter (the reservation already carries it).
    Quote { premium: u64, nonce: u64 },
    Decline { reason: String },
}

/// Decide one WS RFQ. `spot_for(market_index)` resolves the live spot
/// for the matched market; `nonce` is the next quote nonce, burned only
/// on a `Quote` outcome. Returns the outcome and its funnel row — a
/// terminal `declined` row, or a pending `quoted` one the fill poller /
/// TTL sweep later closes.
pub async fn decide_ws_rfq(
    deps: &WsDeps<'_>,
    rfq: &WsRfq<'_>,
    spot_for: impl Fn(usize) -> Result<f64, SpotError>,
    nonce: u64,
    now: u64,
) -> (WsOutcome, RfqOutcomeRow) {
    let spec = rfq.spec;
    let mut row = RfqOutcomeRow::base(
        rfq.request_id.to_string(),
        "ws",
        spec.is_put,
        match rfq.side {
            Side::Writer => "writer",
            Side::Trader => "trader",
        },
        spec.strike_scaled(),
        spec.expiry_ms,
        rfq.write_amount,
        now,
    );
    let declined = |row: RfqOutcomeRow, reason: String| {
        let r = row.declined(reason.clone(), now);
        (WsOutcome::Decline { reason }, r)
    };

    let Some(desk) = deps.desk else {
        return declined(row, "desk disabled".into());
    };

    // The spec IS the pricing input. There is no bucket to resolve — it
    // may not exist until the taker's own transaction creates it — and
    // no lookup to trust or distrust: the bot signs the spec it priced,
    // and on chain that quote can only ever be spent against a bucket
    // with exactly these economics.
    let (asset_ct, settlement_ct) = (
        protocol_types::asset::canonicalize_move_type(&spec.asset),
        protocol_types::asset::canonicalize_move_type(&spec.settlement),
    );
    // Pick the market whose pair this spec belongs to. This is the check
    // that keeps a spoofed spec harmless: the bot prices only pairs it
    // configured.
    let Some(mi) = desk
        .market_meta
        .iter()
        .position(|m| serves_pair(&asset_ct, &settlement_ct, &m.coin_type, deps.settlement_coin_type))
    else {
        let reason = format!("pair not served: {asset_ct}/{settlement_ct}");
        metrics::counter!("mm_bot_quote_failures_total", "reason" => "pair_not_served").increment(1);
        tracing::debug!(request_id = rfq.request_id, %reason, "declining");
        return declined(row, reason);
    };
    row.symbol = Some(desk.market_meta[mi].symbol.clone());

    // Live spot scaled into the bucket's units.
    let spot = match spot_for(mi) {
        Ok(s) => s,
        Err(e) => {
            metrics::counter!("mm_bot_quote_failures_total", "reason" => "stale_price").increment(1);
            tracing::debug!(request_id = rfq.request_id, reason = e.as_str(), "declining: stale market data");
            return declined(row, format!("stale market data: {}", e.as_str()));
        }
    };
    row.spot_at_request = Some(spot);

    // Admissibility BEFORE the model. A permissionless spec surface is a
    // new risk surface, not just new plumbing — see `desk::guards`.
    let tau = (spec.expiry_ms.saturating_sub(now)) as f64 / (1000.0 * 86_400.0 * 365.0);
    let sigma = desk.model_sigma(mi, spot, spec.strike_scaled(), tau);
    if let Err(refusal) = guards::admissible(deps.guard_cfg, spec, spot, sigma, now) {
        metrics::counter!("mm_bot_quote_failures_total", "reason" => refusal.label()).increment(1);
        tracing::debug!(request_id = rfq.request_id, refusal = refusal.label(), "declining");
        return declined(row, refusal.reason());
    }
    if !deps.spec_limiter.allow(spec) {
        let refusal = guards::Refusal::RateLimited;
        metrics::counter!("mm_bot_quote_failures_total", "reason" => refusal.label()).increment(1);
        tracing::debug!(request_id = rfq.request_id, "declining: spec rate limit");
        return declined(row, refusal.reason());
    }

    let inputs = RfqInputs {
        write_amount: rfq.write_amount,
        is_put: spec.is_put,
        strike: spec.sig as u128,
        strike_scale: spec.exp,
        expiry_ms: spec.expiry_ms,
    };
    // The nonce is fixed BEFORE pricing so the reservation the desk
    // takes under this request id already carries its chain join key
    // (SO-444).
    let key = QuoteKey { request_id: rfq.request_id.to_string(), nonce };
    match desk.price_ws_rfq(rfq.side, mi, inputs, spot, Some(key), now).await {
        Decision::Quote { premium, model_fair, surface_vol, .. } => {
            let row = row.quoted(
                premium,
                model_fair,
                surface_vol,
                now.saturating_add(deps.quote_ttl_ms),
                Some(nonce),
            );
            (WsOutcome::Quote { premium, nonce }, row)
        }
        Decision::Decline { reason } => {
            metrics::counter!("mm_bot_quote_failures_total", "reason" => "price_declined").increment(1);
            tracing::debug!(request_id = rfq.request_id, %reason, "declining");
            declined(row, reason)
        }
    }
}

/// The WS path's single funnel write site: a no-op without a sink
/// (`[desk.history] record_rfq_outcomes = false`).
pub fn record(rfq: &Option<Arc<dyn RfqRecorder>>, row: RfqOutcomeRow) {
    if let Some(r) = rfq {
        r.record_rfq(row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desk::book::{DetectedFill, FillLink, FillSide};
    use crate::desk::history::MemoryRfqRecorder;
    use crate::desk::testkit::{self, COIN, DAY_MS, SETTLEMENT};
    use crate::desk::{close_filled_rfqs, Desk};

    /// Minute-aligned so `NotCreatable` never masks the gate under test.
    const NOW: u64 = 1_699_999_980_000;
    const TTL_MS: u64 = 30_000;

    fn spec(asset: &str, strike: u128, is_put: bool) -> BucketSpec {
        BucketSpec::new(asset, SETTLEMENT, NOW + 30 * DAY_MS, strike, 0, is_put).unwrap()
    }

    fn desk() -> Desk {
        testkit::desk(testkit::kernel(1e9), testkit::paper_venue("rfq", 0.0, 0.0, 1.0))
    }

    struct Harness {
        desk: Desk,
        guard_cfg: GuardConfig,
        limiter: SpecRateLimiter,
    }

    impl Harness {
        fn new(guard_cfg: GuardConfig) -> Self {
            let limiter = SpecRateLimiter::new(&guard_cfg);
            Self { desk: desk(), guard_cfg, limiter }
        }

        async fn decide(
            &self,
            live: bool,
            request_id: &str,
            side: Side,
            spec: &BucketSpec,
            spot: Result<f64, SpotError>,
            nonce: u64,
        ) -> (WsOutcome, RfqOutcomeRow) {
            let deps = WsDeps {
                desk: live.then_some(&self.desk),
                settlement_coin_type: SETTLEMENT,
                guard_cfg: &self.guard_cfg,
                spec_limiter: &self.limiter,
                quote_ttl_ms: TTL_MS,
            };
            let rfq = WsRfq { request_id, side, spec, write_amount: 1_000_000 };
            decide_ws_rfq(&deps, &rfq, |_| spot, nonce, NOW).await
        }
    }

    fn reason(o: &WsOutcome) -> &str {
        match o {
            WsOutcome::Decline { reason } => reason,
            other => panic!("expected Decline, got {other:?}"),
        }
    }

    /// One row per RFQ through every wired decision path — six declines
    /// plus quoted → filled and quoted → expired — with the recorder ON.
    #[tokio::test]
    async fn every_ws_decision_path_yields_exactly_one_terminal_row() {
        // Budget of two per spec: the trader decline (5) and the signed
        // quote (6) both pass the admissibility + rate gates on `atm`,
        // so the third attempt (7) is the one the limiter refuses.
        let h = Harness::new(GuardConfig { max_quotes_per_spec: 2, ..GuardConfig::default() });
        let rec = Arc::new(MemoryRfqRecorder::default());
        let rfq: Option<Arc<dyn RfqRecorder>> = Some(rec.clone());
        let atm = spec(COIN, 100, false);

        // 1. Desk disabled.
        let (o, row) = h.decide(false, "r1", Side::Writer, &atm, Ok(100.0), 1).await;
        assert_eq!(reason(&o), "desk disabled");
        record(&rfq, row);
        // 2. Pair not served.
        let (o, row) = h.decide(true, "r2", Side::Writer, &spec("0x9::x::X", 100, false), Ok(100.0), 1).await;
        assert!(reason(&o).starts_with("pair not served"), "{o:?}");
        record(&rfq, row);
        // 3. Stale market data.
        let (o, row) = h.decide(true, "r3", Side::Writer, &atm, Err(SpotError::UnderlyingStale), 1).await;
        assert_eq!(reason(&o), "stale market data: underlying price stale or unseen");
        record(&rfq, row);
        // 4. Inadmissible spec (deep wing).
        let (o, row) = h.decide(true, "r4", Side::Writer, &spec(COIN, 1_000, false), Ok(100.0), 1).await;
        assert_eq!(reason(&o), "strike outside the quotable moneyness band");
        record(&rfq, row);
        // 5. Trader side: the desk never writes (doc 08 §4.1) — this is
        //    the `price_declined` path through `Desk::price_ws_rfq`.
        let (o, row) = h.decide(true, "r5", Side::Trader, &atm, Ok(100.0), 1).await;
        assert_eq!(reason(&o), "desk does not write options (long-only strategy)");
        record(&rfq, row);
        // 6. A signed quote (pending row, nonce burned)…
        let (o, row) = h.decide(true, "r6", Side::Writer, &atm, Ok(100.0), 41).await;
        let WsOutcome::Quote { nonce, .. } = o else { panic!("expected Quote, got {o:?}") };
        assert_eq!(nonce, 41);
        assert_eq!(row.outcome, "quoted");
        assert_eq!(row.nonce, Some(41));
        assert_eq!(row.valid_until_ms, Some((NOW + TTL_MS) as i64));
        record(&rfq, row);
        // 7. …which exhausts the spec's rate budget: rate limited.
        let (o, row) = h.decide(true, "r7", Side::Writer, &atm, Ok(100.0), 42).await;
        assert_eq!(reason(&o), "too many quotes for this spec");
        record(&rfq, row);
        // 8. A second quote on another spec, left to expire.
        let put = spec(COIN, 100, true);
        let (o, row) = h.decide(true, "r8", Side::Writer, &put, Ok(100.0), 42).await;
        assert!(matches!(o, WsOutcome::Quote { nonce: 42, .. }), "{o:?}");
        record(&rfq, row);

        // Nothing is terminal for the two quotes yet.
        let pending = |r: &MemoryRfqRecorder| {
            r.outcomes().into_iter().filter(|(_, o)| o == "quoted").count()
        };
        assert_eq!(pending(&rec), 2);

        // r6 fills: the poller's wired FillLink → RfqFillKey join.
        let fill = DetectedFill {
            sequence: 900,
            bucket_id: protocol_types::ids::ObjectId::new([7u8; 32]),
            side: FillSide::Bought,
            amount: 1_000_000,
            premium: 1,
            link: FillLink::WsQuote { nonce: 41 },
        };
        close_filled_rfqs(&rfq, &[(fill, 1.0)], NOW + 5_000);
        // r8 expires: not before the TTL + detection grace…
        assert_eq!(rec.sweep_expired(NOW + TTL_MS), 0);
        // …then exactly once.
        assert_eq!(rec.sweep_expired(NOW + TTL_MS + 300_001), 1);
        assert_eq!(rec.sweep_expired(NOW + TTL_MS + 600_000), 0);

        let outcomes = rec.outcomes();
        assert_eq!(outcomes.len(), 8, "one row per RFQ: {outcomes:?}");
        let mut ids: Vec<&str> = outcomes.iter().map(|(id, _)| id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 8, "request ids must be unique");
        for (id, outcome) in &outcomes {
            let expected = match id.as_str() {
                "r6" => "filled",
                "r8" => "expired",
                _ => "declined",
            };
            assert_eq!(outcome, expected, "{id}");
        }
        let rows = rec.rows.lock();
        let r6 = rows.iter().find(|r| r.request_id == "r6").unwrap();
        assert_eq!(r6.fill_sequence, Some(900));
        assert_eq!(r6.symbol.as_deref(), Some("TSUI"));
        assert_eq!(r6.spot_at_request, Some(100.0));
        // The declined rows keep their reasons and stay declined.
        assert!(rows.iter().filter(|r| r.outcome == "declined").all(|r| r.reason.is_some()));
    }

    /// With the flag off there is no sink (`History::rfq_recorder` is
    /// `None`), so the same decisions write nothing — while the quote
    /// path itself (reservation included) is unaffected.
    #[tokio::test]
    async fn with_the_recorder_off_no_decision_path_writes_a_row() {
        let h = Harness::new(GuardConfig::default());
        let rec = Arc::new(MemoryRfqRecorder::default());
        let off: Option<Arc<dyn RfqRecorder>> = None;
        let atm = spec(COIN, 100, false);
        let (o, row) = h.decide(true, "q1", Side::Writer, &atm, Ok(100.0), 7).await;
        assert!(matches!(o, WsOutcome::Quote { nonce: 7, .. }), "{o:?}");
        record(&off, row);
        let (_, row) = h.decide(false, "q2", Side::Writer, &atm, Ok(100.0), 8).await;
        record(&off, row);
        let (_, row) = h.decide(true, "q3", Side::Trader, &atm, Ok(100.0), 8).await;
        record(&off, row);
        let fill = DetectedFill {
            sequence: 1,
            bucket_id: protocol_types::ids::ObjectId::new([7u8; 32]),
            side: FillSide::Bought,
            amount: 1,
            premium: 1,
            link: FillLink::WsQuote { nonce: 7 },
        };
        close_filled_rfqs(&off, &[(fill, 1.0)], NOW);
        assert!(rec.rows.lock().is_empty());
        // The quote still reserved its premium under the request id.
        let k = h.desk.kernel.read();
        let b = &k.book;
        assert!(b.reserved_total() > 0);
        assert!(b.reservations_snapshot().iter().any(|r| r.key == "q1" && r.nonce == Some(7)));
    }
}
