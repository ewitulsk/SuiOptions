//! Kernel tests: the live/simulation trace-parity gate (doc 08 §5 P1),
//! determinism, and the kernel-vs-pure-function equivalences the
//! mm-bot runtime tests rely on.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use parking_lot::RwLock;
use protocol_types::ids::ObjectId;
use protocol_types::sides::Side;
use vol_forecast::RollingVolBuffer;

use super::*;
use crate::book::Holding;
use crate::exits::put::PutExerciseConfig;
use crate::hedge::Fill;
use crate::model::SurfaceConfig;

pub(crate) const DAY_MS: u64 = 86_400_000;
/// 2026-09-01, so capital-snapshot age arithmetic never underflows.
pub(crate) const T0: u64 = 1_788_220_800_000;
pub(crate) const COIN: &str = "0x1::tsui::TSUI";

fn oid(b: u8) -> ObjectId {
    ObjectId::new([b; 32])
}

/// Model on cold vol buffers: the surface quotes the 0.60 fallback with
/// no risk premium (the `quote` test fixture).
pub(crate) fn model() -> MarketModel {
    MarketModel::new(
        "TSUI".into(),
        COIN.into(),
        Arc::new(RwLock::new(RollingVolBuffer::new(DAY_MS))),
        Arc::new(RwLock::new(RollingVolBuffer::new(7 * DAY_MS))),
        0.60,
        0.0,
        0.0,
        SurfaceConfig {
            risk_premium: 0.0,
            skew: 0.0,
            convexity: 0.0,
            term_short_boost: 0.0,
            term_decay_years: 0.25,
            anchor_ratio: None,
            floor_vol: 0.01,
            cap_vol: 5.0,
            short_window_weight: 1.0,
            long_window_weight: 1.0,
        },
    )
}

pub(crate) fn v1() -> V1BidParams {
    V1BidParams {
        base_spread_volpts: 0.05,
        size_penalty_volpts_per_pct_nav: 0.01,
        size_penalty_quadratic_from_pct: 3.0,
        inventory_penalty_max_volpts: 0.10,
        inventory_penalty_start_util: 0.6,
        max_single_fill_pct_nav: 5.0,
        funding_income_credit: 0.0,
        composition_penalty_volpts: 0.05,
    }
}

pub(crate) fn config() -> KernelConfig {
    KernelConfig {
        v1: v1(),
        limits: LimitsConfig::default(),
        capital: CapitalConfig::default(),
        hedge: HedgeConfig { band_pct_nav: 5.0, ..HedgeConfig::default() },
        exits: ExitsConfig {
            spot_pools: HashMap::from([("TSUI".to_string(), "0x1".to_string())]),
            put: PutExerciseConfig::default(),
            max_slice: 1_000_000_000_000,
            ..ExitsConfig::default()
        },
        quote_ttl_ms: 30_000,
        expected_holding_years: 21.0 / 365.0,
        stress_gap_down: 0.60,
        stress_gap_up: 0.80,
        primary_slippage_bps: 0.0,
        settlement_decimals: 6,
        curator_session: false,
        deepbook_adapter: false,
    }
}

pub(crate) fn kernel(nav: u64) -> DeskKernel {
    DeskKernel::new(config(), vec![model()], Book::new(nav), 0.0, false, T0)
}

fn holding(bucket: u8, is_put: bool, strike: u128, expiry_ms: u64, vault: u64, wallet: u64) -> Holding {
    Holding {
        bucket_id: oid(bucket),
        option_coin_type: format!("0x1::c{bucket}::C"),
        asset_coin_type: COIN.into(),
        settlement_coin_type: "0x1::tusdc::TUSDC".into(),
        is_put,
        strike,
        strike_scale: 0,
        expiry_ms,
        amount_vault: vault,
        amount_wallet: wallet,
        coin_positions: Vec::new(),
    }
}

/// ATM call/put 30 days out, 500k units.
fn atm(is_put: bool) -> RfqInputs {
    RfqInputs { write_amount: 500_000, is_put, strike: 100, strike_scale: 0, expiry_ms: T0 + 30 * DAY_MS }
}

fn mark_update(at_ms: u64, custody: Option<(Vec<Holding>, Vec<Written>)>) -> Event {
    let (holdings, written) = match custody {
        Some((h, w)) => (Some(h), Some(w)),
        None => (None, None),
    };
    Event::MarkUpdate(Box::new(MarkUpdate {
        at_ms,
        holdings,
        written,
        nav: Some(1_000_000_000),
        appraisal_at: Some(at_ms),
        risk_off: Some(false),
        spot_by_model: vec![Some(100.0)],
        free_settlement: 1e9,
        free_underlying_by_asset: HashMap::from([("TSUI".to_string(), 5e8)]),
        external: None,
        queued_withdrawal_value: Some(0.0),
        queued_withdrawal_shares: 0.0,
    }))
}

fn rfq(id: &str, side: Side, inputs: RfqInputs, nonce: Option<u64>, at_ms: u64) -> Event {
    Event::Rfq { request_id: id.into(), side, market: 0, inputs, spot: 100.0, reserve: nonce, at_ms }
}

/// The recorded trace both harnesses replay: a mark pass over a
/// call-heavy book, writer/trader RFQs, a hedge tick that must trade
/// (1.2M-unit ATM call ≈ 0.6M delta against a 0.5M-unit band), funding, a
/// chain fill, a revert, the exit ladder over an ITM wallet call, a NAV
/// drawdown that trips the kill switch, and an operator risk-off.
fn trace() -> Vec<Event> {
    let call = holding(1, false, 100, T0 + 30 * DAY_MS, 1_200_000, 0);
    let itm = holding(2, false, 90, T0 + 12 * 3_600_000, 0, 1_000);
    vec![
        Event::Spot { market: 0, spot: 100.0, at_ms: T0 },
        mark_update(T0, Some((vec![call, itm], Vec::new()))),
        rfq("r1", Side::Writer, atm(false), Some(1), T0 + 1_000),
        rfq("r2", Side::Trader, atm(false), Some(2), T0 + 1_500),
        rfq("r3", Side::Writer, atm(true), Some(3), T0 + 2_000),
        Event::QuoteAccepted { request_id: "r1".into(), at_ms: T0 + 2_500 },
        Event::HedgeTimer { market: 0, at_ms: T0 + 3_000 },
        Event::HedgeTick { market: 0, position_units: 0.0, funding_rate_annual: 0.0, at_ms: T0 + 3_000 },
        Event::FundingSettled { market: 0, paid: 3.0, at_ms: T0 + 3_500 },
        Event::QuoteFilled {
            fill: DetectedFill {
                sequence: 900,
                bucket_id: oid(1),
                side: FillSide::Bought,
                amount: 1_000_000,
                premium: 6_000_000,
                link: FillLink::WsQuote { nonce: 1 },
            },
            fair_total: 6_500_000.0,
            at_ms: T0 + 4_000,
        },
        Event::QuoteReverted { request_id: "r3".into(), at_ms: T0 + 4_500 },
        Event::ExitTimer { wallet_cash: 1_000_000, at_ms: T0 + 5_000 },
        Event::HedgeTimer { market: 0, at_ms: T0 + 6_000 },
        Event::HedgeTick { market: 0, position_units: -5_000_000.0, funding_rate_annual: 0.0, at_ms: T0 + 6_000 },
        Event::NavSample { nav: 800_000_000, at_ms: T0 + 7_000 },
        rfq("r4", Side::Writer, atm(false), Some(4), T0 + 8_000),
        Event::Policy { state: PolicyState::RiskOff, active: true, at_ms: T0 + 9_000 },
        rfq("r5", Side::Writer, atm(false), Some(5), T0 + 10_000),
    ]
}

/// What a synchronous venue (the paper venue) answers a submitted order
/// with: ack + full fill at the reference spot, then its realized P&L.
fn venue_response(market: usize, order: &HedgeOrder, at_ms: u64) -> Vec<Event> {
    vec![
        Event::Hedge { market, event: HedgeEvent::Acknowledged(order.id), at_ms },
        Event::Hedge {
            market,
            event: HedgeEvent::Filled(Fill { order: order.id, size_units: order.size_units, price: order.spot }),
            at_ms,
        },
        Event::HedgeRealized { market, realized_pnl: 12.5 * order.id as f64, at_ms },
    ]
}

fn at_ms(ev: &Event) -> u64 {
    match ev {
        Event::Spot { at_ms, .. }
        | Event::SpotStale { at_ms, .. }
        | Event::Rfq { at_ms, .. }
        | Event::QuoteSent { at_ms, .. }
        | Event::QuoteAccepted { at_ms, .. }
        | Event::QuoteExpired { at_ms, .. }
        | Event::QuoteReverted { at_ms, .. }
        | Event::QuoteFilled { at_ms, .. }
        | Event::Hedge { at_ms, .. }
        | Event::HedgeTimer { at_ms, .. }
        | Event::HedgeTick { at_ms, .. }
        | Event::HedgeRealized { at_ms, .. }
        | Event::FundingSettled { at_ms, .. }
        | Event::VenueMargin { at_ms, .. }
        | Event::Liquidation { at_ms, .. }
        | Event::ExitTimer { at_ms, .. }
        | Event::PutLiquidity { at_ms, .. }
        | Event::ExercisePtbResult { at_ms, .. }
        | Event::MarginTopUpResult { at_ms, .. }
        | Event::NavSample { at_ms, .. }
        | Event::Policy { at_ms, .. } => *at_ms,
        Event::MarkUpdate(u) => u.at_ms,
    }
}

/// Live-shaped adapter: each I/O task hands its event to the kernel the
/// moment it has it and executes the commands inline — a submitted
/// hedge order comes straight back as the venue's synchronous events.
fn run_live_shaped(trace: &[Event]) -> Vec<Command> {
    let mut k = kernel(1_000_000_000);
    let mut out = Vec::new();
    fn apply(k: &mut DeskKernel, ev: Event, out: &mut Vec<Command>) {
        let at = at_ms(&ev);
        for cmd in k.on_event(ev) {
            if let Command::SubmitHedgeOrder { market, order } = &cmd {
                let responses = venue_response(*market, order, at);
                out.push(cmd.clone());
                for r in responses {
                    apply(k, r, out);
                }
                continue;
            }
            out.push(cmd);
        }
    }
    for ev in trace {
        apply(&mut k, ev.clone(), &mut out);
    }
    out
}

/// Simulation-shaped adapter: a clock-ordered event queue; the trace is
/// scheduled up front and venue responses are scheduled at their
/// actionable time, FIFO within a timestamp.
fn run_sim_shaped(trace: &[Event]) -> Vec<Command> {
    let mut k = kernel(1_000_000_000);
    let mut out = Vec::new();
    let mut seq = 0u64;
    let mut queue: BTreeMap<(u64, u64), Event> = BTreeMap::new();
    for ev in trace {
        queue.insert((at_ms(ev), seq), ev.clone());
        seq += 1;
    }
    while let Some((&key, _)) = queue.iter().next() {
        let ev = queue.remove(&key).unwrap();
        let at = key.0;
        for cmd in k.on_event(ev) {
            if let Command::SubmitHedgeOrder { market, order } = &cmd {
                // Responses land before anything scheduled later, and
                // before anything else at the same instant that is
                // still queued behind them.
                let mut responses = venue_response(*market, order, at);
                let insert_seq = queue
                    .range((at, 0)..(at + 1, 0))
                    .next()
                    .map(|((_, s), _)| *s)
                    .unwrap_or(seq);
                // Re-key everything at `at` after the responses.
                let shifted: Vec<_> = queue.range((at, 0)..(at + 1, 0)).map(|(k, _)| *k).collect();
                let n = responses.len() as u64;
                for key in shifted.into_iter().rev() {
                    let ev = queue.remove(&key).unwrap();
                    queue.insert((key.0, key.1 + n), ev);
                }
                for (i, r) in responses.drain(..).enumerate() {
                    queue.insert((at, insert_seq + i as u64), r);
                }
                seq += n;
            }
            out.push(cmd);
        }
    }
    out
}

/// P1 gate: live adapter and simulation adapter produce identical
/// commands for identical event traces.
#[test]
fn live_and_simulation_harnesses_yield_byte_identical_commands() {
    let trace = trace();
    let live = run_live_shaped(&trace);
    let sim = run_sim_shaped(&trace);
    let (live_s, sim_s) = (format!("{live:#?}"), format!("{sim:#?}"));
    assert_eq!(live_s, sim_s, "live vs sim command sequences differ");
    // Determinism (doc 08 §1 item 7): the same trace again is byte-identical.
    assert_eq!(format!("{:#?}", run_live_shaped(&trace)), live_s);

    // The trace exercised every command family it was built to.
    let has = |f: &dyn Fn(&Command) -> bool| live.iter().any(f);
    let r1 = live
        .iter()
        .find(|c| matches!(c, Command::Quote { request_id, .. } | Command::Decline { request_id, .. } if request_id == "r1"));
    assert!(matches!(r1, Some(Command::Quote { .. })), "r1: {r1:?}");
    assert!(has(&|c| matches!(c, Command::Decline { request_id, reason } if request_id == "r2" && reason.contains("long-only"))));
    assert!(has(&|c| matches!(c, Command::ReservePremium(r) if r.key == "r1")));
    assert!(has(&|c| matches!(c, Command::ReleasePremium(r) if r.key == "r1" && r.state == crate::book::ReservationState::Filled)));
    assert!(has(&|c| matches!(c, Command::ReleasePremium(r) if r.key == "r3" && r.state == crate::book::ReservationState::Reverted)));
    assert!(has(&|c| matches!(c, Command::SubmitHedgeOrder { order, .. } if order.size_units < 0.0)));
    assert!(has(&|c| matches!(c, Command::RecordPnl(r) if r.line == PnlLine::Scalp)));
    assert!(has(&|c| matches!(c, Command::RecordPnl(r) if r.line == PnlLine::Funding && r.amount == -3.0)));
    assert!(has(&|c| matches!(c, Command::RecordPnl(r) if r.line == PnlLine::Spread && r.amount == 500_000.0)));
    assert!(has(&|c| matches!(c, Command::ExecuteCallPtb { leg: CallLeg::WalletCash { amount: 1_000, strike_cost: 90_000 }, .. })));
    assert!(has(&|c| matches!(c, Command::ActivatePolicy(PolicyState::KillSwitch))));
    assert!(has(&|c| matches!(c, Command::Decline { request_id, reason } if request_id == "r4" && reason.contains("kill switch"))));
    assert!(has(&|c| matches!(c, Command::ActivatePolicy(PolicyState::RiskOff))));
    assert!(has(&|c| matches!(c, Command::Decline { request_id, reason } if request_id == "r5" && reason.contains("risk-off"))));
}

/// The kernel's RFQ decision IS `quote::price_writer_flow` under the
/// kernel's own flow context (what `Desk::price_ws_rfq` computed).
#[test]
fn rfq_decision_matches_the_pure_writer_flow() {
    let mut k = kernel(1_000_000_000);
    let _ = k.on_event(mark_update(T0, None));
    let ctx = k.flow_context(0, 100.0);
    let direct = quote::price_writer_flow(&k.models[0], &k.cfg.v1, &k.cfg.limits, &ctx, &atm(false), T0);
    let via_kernel = k.decide_rfq(Side::Writer, 0, atm(false), 100.0, None, T0);
    assert_eq!(direct, via_kernel);
    assert!(matches!(direct, Decision::Quote { .. }), "{direct:?}");
    // No reservation without a key.
    assert_eq!(k.book.reserved_total(), 0);
    // With a key the premium is reserved for TTL + grace, once per key.
    let Decision::Quote { premium, .. } = k.decide_rfq(Side::Writer, 0, atm(false), 100.0, Some(("q", 9)), T0) else {
        panic!()
    };
    assert_eq!(k.book.reserved_total(), premium);
    let dup = k.decide_rfq(Side::Writer, 0, atm(false), 100.0, Some(("q", 10)), T0);
    assert!(matches!(dup, Decision::Decline { reason } if reason.contains("duplicate")));
    let r = &k.book.reservations_snapshot()[0];
    assert_eq!(r.expires_ms, T0 + 30_000 + 300_000);
    assert_eq!(r.nonce, Some(9));
}

/// Doc 08 §4.1: a trader-side RFQ hard-declines and reserves nothing;
/// legacy written inventory lifted by the mark pass blocks quoting until
/// it is unwound (and the policy transitions are emitted).
#[test]
fn trader_side_and_legacy_inventory_decline() {
    let mut k = kernel(1_000_000_000);
    let _ = k.on_event(mark_update(T0, None));
    let cmds = k.on_event(rfq("t1", Side::Trader, atm(false), Some(1), T0));
    assert_eq!(
        cmds,
        vec![Command::Decline {
            request_id: "t1".into(),
            reason: "desk does not write options (long-only strategy)".into()
        }]
    );
    assert_eq!(k.book.reserved_total(), 0);

    let written = Written {
        bucket_id: oid(1),
        position_id: oid(2),
        asset_coin_type: COIN.into(),
        is_put: false,
        strike: 100,
        strike_scale: 0,
        expiry_ms: T0 + 30 * DAY_MS,
        amount: 5,
        covered: 0,
    };
    let cmds = k.on_event(mark_update(T0 + 60_000, Some((Vec::new(), vec![written]))));
    assert!(cmds.contains(&Command::ActivatePolicy(PolicyState::LegacyWrittenInventory)));
    let cmds = k.on_event(rfq("w1", Side::Writer, atm(false), None, T0 + 61_000));
    assert!(matches!(&cmds[0], Command::Decline { reason, .. } if reason == "legacy written inventory present (5 naked units); quoting blocked until unwound"));
    let cmds = k.on_event(mark_update(T0 + 120_000, Some((Vec::new(), Vec::new()))));
    assert!(cmds.contains(&Command::ClearPolicy(PolicyState::LegacyWrittenInventory)));
    let cmds = k.on_event(rfq("w2", Side::Writer, atm(false), None, T0 + 121_000));
    assert!(matches!(&cmds[0], Command::Quote { .. }), "{cmds:?}");
}

/// Doc 08 §4.2 through the kernel: a long-call book targets a SHORT
/// perp; a partial fill rides in the working orders so the next tick
/// does not resubmit it; a long-put book targets a LONG perp; a mixed
/// book nets before it trades.
#[test]
fn hedge_ticks_plan_signed_orders_and_never_resubmit_working_size() {
    // NAV 1000 at spot 10 with the default 15% band ⇒ 15 units.
    let mut k = kernel(1_000);
    k.exposure.nav = 1_000.0;
    let _ = k.on_event(Event::Spot { market: 0, spot: 10.0, at_ms: T0 });
    k.markets[0].book_delta_units = 100.0;
    let cmds = k.on_event(Event::HedgeTick { market: 0, position_units: 0.0, funding_rate_annual: 0.0, at_ms: T0 + 1 });
    assert_eq!(cmds, vec![Command::SubmitHedgeOrder { market: 0, order: HedgeOrder { id: 1, size_units: -100.0, spot: 10.0 } }]);
    // Half fills: the remainder is working, so the next tick (venue now
    // −50) plans nothing.
    let _ = k.on_event(Event::Hedge { market: 0, event: HedgeEvent::PartiallyFilled(Fill { order: 1, size_units: -50.0, price: 10.0 }), at_ms: T0 + 2 });
    assert_eq!(k.markets[0].open_orders.working_units(), -50.0);
    let cmds = k.on_event(Event::HedgeTick { market: 0, position_units: -50.0, funding_rate_annual: 0.0, at_ms: T0 + 3 });
    assert!(cmds.is_empty(), "{cmds:?}");
    assert_eq!(k.markets[0].hedge_position_units, -50.0);
    // The rest fills; a stale spot reconciles but plans nothing.
    let _ = k.on_event(Event::Hedge { market: 0, event: HedgeEvent::Filled(Fill { order: 1, size_units: -50.0, price: 10.0 }), at_ms: T0 + 4 });
    assert!(k.markets[0].open_orders.is_empty());
    let _ = k.on_event(Event::SpotStale { market: 0, at_ms: T0 + 5 });
    k.markets[0].book_delta_units = 200.0;
    assert!(k.on_event(Event::HedgeTick { market: 0, position_units: -100.0, funding_rate_annual: 0.0, at_ms: T0 + 6 }).is_empty());
    assert_eq!(k.markets[0].next_order_id, 1, "a working order must not be resubmitted");

    // Long puts: a LONG perp.
    let mut k = kernel(1_000);
    k.exposure.nav = 1_000.0;
    let _ = k.on_event(Event::Spot { market: 0, spot: 10.0, at_ms: T0 });
    k.markets[0].book_delta_units = -80.0;
    let cmds = k.on_event(Event::HedgeTick { market: 0, position_units: 0.0, funding_rate_annual: 0.0, at_ms: T0 + 1 });
    assert!(matches!(&cmds[0], Command::SubmitHedgeOrder { order, .. } if order.size_units == 80.0));
    // Mixed: +100 / −100 nets to nothing; +100 / −70 hedges the residual.
    let mut k = kernel(1_000);
    k.exposure.nav = 1_000.0;
    let _ = k.on_event(Event::Spot { market: 0, spot: 10.0, at_ms: T0 });
    k.markets[0].book_delta_units = 100.0 + -100.0;
    assert!(k.on_event(Event::HedgeTick { market: 0, position_units: 0.0, funding_rate_annual: 0.0, at_ms: T0 + 1 }).is_empty());
    k.markets[0].book_delta_units = 100.0 + -70.0;
    let cmds = k.on_event(Event::HedgeTick { market: 0, position_units: 0.0, funding_rate_annual: 0.0, at_ms: T0 + 2 });
    assert!(matches!(&cmds[0], Command::SubmitHedgeOrder { order, .. } if order.size_units == -30.0));
    // A stale working order is cancelled (and stops counting) by the
    // timer sweep, so the tick that follows re-plans it.
    let cmds = k.on_event(Event::HedgeTimer { market: 0, at_ms: T0 + 2 + 60_000 });
    assert_eq!(cmds, vec![Command::CancelHedgeOrder { market: 0, id: 1 }]);
    let cmds = k.on_event(Event::HedgeTick { market: 0, position_units: 0.0, funding_rate_annual: 0.0, at_ms: T0 + 2 + 60_000 });
    assert!(matches!(&cmds[0], Command::SubmitHedgeOrder { order, .. } if order.id == 2 && order.size_units == -30.0), "{cmds:?}");
}

/// Funding and scalp land on their own lines, in order, as commands.
#[test]
fn funding_and_scalp_records_become_commands() {
    let mut k = kernel(1_000);
    k.seed_hedge_realized(0, 40.0);
    let cmds = k.on_event(Event::FundingSettled { market: 0, paid: -100.0, at_ms: T0 });
    assert_eq!(cmds, vec![Command::RecordPnl(PnlRecord { ts_ms: T0, line: PnlLine::Funding, amount: 100.0, note: "hedge funding accrual".into() })]);
    assert!(k.on_event(Event::FundingSettled { market: 0, paid: 0.0, at_ms: T0 }).is_empty());
    let cmds = k.on_event(Event::HedgeRealized { market: 0, realized_pnl: 240.0, at_ms: T0 + 1 });
    assert_eq!(cmds, vec![Command::RecordPnl(PnlRecord { ts_ms: T0 + 1, line: PnlLine::Scalp, amount: 200.0, note: "hedge rebalance".into() })]);
    assert!(k.on_event(Event::HedgeRealized { market: 0, realized_pnl: 240.0, at_ms: T0 + 2 }).is_empty());
    assert_eq!((k.book.pnl.funding, k.book.pnl.scalp), (100.0, 200.0));
}

/// The put waterfall through the kernel: a wanted put asks for its pool
/// ladder, the ladder plans the slices (wallet leg), and a landed slice
/// schedules the LONG-perp hedge close (a sell of |Δ| × units).
#[test]
fn put_exercise_queries_liquidity_plans_slices_and_closes_the_hedge() {
    let mut k = kernel(1_000_000_000);
    // 9-dec-ish put: strike 4_000 at scale 6 ⇒ $0.004/unit; 250M units
    // (the `put` fixture's profitable size), ITM inside the sweep window.
    let mut put_h = holding(3, true, 4_000, T0 + 3_600_000, 0, 250_000_000_000);
    put_h.strike_scale = 6;
    let _ = k.on_event(Event::Spot { market: 0, spot: 0.0035, at_ms: T0 });
    k.book.holdings = vec![put_h];
    let cmds = k.on_event(Event::ExitTimer { wallet_cash: 0, at_ms: T0 });
    assert_eq!(cmds, vec![Command::QueryPutLiquidity { bucket: oid(3), market: 0 }]);
    let pool = PoolLiquidity {
        base_balance: 250_000_000_000,
        quote_balance: 0,
        lot_size: 1_000,
        min_size: 10_000,
        asks: vec![(3_500_000, 1_000_000_000_000)],
    };
    let cmds = k.on_event(Event::PutLiquidity { bucket: oid(3), wallet_underlying: 0, vault_underlying: 0, pool, at_ms: T0 });
    assert!(!cmds.is_empty());
    assert!(cmds.iter().all(|c| matches!(c, Command::ExecutePutPtb { leg: PutLeg::Wallet, plan, .. } if plan.path == PutPath::BaseFlash)), "{cmds:?}");
    let total: u64 = cmds.iter().map(|c| match c { Command::ExecutePutPtb { plan, .. } => plan.amount, _ => 0 }).sum();
    assert_eq!(total, 250_000_000_000, "the ladder reconciles");
    // A slice lands: the hedge close sells Δ × units (Δ < 0 ⇒ a sell).
    let cmds = k.on_event(Event::ExercisePtbResult { bucket: oid(3), leg: PutLeg::Wallet, units: 1_000_000_000, ok: true, at_ms: T0 + 1 });
    assert!(matches!(&cmds[0], Command::SubmitHedgeOrder { order, .. } if order.size_units < 0.0), "{cmds:?}");
    // A failed slice closes nothing.
    assert!(k.on_event(Event::ExercisePtbResult { bucket: oid(3), leg: PutLeg::Wallet, units: 1_000_000_000, ok: false, at_ms: T0 + 2 }).is_empty());
}
