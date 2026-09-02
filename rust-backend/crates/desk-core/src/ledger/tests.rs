//! Hand-computable ledger fixtures (doc 08 P1 gate): a call life, a put
//! life, expiry, call and put exercise (all three put paths), a hedge
//! reversal, funding long and short, a liquidation, a failed PTB, a
//! queued withdrawal, a resale, custody re-sync, and the invariant
//! checker catching a deliberately corrupted ledger.

use super::*;

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9 * a.abs().max(b.abs()).max(1.0)
}

fn reconciled(l: &Ledger) {
    assert_eq!(l.check(), Vec::<Violation>::new(), "nav {} explained {}", l.nav(), l.nav_explained());
}

fn reservation(key: &str, amount: u64, is_put: bool, expiry_ms: u64, at_ms: u64) -> Reservation {
    Reservation {
        key: key.into(),
        nonce: Some(1),
        amount,
        is_put,
        expiry_ms,
        exercise_cash: 0.0,
        hedge_notional: 0.0,
        quoted_at_ms: at_ms,
        expires_ms: at_ms + 30_000,
        state: ReservationState::Quoted,
        state_at_ms: at_ms,
    }
}

fn spec(kind: OptionKind, strike: f64, expiry_ms: u64) -> OptionSpec {
    OptionSpec { kind, strike, expiry_ms }
}

fn bought(option: &str, s: OptionSpec, qty: f64, premium: f64, mark: f64, at_ms: u64) -> LedgerEvent {
    LedgerEvent::OptionBought { option: option.into(), spec: Some(s), qty, premium, mark_per_unit: mark, at_ms }
}

fn plan(option: &str, path: ExercisePath, qty: f64) -> ExercisePlan {
    ExercisePlan {
        option: option.into(),
        path,
        qty,
        asset: "SUI".into(),
        settlement_out: 0.0,
        settlement_in: 0.0,
        underlying_in: 0.0,
        underlying_out: 0.0,
        flash_borrowed: 0.0,
        flash_repaid: 0.0,
        route_notional: 0.0,
        gas: 0.0,
    }
}

/// A call's life: reserve → accept → fill (spread) → mark → expire
/// worthless. Every step reconciles by hand.
#[test]
fn call_life_reconciles_by_hand() {
    let mut l = Ledger::new(1_000.0);
    reconciled(&l);
    l.apply(&LedgerEvent::Reserve(reservation("r1", 60, false, 100, 1))).unwrap();
    assert_eq!(l.reserved_total(), 60.0);
    assert!(l.pending.quotes.contains("r1"));
    assert_eq!(l.available_capital(), 940.0);
    // A duplicate key and an over-capital reservation are refused.
    assert_eq!(
        l.apply(&LedgerEvent::Reserve(reservation("r1", 1, false, 100, 1))),
        Err(LedgerError::DuplicateReservation("r1".into()))
    );
    assert!(matches!(
        l.apply(&LedgerEvent::Reserve(reservation("r2", 941, false, 100, 1))),
        Err(LedgerError::ExceedsAvailableCapital { .. })
    ));
    l.apply(&LedgerEvent::ReservationTransition { key: "r1".into(), state: ReservationState::Accepted, at_ms: 2 }).unwrap();
    assert_eq!(l.reserved_total(), 60.0, "accepted keeps capacity");
    assert!(l.pending.quotes.is_empty());
    // Fill: 10 units for 60, marked 6.5 ⇒ spread +5.
    l.apply(&bought("c", spec(OptionKind::Call, 10.0, 100), 10.0, 60.0, 6.5, 3)).unwrap();
    l.apply(&LedgerEvent::ReservationTransition { key: "r1".into(), state: ReservationState::Filled, at_ms: 3 }).unwrap();
    assert_eq!(l.reserved_total(), 0.0);
    assert_eq!(l.settlement, 940.0);
    assert!(near(l.option_marks(), 65.0));
    assert!(near(l.nav(), 1_005.0));
    assert!(near(l.lines.spread, 5.0));
    assert_eq!(l.lines.premium_paid, 60.0);
    let u = l.premium_usage();
    assert!(near(u.call, 65.0) && u.put == 0.0 && near(u.total, 65.0) && near(u.by_expiry[&100], 65.0));
    reconciled(&l);
    // Mark to 8: +15 unrealized.
    l.apply(&LedgerEvent::MarkOptions { marks: vec![("c".into(), 8.0)], at_ms: 4 }).unwrap();
    assert!(near(l.nav(), 1_020.0));
    assert!(near(l.lines.option_mark, 15.0));
    reconciled(&l);
    // Expiry: worthless.
    l.apply(&LedgerEvent::ExpireOptions { at_ms: 100 }).unwrap();
    assert!(l.options.is_empty());
    assert!(near(l.nav(), 940.0));
    assert!(near(l.lines.option_mark, -65.0));
    assert_eq!(l.lines.expired_worthless, 1);
    reconciled(&l);
    assert_eq!(l.events_applied, 6, "refused events do not count");
}

/// A put's life through the vault-underlying exercise path: the exact
/// units leave, the exact underlying is delivered, the exact strike
/// cash arrives.
#[test]
fn put_life_and_vault_underlying_exercise_deliver_exact_assets() {
    let mut l = Ledger::new(1_000.0);
    l.apply(&LedgerEvent::ResyncBalances { settlement: None, underlying: vec![("SUI".into(), 50.0, 10.0)], at_ms: 1 }).unwrap();
    assert!(near(l.nav(), 1_500.0));
    assert!(near(l.equity_flows.resync_underlying, 500.0));
    l.apply(&bought("p", spec(OptionKind::Put, 10.0, 1_000), 20.0, 30.0, 1.5, 2)).unwrap();
    assert!(near(l.nav(), 1_500.0), "bought at fair: no spread");
    l.apply(&LedgerEvent::MarkUnderlying { asset: "SUI".into(), mark: 8.0, at_ms: 3 }).unwrap();
    l.apply(&LedgerEvent::MarkOptions { marks: vec![("p".into(), 2.0)], at_ms: 3 }).unwrap();
    assert!(near(l.nav(), 970.0 + 400.0 + 40.0));
    reconciled(&l);
    let mut x = plan("p", ExercisePath::PutVaultUnderlying, 20.0);
    x.underlying_out = 20.0;
    x.settlement_in = 200.0;
    x.gas = 1.0;
    l.apply(&LedgerEvent::ExerciseSubmitted { op: 1, plan: x, at_ms: 4 }).unwrap();
    assert_eq!(l.options["p"].pending_units, 20.0);
    assert_eq!(l.pending.committed_spend(), 1.0);
    assert_eq!(l.find_pending_exercise("p", 20.0), Some(1));
    reconciled(&l);
    l.apply(&LedgerEvent::ExerciseSettled { op: 1, ok: true, actual: None, at_ms: 5 }).unwrap();
    assert!(l.options.is_empty(), "the exact quantity left");
    assert_eq!(l.underlying["SUI"].units, 30.0, "the exact underlying was delivered");
    assert!(near(l.settlement, 970.0 + 200.0 - 1.0));
    assert!(near(l.nav(), 1_169.0 + 240.0));
    assert!(near(l.lines.option_payoff, 40.0));
    assert!(near(l.lines.gas, 1.0));
    assert_eq!(l.lines.exercises, 1);
    assert!(l.pending.is_empty());
    reconciled(&l);
}

/// The two flash put paths: borrowed inside the PTB, repaid exactly,
/// zero flash liability after; a plan that cannot repay aborts and
/// moves nothing.
#[test]
fn put_flash_paths_repay_exactly_and_a_short_repayment_aborts() {
    for path in [ExercisePath::PutBaseFlash, ExercisePath::PutQuoteFlash] {
        let mut l = Ledger::new(1_000.0);
        l.apply(&bought("p", spec(OptionKind::Put, 10.0, 1_000), 20.0, 30.0, 2.0, 1)).unwrap();
        let mut x = plan("p", path, 20.0);
        // Borrow 20 units (160 at 8), receive the 200 strike, buy 20 back
        // for 161, repay: +39 net.
        x.flash_borrowed = 160.0;
        x.flash_repaid = 160.0;
        x.settlement_in = 39.0;
        x.route_notional = 161.0;
        l.apply(&LedgerEvent::ExerciseSubmitted { op: 7, plan: x.clone(), at_ms: 2 }).unwrap();
        l.apply(&LedgerEvent::ExerciseSettled { op: 7, ok: true, actual: None, at_ms: 3 }).unwrap();
        assert_eq!(l.flash_outstanding, 0.0);
        assert!(near(l.settlement, 970.0 + 39.0));
        assert!(near(l.nav(), 1_009.0));
        assert!(near(l.lines.exercise_turnover_notional, 161.0));
        assert!(l.options.is_empty());
        reconciled(&l);

        // Short repayment: the PTB aborts atomically.
        let mut l = Ledger::new(1_000.0);
        l.apply(&bought("p", spec(OptionKind::Put, 10.0, 1_000), 20.0, 30.0, 2.0, 1)).unwrap();
        let mut bad = x.clone();
        bad.flash_repaid = 159.0;
        l.apply(&LedgerEvent::ExerciseSubmitted { op: 8, plan: bad, at_ms: 2 }).unwrap();
        let before = l.clone();
        assert_eq!(
            l.apply(&LedgerEvent::ExerciseSettled { op: 8, ok: true, actual: None, at_ms: 3 }),
            Err(LedgerError::FlashNotRepaid { borrowed: 160.0, repaid: 159.0 })
        );
        assert_eq!(l.settlement, before.settlement);
        assert_eq!(l.options["p"].qty, 20.0);
        assert_eq!(l.options["p"].pending_units, 0.0);
        assert_eq!(l.flash_outstanding, 0.0);
        assert_eq!(l.lines.exercise_failures, 1);
        assert!(l.pending.exercises.is_empty());
        reconciled(&l);
    }
}

/// Call exercise, cash path (the underlying stays) and flash path (the
/// underlying is sold inside the PTB).
#[test]
fn call_exercise_cash_and_flash_paths() {
    let mut l = Ledger::new(1_000.0);
    l.apply(&bought("c", spec(OptionKind::Call, 10.0, 1_000), 10.0, 15.0, 2.0, 1)).unwrap();
    let mut x = plan("c", ExercisePath::CallCash, 10.0);
    x.settlement_out = 100.0;
    x.underlying_in = 10.0;
    l.apply(&LedgerEvent::MarkUnderlying { asset: "SUI".into(), mark: 12.0, at_ms: 1 }).unwrap();
    l.apply(&LedgerEvent::ExerciseSubmitted { op: 1, plan: x, at_ms: 2 }).unwrap();
    assert_eq!(l.pending.committed_spend(), 100.0);
    assert!(near(l.available_capital(), 985.0 - 100.0));
    // Deliver at the 12 mark: a balance created by the exercise carries
    // the market mark the caller sets next.
    l.apply(&LedgerEvent::ExerciseSettled { op: 1, ok: true, actual: None, at_ms: 3 }).unwrap();
    l.apply(&LedgerEvent::MarkUnderlying { asset: "SUI".into(), mark: 12.0, at_ms: 3 }).unwrap();
    assert_eq!(l.underlying["SUI"].units, 10.0);
    assert!(near(l.settlement, 985.0 - 100.0));
    assert!(near(l.nav(), 885.0 + 120.0));
    assert!(l.options.is_empty());
    reconciled(&l);

    let mut l = Ledger::new(1_000.0);
    l.apply(&bought("c", spec(OptionKind::Call, 10.0, 1_000), 10.0, 15.0, 2.0, 1)).unwrap();
    let mut x = plan("c", ExercisePath::CallFlash, 10.0);
    x.flash_borrowed = 100.0;
    x.flash_repaid = 100.0;
    x.settlement_out = 100.0;
    x.settlement_in = 119.0;
    x.route_notional = 119.0;
    l.apply(&LedgerEvent::ExerciseSubmitted { op: 1, plan: x, at_ms: 2 }).unwrap();
    l.apply(&LedgerEvent::ExerciseSettled { op: 1, ok: true, actual: None, at_ms: 3 }).unwrap();
    assert!(near(l.settlement, 985.0 + 19.0));
    assert!(near(l.nav(), 1_004.0));
    assert!(near(l.lines.option_payoff, 19.0));
    assert!(near(l.lines.exercise_costs, 100.0));
    assert!(l.underlying.is_empty());
    reconciled(&l);
}

/// A failed PTB (`ok = false`) changes no balance; a resale does.
#[test]
fn failed_ptb_moves_nothing_and_resale_realizes() {
    let mut l = Ledger::new(1_000.0);
    l.apply(&bought("c", spec(OptionKind::Call, 10.0, 1_000), 10.0, 60.0, 6.0, 1)).unwrap();
    let mut x = plan("c", ExercisePath::CallCash, 4.0);
    x.settlement_out = 40.0;
    x.underlying_in = 4.0;
    l.apply(&LedgerEvent::ExerciseSubmitted { op: 1, plan: x, at_ms: 2 }).unwrap();
    let before = l.clone();
    l.apply(&LedgerEvent::ExerciseSettled { op: 1, ok: false, actual: None, at_ms: 3 }).unwrap();
    assert_eq!(l.settlement, before.settlement);
    assert_eq!(l.underlying, before.underlying);
    assert_eq!(l.options["c"].qty, 10.0);
    assert_eq!(l.options["c"].pending_units, 0.0);
    assert_eq!(l.lines.exercise_failures, 1);
    assert_eq!(l.lines.option_payoff, 0.0);
    reconciled(&l);
    // Resale of the whole line for 70 (mark 60): +10 realized.
    l.apply(&LedgerEvent::ResaleSubmitted { op: 2, option: "c".into(), qty: 10.0, expected_proceeds: 70.0, at_ms: 4 }).unwrap();
    assert!(matches!(
        l.apply(&LedgerEvent::ResaleSubmitted { op: 3, option: "c".into(), qty: 1.0, expected_proceeds: 7.0, at_ms: 4 }),
        Err(LedgerError::InsufficientUnits { .. })
    ));
    l.apply(&LedgerEvent::ResaleSettled { op: 2, proceeds: Some(70.0), at_ms: 5 }).unwrap();
    assert!(l.options.is_empty());
    assert!(near(l.settlement, 1_010.0));
    assert!(near(l.lines.option_exit, 10.0));
    assert_eq!(l.lines.resales, 1);
    reconciled(&l);
}

/// Hedge reversal: short 200 at avg 11, cover 50 at 9 (+100), buy 250
/// at 10 (closes 150 for +150, opens long 100 at 10); marked 11 the
/// long shows +100 unrealized. Realized + unrealized reconciles to the
/// traded cash flow across every fill.
#[test]
fn hedge_reversal_reconciles_realized_and_unrealized() {
    let mut l = Ledger::new(1_000.0);
    let m = "SUI".to_string();
    let fill = |op: u64, size: f64, px: f64| LedgerEvent::PerpFill {
        op: Some(op),
        market: m.clone(),
        size_units: size,
        price: px,
        fee: 0.0,
        reference: px,
        gas: 0.0,
        passive: false,
        partial: false,
        at_ms: op,
    };
    l.apply(&LedgerEvent::HedgeSubmitted { op: 1, market: m.clone(), size_units: -100.0, spot: 10.0, at_ms: 1 }).unwrap();
    assert_eq!(l.pending.hedges.len(), 1);
    l.apply(&fill(1, -100.0, 10.0)).unwrap();
    assert!(l.pending.hedges.is_empty());
    l.apply(&fill(2, -100.0, 12.0)).unwrap();
    let p = l.perps[&m];
    assert!(near(p.entry, 11.0) && p.units == -200.0);
    l.apply(&fill(3, 50.0, 9.0)).unwrap();
    assert!(near(l.perps[&m].realized, 100.0));
    l.apply(&fill(4, 250.0, 10.0)).unwrap();
    let p = l.perps[&m];
    assert!(near(p.realized, 250.0) && p.units == 100.0 && p.entry == 10.0);
    l.apply(&LedgerEvent::MarkPerp { market: m.clone(), mark: 11.0, at_ms: 5 }).unwrap();
    let p = l.perps[&m];
    assert!(near(p.unrealized(), 100.0));
    assert!(near(p.cash_flow, -750.0));
    assert!(near(l.settlement, 1_250.0));
    assert!(near(l.nav(), 1_350.0));
    assert_eq!(l.lines.hedge_fills, 4);
    assert!(near(l.lines.hedge_turnover_notional, 1_000.0 + 1_200.0 + 450.0 + 2_500.0));
    reconciled(&l);
    // Fees, gas and a passive fill land on their lines.
    l.apply(&LedgerEvent::PerpFill {
        op: None,
        market: m.clone(),
        size_units: -10.0,
        price: 11.0,
        fee: 0.5,
        reference: 11.0,
        gas: 0.1,
        passive: true,
        partial: true,
        at_ms: 6,
    })
    .unwrap();
    assert!(near(l.lines.maker_fees, 0.5) && near(l.lines.gas, 0.1) && l.lines.passive_fills == 1 && l.lines.partial_fills == 1);
    assert!(near(l.lines.hedge_realized, 260.0));
    reconciled(&l);
}

/// Funding is accrued against the signed position: a short RECEIVES
/// positive funding, a long PAYS it (doc 08 §1 item 4).
#[test]
fn funding_long_and_short() {
    for (units, paid) in [(-100.0, -1.0), (100.0, 1.0)] {
        let mut l = Ledger::new(1_000.0);
        l.apply(&LedgerEvent::PerpFill {
            op: None,
            market: "SUI".into(),
            size_units: units,
            price: 10.0,
            fee: 0.0,
            reference: 10.0,
            gas: 0.0,
            passive: false,
            partial: false,
            at_ms: 1,
        })
        .unwrap();
        // rate 0.001 × units × mark 10
        l.apply(&LedgerEvent::Funding { market: "SUI".into(), paid, at_ms: 2 }).unwrap();
        assert!(near(l.settlement, 1_000.0 - paid));
        assert!(near(l.lines.funding_paid, paid));
        assert!(near(l.perps["SUI"].funding_paid, paid));
        assert!(near(l.nav(), 1_000.0 - paid));
        reconciled(&l);
    }
}

/// Liquidation of a long at 10x: the mark P&L is realized at the
/// liquidation price, the remaining margin is forfeited, working orders
/// on the market are dropped.
#[test]
fn liquidation_forfeits_margin_and_reconciles() {
    let mut l = Ledger::new(1_000.0);
    let m = "SUI".to_string();
    l.apply(&LedgerEvent::HedgeSubmitted { op: 1, market: m.clone(), size_units: 100.0, spot: 10.0, at_ms: 1 }).unwrap();
    l.apply(&LedgerEvent::PerpFill {
        op: Some(1),
        market: m.clone(),
        size_units: 100.0,
        price: 10.0,
        fee: 0.0,
        reference: 10.0,
        gas: 0.0,
        passive: false,
        partial: false,
        at_ms: 1,
    })
    .unwrap();
    l.apply(&LedgerEvent::MarginMoved { market: m.clone(), amount: 100.0, at_ms: 1 }).unwrap();
    assert!(near(l.settlement, 900.0) && near(l.perp_collateral(), 100.0) && near(l.nav(), 1_000.0));
    l.apply(&LedgerEvent::HedgeSubmitted { op: 2, market: m.clone(), size_units: 5.0, spot: 10.0, at_ms: 2 }).unwrap();
    l.apply(&LedgerEvent::MarkPerp { market: m.clone(), mark: 9.2, at_ms: 3 }).unwrap();
    assert!(near(l.nav(), 920.0));
    reconciled(&l);
    l.apply(&LedgerEvent::Liquidation { market: m.clone(), size_closed: -100.0, price: 9.2, penalty: 20.0, full: true, at_ms: 4 }).unwrap();
    let p = l.perps[&m];
    assert_eq!(p.units, 0.0);
    assert_eq!(p.collateral, 0.0);
    assert!(near(p.realized, -80.0));
    assert!(near(l.settlement, 900.0));
    assert!(near(l.nav(), 900.0));
    assert!(near(l.lines.liquidation_loss, 20.0));
    assert_eq!(l.lines.liquidations, 1);
    assert!(l.pending.hedges.is_empty(), "the venue dropped the working order");
    reconciled(&l);
}

/// Margin in transit stays an asset; a rejected transfer returns.
#[test]
fn margin_topups_in_transit_land_or_return() {
    let mut l = Ledger::new(1_000.0);
    l.apply(&LedgerEvent::MarginTopUpSent { op: 1, market: "SUI".into(), amount: 30.0, at_ms: 1 }).unwrap();
    assert!(near(l.settlement, 970.0) && near(l.nav(), 1_000.0));
    assert_eq!(l.pending.margin_in_transit(), 30.0);
    reconciled(&l);
    l.apply(&LedgerEvent::MarginTopUpLanded { op: 1, accepted: true, at_ms: 2 }).unwrap();
    assert!(near(l.perps["SUI"].collateral, 30.0) && near(l.nav(), 1_000.0));
    assert_eq!((l.lines.margin_topups, l.lines.topup_total), (1, 30.0));
    l.apply(&LedgerEvent::MarginTopUpSent { op: 2, market: "SUI".into(), amount: 10.0, at_ms: 3 }).unwrap();
    l.apply(&LedgerEvent::MarginTopUpLanded { op: 2, accepted: false, at_ms: 4 }).unwrap();
    assert!(near(l.settlement, 970.0) && l.lines.topup_rejects == 1);
    assert_eq!(l.apply(&LedgerEvent::MarginTopUpLanded { op: 9, accepted: true, at_ms: 5 }), Err(LedgerError::UnknownOp(9)));
    reconciled(&l);
}

/// Queued withdrawals are a liability at their last valuation; an
/// unvaluable read keeps the last figure.
#[test]
fn queued_withdrawals_are_a_liability() {
    let mut l = Ledger::new(1_000.0);
    l.apply(&LedgerEvent::QueuedWithdrawals(QueuedWithdrawals { shares: 10.0, value: Some(50.0), observed_at_ms: 1 })).unwrap();
    assert!(near(l.liabilities(), 50.0) && near(l.nav(), 950.0));
    assert!(near(l.equity_flows.withdrawal_queue, -50.0));
    reconciled(&l);
    l.apply(&LedgerEvent::QueuedWithdrawals(QueuedWithdrawals { shares: 10.0, value: None, observed_at_ms: 2 })).unwrap();
    assert!(near(l.nav(), 950.0), "carried at the last valuation");
    assert_eq!(l.queued_withdrawals.shares, 10.0);
    l.apply(&LedgerEvent::QueuedWithdrawals(QueuedWithdrawals { shares: 0.0, value: Some(0.0), observed_at_ms: 3 })).unwrap();
    assert!(near(l.nav(), 1_000.0));
    reconciled(&l);
    // The external account is tracked with its freshness.
    l.apply(&LedgerEvent::External(ExternalAccount {
        exposure: 100.0,
        attested_equity: Some(98.0),
        attested_at_ms: Some(3),
        total_budget: 200.0,
        daily_release_limit: 100.0,
        daily_release_used: 40.0,
        window_start_ms: 0,
        observed_at_ms: 3,
    }))
    .unwrap();
    assert_eq!(l.external.budget_remaining(), 100.0);
    assert_eq!(l.external.daily_release_remaining(), 60.0);
    assert_eq!(l.external.equity_age_ms(10), Some(7));
    reconciled(&l);
}

/// Custody / venue truth: what the chain says that the ledger did not
/// predict lands on the resync lines, and the identity still holds.
#[test]
fn resync_books_unexplained_changes_to_equity_flows() {
    let mut l = Ledger::new(1_000.0);
    l.apply(&bought("c", spec(OptionKind::Call, 10.0, 1_000), 10.0, 60.0, 6.0, 1)).unwrap();
    // Chain says 8 units (2 left some other way) and 930 settlement.
    l.apply(&LedgerEvent::ResyncOptions {
        positions: vec![OptionSync { option: "c".into(), spec: spec(OptionKind::Call, 10.0, 1_000), qty: 8.0, mark_per_unit: Some(7.0) }],
        at_ms: 2,
    })
    .unwrap();
    l.apply(&LedgerEvent::ResyncBalances { settlement: Some(930.0), underlying: vec![], at_ms: 2 }).unwrap();
    assert!(near(l.lines.option_mark, 10.0), "the re-mark is P&L");
    assert!(near(l.equity_flows.resync_options, -14.0), "2 units at 7");
    assert!(near(l.equity_flows.resync_settlement, -10.0));
    assert!(near(l.equity_flows.residual(), -24.0));
    assert!(near(l.options["c"].cost_basis, 48.0), "cost basis pro rata");
    reconciled(&l);
    // A line the chain no longer holds is gone.
    l.apply(&LedgerEvent::ResyncOptions { positions: vec![], at_ms: 3 }).unwrap();
    assert!(l.options.is_empty());
    reconciled(&l);
    // Venue truth for the perp: units the ledger never saw filled come
    // in at the mark; a mismatch on the closed side realizes.
    l.apply(&LedgerEvent::ResyncPerp { market: "SUI".into(), units: -50.0, mark: Some(10.0), at_ms: 4 }).unwrap();
    assert_eq!(l.perps["SUI"].units, -50.0);
    l.apply(&LedgerEvent::ResyncPerp { market: "SUI".into(), units: -20.0, mark: Some(9.0), at_ms: 5 }).unwrap();
    assert!(near(l.perps["SUI"].realized, 30.0));
    reconciled(&l);
}

/// An expired line with an exercise in flight keeps a shell for the
/// pending units (the PTB landed before expiry, detection came later);
/// the rest is worthless.
#[test]
fn expiry_keeps_a_shell_for_units_in_flight() {
    let mut l = Ledger::new(1_000.0);
    l.apply(&bought("c", spec(OptionKind::Call, 10.0, 100), 10.0, 60.0, 6.0, 1)).unwrap();
    let mut x = plan("c", ExercisePath::CallFlash, 4.0);
    x.settlement_in = 8.0;
    l.apply(&LedgerEvent::ExerciseSubmitted { op: 1, plan: x, at_ms: 2 }).unwrap();
    l.apply(&LedgerEvent::ExpireOptions { at_ms: 100 }).unwrap();
    let p = &l.options["c"];
    assert_eq!((p.qty, p.pending_units, p.mark_per_unit), (4.0, 4.0, 0.0));
    assert!(near(l.lines.option_mark, -60.0));
    reconciled(&l);
    l.apply(&LedgerEvent::ExerciseSettled { op: 1, ok: true, actual: None, at_ms: 101 }).unwrap();
    assert!(l.options.is_empty());
    assert!(near(l.settlement, 948.0));
    reconciled(&l);
}

/// The invariant checker catches a deliberately corrupted ledger, one
/// invariant at a time.
#[test]
fn checker_catches_a_corrupted_ledger() {
    let mut base = Ledger::new(1_000.0);
    base.apply(&bought("c", spec(OptionKind::Call, 10.0, 1_000), 10.0, 60.0, 6.0, 1)).unwrap();
    base.apply(&LedgerEvent::PerpFill {
        op: None,
        market: "SUI".into(),
        size_units: -5.0,
        price: 10.0,
        fee: 0.0,
        reference: 10.0,
        gas: 0.0,
        passive: false,
        partial: false,
        at_ms: 2,
    })
    .unwrap();
    base.apply(&LedgerEvent::Reserve(reservation("r", 100, true, 200, 3))).unwrap();
    reconciled(&base);

    let names = |l: &Ledger| l.check().into_iter().map(|v| v.invariant).collect::<Vec<_>>();
    let mut l = base.clone();
    l.settlement += 1.0;
    assert_eq!(names(&l), vec!["assets − liabilities = NAV"]);
    let mut l = base.clone();
    l.perps.get_mut("SUI").unwrap().realized += 1.0;
    assert!(names(&l).contains(&"perp realized + unrealized = traded cash flow + units × mark"));
    let mut l = base.clone();
    l.options.get_mut("c").unwrap().pending_units = 11.0;
    assert!(names(&l).contains(&"option quantities are non-negative and cover their pending units"));
    let mut l = base.clone();
    l.flash_outstanding = 5.0;
    assert!(names(&l).contains(&"flash liabilities are zero between events"));
    let mut l = base.clone();
    l.settlement = 50.0;
    l.nav0 -= 890.0;
    assert_eq!(names(&l), vec!["reservations + committed spend ≤ available capital"]);
    let mut l = base.clone();
    l.perps.get_mut("SUI").unwrap().collateral = -1.0;
    l.settlement += 1.0;
    assert!(names(&l).contains(&"perp collateral is non-negative"));
    let mut l = base.clone();
    l.pending.exercises.insert(9, PendingExercise { plan: plan("c", ExercisePath::CallCash, 3.0), submitted_ms: 0 });
    assert!(names(&l).contains(&"pending exercises are backed by their option line"));
    assert!(base.verify().is_ok());
    assert!(l.verify().is_err());
}

/// Deterministic and serializable: the same events give a byte-identical
/// JSON ledger, which round-trips.
#[test]
fn deterministic_and_serde_round_trips() {
    let run = || {
        let mut l = Ledger::new(1_000.0);
        l.apply(&LedgerEvent::Reserve(reservation("b", 10, false, 100, 1))).unwrap();
        l.apply(&LedgerEvent::Reserve(reservation("a", 10, true, 200, 1))).unwrap();
        l.apply(&bought("c", spec(OptionKind::Call, 10.0, 100), 10.0, 60.0, 6.0, 2)).unwrap();
        l.apply(&LedgerEvent::HedgeSubmitted { op: 1, market: "SUI".into(), size_units: -5.0, spot: 10.0, at_ms: 3 }).unwrap();
        l
    };
    let a = serde_json::to_string(&run()).unwrap();
    assert_eq!(a, serde_json::to_string(&run()).unwrap());
    let back: Ledger = serde_json::from_str(&a).unwrap();
    assert_eq!(back, run());
    assert!(a.find("\"a\"").unwrap() < a.find("\"b\"").unwrap(), "sorted keys");
}
