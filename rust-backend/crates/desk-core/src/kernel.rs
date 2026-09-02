//! `DeskKernel` — the one deterministic strategy kernel the live desk
//! and the backtester both drive (doc 08 §2, SO-450).
//!
//! ```text
//! external event ──▶ DeskKernel::on_event ──▶ Vec<Command>
//! ```
//!
//! Every input the desk's I/O tasks used to read for themselves — the
//! oracle, the venue, the chain, the clock — arrives as an [`Event`]
//! carrying its own timestamp; every side effect the tasks used to
//! perform leaves as a [`Command`] the adapters execute. The kernel owns
//! the book, the exposure, the per-market hedge state and the policy
//! flags, so the same event trace yields the same command sequence under
//! the live adapters (`services/mm-bot`) and the simulation adapters
//! (`desk-backtester`) — the P1 parity gate.
//!
//! What the handlers replicate, line for line, is the live desk as of
//! SO-447: `Desk::price_ws_rfq` ([`Event::Rfq`]), the book refresher
//! ([`Event::MarkUpdate`]), `Rebalancer::rebalance_once`
//! ([`Event::HedgeTick`] + [`Event::Hedge`] + [`Event::FundingSettled`] +
//! [`Event::HedgeRealized`]), the fill poller ([`Event::QuoteFilled`])
//! and the exits ladder ([`Event::ExitTimer`] + [`Event::PutLiquidity`]).
//! Same quotes, same hedges, same declines, same rows.

use std::collections::HashMap;

use protocol_types::ids::ObjectId;
use protocol_types::sides::Side;

use crate::book::{
    Book, DetectedFill, FillLink, FillSide, Holding, PnlLine, PnlRecord, QuoteReservation,
    Reservation, ReserveError, Written,
};
use crate::exits::put::{self, PoolLiquidity, PutLiquidity, PutPath, PutPlan};
use crate::exits::{self, ExitAction, ExitsConfig};
use crate::exposure::{self, MarkInputs, MarkSnapshot, SpotSnapshot};
use crate::hedge::{self, HedgeConfig, HedgeEvent, HedgeOrder, OpenOrders, OrderId};
use crate::limits::{
    self, BookExposure, CapitalConfig, CapitalInputs, ExternalInputs, KillSwitchState,
    LimitsConfig, VenueMarginInputs,
};
use crate::model::{MarketModel, V1BidParams};
use crate::quote::{self, Decision, FlowContext, RfqInputs};

// ── config ─────────────────────────────────────────────────────────────

/// Everything the kernel's policies are parameterized by — the pure
/// slice of `[desk]` (mm-bot's `DeskConfig` derives it).
#[derive(Clone, Debug)]
pub struct KernelConfig {
    pub v1: V1BidParams,
    pub limits: LimitsConfig,
    pub capital: CapitalConfig,
    pub hedge: HedgeConfig,
    pub exits: ExitsConfig,
    /// Signed-quote TTL; a reservation outlives it by
    /// `capital.reservation_grace_secs`.
    pub quote_ttl_ms: u64,
    pub expected_holding_years: f64,
    /// The monitors' stress gaps as positive fractions (`|gap|`).
    pub stress_gap_down: f64,
    pub stress_gap_up: f64,
    /// Primary hedge venue slippage (the bid's venue cost input).
    pub primary_slippage_bps: f64,
    pub settlement_decimals: u8,
    /// Whether curator-session (vault-custody) flows are wired: offset
    /// closes and vault-custody exercise. Without them the exit ladder
    /// holds vault units exactly as the live task does.
    pub curator_session: bool,
    /// Whether the deepbook-adapter repurchase (vault-custody put
    /// exercise) is wired.
    pub deepbook_adapter: bool,
}

// ── events (doc 08 §2.1) ───────────────────────────────────────────────

/// One per-market hedge venue outcome.
pub type MarketIndex = usize;

/// The refresher tick's observations: the chain and indexer reads the
/// mark pass and the capital snapshot are built from.
#[derive(Clone, Debug, Default)]
pub struct MarkUpdate {
    pub at_ms: u64,
    /// Custody re-sync (held coins + written positions), when the tick
    /// re-read them.
    pub custody: Option<(Vec<Holding>, Vec<Written>)>,
    /// Budget base (`book::budget_base`), `None` when unreadable.
    pub nav: Option<u64>,
    pub appraisal_at: Option<u64>,
    /// The vault's risk-off state from the indexer view, `None` when the
    /// view was unreachable (state kept).
    pub risk_off: Option<bool>,
    /// Fresh spot per model (`None` = stale).
    pub spot_by_model: Vec<Option<f64>>,
    pub free_settlement: f64,
    pub free_underlying_by_asset: HashMap<String, f64>,
    pub external: Option<ExternalInputs>,
    pub queued_withdrawal_value: Option<f64>,
}

/// Which custody leg a put-exercise liquidity read / PTB is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutLeg {
    /// Wallet float coins (the three wallet routes).
    Wallet,
    /// One VaultMm coin-custody position (vault-underlying route only).
    VaultCoin { position_id: ObjectId },
}

/// Which call-exercise PTB to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallLeg {
    /// `bucket::exercise` funded from wallet settlement cash.
    WalletCash { amount: u64, strike_cost: u64 },
    /// The DeepBook flash-loan ladder over the wallet float.
    WalletFlash { amount: u64 },
    /// `vault_mm::exercise_call_coin` over the coin-custody positions.
    VaultCoins,
}

/// A policy state the kernel activates / clears (doc 08 §2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyState {
    KillSwitch,
    RiskOff,
    StressBlocked,
    LegacyWrittenInventory,
}

/// Events into the kernel (doc 08 §2.1). Every event carries the time
/// it is actionable at; the kernel has no clock.
#[derive(Clone, Debug)]
pub enum Event {
    /// Oracle observation: a fresh spot for `market`.
    Spot { market: MarketIndex, spot: f64, at_ms: u64 },
    /// Staleness transition: the market's feed is unusable until the
    /// next `Spot`.
    SpotStale { market: MarketIndex, at_ms: u64 },
    /// Earn RFQ arrival. `reserve = Some(nonce)` for a signed WS quote
    /// (reserves premium under `request_id`), `None` for an indicative
    /// view or the auction channel's max-bid pricing.
    Rfq {
        request_id: String,
        side: Side,
        market: MarketIndex,
        inputs: RfqInputs,
        spot: f64,
        reserve: Option<u64>,
        at_ms: u64,
    },
    /// The signed quote left for the taker.
    QuoteSent { request_id: String, at_ms: u64 },
    /// The taker took the quote (execution submitted, not yet on chain).
    QuoteAccepted { request_id: String, at_ms: u64 },
    /// The quote's TTL elapsed with no detected fill.
    QuoteExpired { request_id: String, at_ms: u64 },
    /// The quote never reached the taker / its execution failed.
    QuoteReverted { request_id: String, at_ms: u64 },
    /// A chain fill the desk participated in, with the model fair TOTAL
    /// premium at detection (`book::apply_fills`' documented
    /// fair-at-detection approximation).
    QuoteFilled { fill: DetectedFill, fair_total: f64, at_ms: u64 },
    /// Hedge order acknowledged / partially filled / filled / rejected /
    /// cancelled.
    Hedge { market: MarketIndex, event: HedgeEvent, at_ms: u64 },
    /// The rebalancer's venue readback this tick: signed position and
    /// the venue's own funding rate (the band input).
    HedgeTick { market: MarketIndex, position_units: f64, funding_rate_annual: f64, at_ms: u64 },
    /// The venue's cumulative realized P&L after this tick's order.
    HedgeRealized { market: MarketIndex, realized_pnl: f64, at_ms: u64 },
    /// Funding settled on the signed position (positive = paid).
    FundingSettled { market: MarketIndex, paid: f64, at_ms: u64 },
    /// Mark / margin update — the refresher tick.
    MarkUpdate(Box<MarkUpdate>),
    /// The monitors' venue-roster read: margin picture + the
    /// notional-weighted funding the bid prices with.
    VenueMargin { inputs: VenueMarginInputs, funding_weighted: f64, at_ms: u64 },
    /// The venue liquidated (part of) the position.
    Liquidation { market: MarketIndex, position_units_after: f64, at_ms: u64 },
    /// Holding / exercise / expiry timer: run the exit ladder.
    ExitTimer { wallet_cash: u64, at_ms: u64 },
    /// The spot-pool liquidity a put exercise plans against, per leg
    /// (answers [`Command::QueryPutLiquidity`]).
    PutLiquidity {
        bucket: ObjectId,
        wallet_underlying: u64,
        vault_underlying: u64,
        pool: PoolLiquidity,
        at_ms: u64,
    },
    /// Exercise PTB success or failure.
    ExercisePtbResult { bucket: ObjectId, leg: PutLeg, units: u64, ok: bool, at_ms: u64 },
    /// Margin top-up success or failure.
    MarginTopUpResult { market: MarketIndex, amount: f64, ok: bool, at_ms: u64 },
    /// NAV sample (kill-switch input).
    NavSample { nav: u64, at_ms: u64 },
    /// An externally driven policy transition (operator / monitors).
    Policy { state: PolicyState, active: bool, at_ms: u64 },
}

// ── commands (doc 08 §2.2) ─────────────────────────────────────────────

/// Commands out of the kernel (doc 08 §2.2). Adapters execute them in
/// order; none is ever executed by the kernel itself.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Sign and send a quote.
    Quote {
        request_id: String,
        premium: u64,
        model_fair: f64,
        surface_vol: f64,
        hedge_notional: f64,
        exercise_cash: f64,
    },
    Decline { request_id: String, reason: String },
    /// Persist a live (`quoted` / `accepted`) reservation row.
    ReservePremium(Reservation),
    /// Persist a terminal (`reverted` / `expired` / `filled`) reservation
    /// row — the capacity is free again.
    ReleasePremium(Reservation),
    SubmitHedgeOrder { market: MarketIndex, order: HedgeOrder },
    CancelHedgeOrder { market: MarketIndex, id: OrderId },
    ReplaceHedgeOrder { market: MarketIndex, old: OrderId, new: HedgeOrder },
    /// Rest an ask for `units` of `bucket` on the exchange. Reserved:
    /// the listings engine still decides asks itself (SO-416).
    ListOption { bucket: ObjectId, units: u64 },
    /// Resell `units` of `bucket` at the taker. Reserved (no taker
    /// resale rung exists since SO-416).
    ResellOption { bucket: ObjectId, units: u64 },
    /// Net a written position against same-bucket custodied coins.
    OffsetClose {
        bucket: ObjectId,
        position_id: ObjectId,
        coin_position_id: ObjectId,
        amount: u64,
        is_put: bool,
    },
    ExecuteCallPtb { bucket: ObjectId, leg: CallLeg },
    /// Read the spot-pool liquidity for a wanted put exercise.
    QueryPutLiquidity { bucket: ObjectId, market: MarketIndex },
    /// Run one of the three put PTBs (`plan.path`) for one slice.
    ExecutePutPtb { bucket: ObjectId, leg: PutLeg, plan: PutPlan },
    /// Post margin at the venue. Reserved: no live policy sizes top-ups
    /// yet (the monitors alert on headroom).
    TopUpMargin { market: MarketIndex, amount: f64 },
    ActivatePolicy(PolicyState),
    ClearPolicy(PolicyState),
    /// Sink one P&L attribution record (metrics + JSONL).
    RecordPnl(PnlRecord),
}

// ── state ──────────────────────────────────────────────────────────────

/// Per-market hedge and market-data state.
#[derive(Debug, Default)]
pub struct MarketState {
    pub spot: Option<SpotSnapshot>,
    /// Net book delta for this underlying (the mark pass writes it).
    pub book_delta_units: f64,
    /// Last signed hedge position the venue reported (the bid reads it —
    /// SO-437).
    pub hedge_position_units: f64,
    /// Venue realized P&L already attributed to the scalp line.
    pub last_realized_pnl: f64,
    /// Process-local order ids (unique per run).
    pub next_order_id: OrderId,
    /// Working orders (SO-438).
    pub open_orders: OpenOrders,
    /// Puts whose exercise is wanted and awaiting a liquidity read:
    /// bucket → (holding snapshot, spot, t_years) at decision time.
    pending_put_exercise: HashMap<ObjectId, (Holding, f64, u64)>,
}

/// The kernel: state + policy. Construct with [`DeskKernel::new`], seed
/// what the live desk seeds at boot (reservations, kill-switch history,
/// venue realized P&L), then drive it with events.
pub struct DeskKernel {
    pub cfg: KernelConfig,
    pub models: Vec<MarketModel>,
    pub book: Book,
    pub exposure: BookExposure,
    pub marks: HashMap<ObjectId, MarkSnapshot>,
    /// Per-symbol spot from the last mark pass.
    pub spots: HashMap<String, SpotSnapshot>,
    pub markets: Vec<MarketState>,
    pub kill_switch: KillSwitchState,
    /// Notional-weighted funding across venues — the bid's input
    /// (seeded from the primary venue's rate, the monitors overwrite).
    pub funding_rate_annual: f64,
    pub venue_margin: VenueMarginInputs,
    pub risk_off: bool,
    pub stress_blocked: bool,
    /// Legacy written inventory the last mark pass lifted off the book
    /// (SO-426): nonzero blocks quoting.
    pub naked_written_units: u64,
    hedge_cost: pricing::desk::HedgeCostParams,
    last_theta_accrual_ms: u64,
}

impl DeskKernel {
    pub fn new(
        cfg: KernelConfig,
        models: Vec<MarketModel>,
        book: Book,
        primary_funding_rate_annual: f64,
        risk_off: bool,
        booted_at_ms: u64,
    ) -> Self {
        let markets = models.iter().map(|_| MarketState::default()).collect();
        let hedge_cost = cfg.hedge.cost_params(cfg.primary_slippage_bps);
        Self {
            cfg,
            models,
            book,
            exposure: BookExposure::default(),
            marks: HashMap::new(),
            spots: HashMap::new(),
            markets,
            kill_switch: KillSwitchState::default(),
            funding_rate_annual: primary_funding_rate_annual,
            venue_margin: VenueMarginInputs::default(),
            risk_off,
            stress_blocked: false,
            naked_written_units: 0,
            hedge_cost,
            last_theta_accrual_ms: booted_at_ms,
        }
    }

    /// Seed the scalp high-water mark from the venue so a restart does
    /// not re-attribute the whole persisted realized P&L as fresh scalp.
    pub fn seed_hedge_realized(&mut self, market: MarketIndex, realized_pnl: f64) {
        self.markets[market].last_realized_pnl = realized_pnl;
    }

    /// The cross-flow context one RFQ prices under (mirrors
    /// `DeskShared::flow_context`).
    pub fn flow_context(&self, market: MarketIndex, spot: f64) -> FlowContext {
        FlowContext {
            spot,
            exposure: self.exposure.clone(),
            funding_rate_annual: self.funding_rate_annual,
            expected_holding_years: self.cfg.expected_holding_years,
            hedge_position_units: self.markets[market].hedge_position_units,
            hedge_cost: self.hedge_cost,
        }
    }

    /// Apply one event; the returned commands are everything the
    /// adapters must now do, in order.
    pub fn on_event(&mut self, ev: Event) -> Vec<Command> {
        let mut out = Vec::new();
        match ev {
            Event::Spot { market, spot, at_ms } => {
                self.markets[market].spot = Some(SpotSnapshot { spot, at_ms });
            }
            Event::SpotStale { market, .. } => {
                self.markets[market].spot = None;
            }
            Event::Rfq { request_id, side, market, inputs, spot, reserve, at_ms } => {
                self.on_rfq(&mut out, request_id, side, market, inputs, spot, reserve, at_ms);
            }
            Event::QuoteSent { .. } => {}
            Event::QuoteAccepted { request_id, at_ms } => {
                self.book.accept_reservation(&request_id, at_ms);
                self.drain_reservations(&mut out);
            }
            Event::QuoteExpired { request_id, at_ms } => {
                self.book.expire_reservation(&request_id, at_ms);
                self.drain_reservations(&mut out);
            }
            Event::QuoteReverted { request_id, at_ms } => {
                self.book.revert_reservation(&request_id, at_ms);
                self.drain_reservations(&mut out);
            }
            Event::QuoteFilled { fill, fair_total, at_ms } => {
                self.on_fill(&mut out, &fill, fair_total, at_ms);
            }
            Event::Hedge { market, event, .. } => {
                self.markets[market].open_orders.apply(&event);
            }
            Event::HedgeTick { market, position_units, funding_rate_annual, at_ms } => {
                self.on_hedge_tick(&mut out, market, position_units, funding_rate_annual, at_ms);
            }
            Event::HedgeRealized { market, realized_pnl, at_ms } => {
                // Long-gamma rebalancing sells high / buys low: realized
                // hedge P&L is the scalp line.
                let m = &mut self.markets[market];
                let scalp = realized_pnl - m.last_realized_pnl;
                m.last_realized_pnl = realized_pnl;
                if scalp != 0.0 {
                    self.book.record_pnl(PnlLine::Scalp, scalp, "hedge rebalance", at_ms);
                    self.drain_pnl(&mut out);
                }
            }
            Event::FundingSettled { paid, at_ms, .. } => {
                // Funding accrues on the signed position as its own P&L
                // line — never through the fills-only realized figure.
                if paid != 0.0 {
                    self.book.record_pnl(PnlLine::Funding, -paid, "hedge funding accrual", at_ms);
                    self.drain_pnl(&mut out);
                }
            }
            Event::MarkUpdate(u) => self.on_mark_update(&mut out, *u),
            Event::VenueMargin { inputs, funding_weighted, .. } => {
                self.funding_rate_annual = funding_weighted;
                self.venue_margin = inputs;
            }
            Event::Liquidation { market, position_units_after, .. } => {
                // The venue closed (part of) the position and every
                // working order with it.
                let m = &mut self.markets[market];
                m.hedge_position_units = position_units_after;
                m.open_orders = OpenOrders::default();
            }
            Event::ExitTimer { wallet_cash, at_ms } => self.on_exit_timer(&mut out, wallet_cash, at_ms),
            Event::PutLiquidity { bucket, wallet_underlying, vault_underlying, pool, at_ms } => {
                self.on_put_liquidity(&mut out, bucket, wallet_underlying, vault_underlying, pool, at_ms);
            }
            Event::ExercisePtbResult { bucket, leg, units, ok, at_ms } => {
                self.on_exercise_result(&mut out, bucket, leg, units, ok, at_ms);
            }
            Event::MarginTopUpResult { .. } => {
                // No top-up policy exists yet (the monitors only alert);
                // the result is recorded by the adapter.
            }
            Event::NavSample { nav, at_ms } => {
                let tripped = self.kill_switch.check(&self.cfg.limits, nav, at_ms);
                transition(&mut out, PolicyState::KillSwitch, self.exposure.kill_switch, tripped);
                self.exposure.kill_switch = tripped;
            }
            Event::Policy { state, active, .. } => {
                let was = match state {
                    PolicyState::KillSwitch => std::mem::replace(&mut self.exposure.kill_switch, active),
                    PolicyState::RiskOff => std::mem::replace(&mut self.risk_off, active),
                    PolicyState::StressBlocked => std::mem::replace(&mut self.stress_blocked, active),
                    PolicyState::LegacyWrittenInventory => {
                        let was = self.naked_written_units > 0;
                        if !active {
                            self.naked_written_units = 0;
                        }
                        was
                    }
                };
                transition(&mut out, state, was, active);
            }
        }
        out
    }

    // ── RFQ (mirrors `Desk::price_ws_rfq`) ─────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn on_rfq(
        &mut self,
        out: &mut Vec<Command>,
        request_id: String,
        side: Side,
        market: MarketIndex,
        inputs: RfqInputs,
        spot: f64,
        reserve: Option<u64>,
        now_ms: u64,
    ) {
        let decision = self.decide_rfq(side, market, inputs, spot, reserve.map(|n| (request_id.as_str(), n)), now_ms);
        match decision {
            Decision::Quote { premium, model_fair, surface_vol, hedge_notional, exercise_cash } => {
                out.push(Command::Quote {
                    request_id,
                    premium,
                    model_fair,
                    surface_vol,
                    hedge_notional,
                    exercise_cash,
                });
                self.drain_reservations(out);
            }
            Decision::Decline { reason } => out.push(Command::Decline { request_id, reason }),
        }
    }

    /// Price one RFQ — the exact decision `Desk::price_ws_rfq` makes.
    /// `Side::Writer` = retail writes (the desk buys). `Side::Trader` =
    /// retail buys — the desk NEVER writes options (SO-426, doc 08
    /// §4.1), so trader RFQs always decline. With `reserve`, a
    /// writer-flow quote reserves its premium under the request id for
    /// the quote TTL plus the fill-detection grace (the transition is
    /// queued for the durable ledger).
    pub fn decide_rfq(
        &mut self,
        side: Side,
        market: MarketIndex,
        inputs: RfqInputs,
        spot: f64,
        reserve: Option<(&str, u64)>,
        now_ms: u64,
    ) -> Decision {
        // SO-418 risk gate: every signed quote routes collateral through
        // `vault_mm::release`, which aborts (code 124) whenever the vault
        // is risk-off — decline before pricing, reserving, or signing.
        if self.risk_off {
            return Decision::Decline {
                reason: "vault risk-off (capital risk state / commitment breach)".into(),
            };
        }
        // Legacy written inventory is a migration problem, not a
        // strategy: surface it and block new quoting until it is
        // unwound (doc 08 §4.1 gate).
        let naked = self.naked_written_units;
        if naked > 0 {
            return Decision::Decline {
                reason: format!(
                    "legacy written inventory present ({naked} naked units); quoting blocked until unwound"
                ),
            };
        }
        let ctx = self.flow_context(market, spot);
        match side {
            Side::Writer => {
                let d = quote::price_writer_flow(
                    &self.models[market],
                    &self.cfg.v1,
                    &self.cfg.limits,
                    &ctx,
                    &inputs,
                    now_ms,
                );
                if let (
                    Some((request_id, nonce)),
                    Decision::Quote { premium, hedge_notional, exercise_cash, .. },
                ) = (reserve, &d)
                {
                    // Reserve the premium while the quote is live (plus
                    // the fill-detection grace); a detected fill, a
                    // revert, or TTL expiry closes it.
                    let ttl_ms = self
                        .cfg
                        .quote_ttl_ms
                        .saturating_add(self.cfg.capital.reservation_grace_secs.saturating_mul(1000));
                    let res = self.book.reserve_quote(
                        QuoteReservation {
                            key: request_id.to_string(),
                            nonce: Some(nonce),
                            amount: *premium,
                            is_put: inputs.is_put,
                            expiry_ms: inputs.expiry_ms,
                            exercise_cash: *exercise_cash,
                            hedge_notional: *hedge_notional,
                            ttl_ms,
                        },
                        now_ms,
                    );
                    match res {
                        Ok(()) => {}
                        Err(ReserveError::ExceedsNav) => {
                            return Decision::Decline {
                                reason: "reservation ledger full (reservations + deployed ≥ NAV)"
                                    .into(),
                            };
                        }
                        Err(ReserveError::DuplicateKey) => {
                            return Decision::Decline {
                                reason: "duplicate request id: a live reservation already holds it"
                                    .into(),
                            };
                        }
                    }
                }
                d
            }
            Side::Trader => Decision::Decline {
                reason: "desk does not write options (long-only strategy)".into(),
            },
        }
    }

    /// The model's vol for one market at a point on the surface — the
    /// input the moneyness guard sizes its band from.
    pub fn model_sigma(&self, market: MarketIndex, spot: f64, strike: f64, t_years: f64) -> f64 {
        self.models[market].sigma(spot, strike, t_years).0
    }

    // ── fills (mirrors the fill poller) ────────────────────────────────

    fn on_fill(&mut self, out: &mut Vec<Command>, f: &DetectedFill, fair_total: f64, now_ms: u64) {
        let (spread, label) = match f.side {
            FillSide::Bought => (fair_total - f.premium as f64, "bought"),
            FillSide::Wrote => (f.premium as f64 - fair_total, "wrote"),
        };
        let note = format!(
            "fill seq={} bucket={} {} amount={} premium={}",
            f.sequence,
            f.bucket_id.to_hex(),
            label,
            f.amount,
            f.premium
        );
        self.book.record_pnl(PnlLine::Spread, spread, &note, now_ms);
        self.drain_pnl(out);
        // A chain fill closes the quote's reservation (SO-444): ground
        // truth, idempotent when it is already closed.
        if let FillLink::WsQuote { nonce } = &f.link {
            self.book.fill_reservation_by_nonce(*nonce, now_ms);
        }
        self.drain_reservations(out);
    }

    // ── hedge (mirrors `Rebalancer::rebalance_once`) ───────────────────

    fn on_hedge_tick(
        &mut self,
        out: &mut Vec<Command>,
        market: MarketIndex,
        position_units: f64,
        funding_rate_annual: f64,
        now_ms: u64,
    ) {
        let timeout_ms = self.cfg.hedge.order_timeout_secs.max(1) * 1000;
        let m = &mut self.markets[market];
        for id in m.open_orders.stale(now_ms, timeout_ms) {
            // The venue acknowledges the cancel synchronously (paper) or
            // through its event stream; either way the remainder no
            // longer counts toward the working size.
            m.open_orders.apply(&HedgeEvent::Cancelled(id));
            out.push(Command::CancelHedgeOrder { market, id });
        }
        let Some(spot) = m.spot.map(|s| s.spot) else {
            return;
        };
        m.hedge_position_units = position_units;
        let nav = self.exposure.nav;
        let band = hedge::band_units(&self.cfg.hedge, nav, spot, funding_rate_annual);
        if let Some(size) = hedge::plan_hedge_order(
            m.book_delta_units,
            position_units,
            m.open_orders.working_units(),
            band,
        ) {
            m.next_order_id += 1;
            let order = HedgeOrder { id: m.next_order_id, size_units: size, spot };
            m.open_orders.submit(&order, now_ms);
            out.push(Command::SubmitHedgeOrder { market, order });
        }
    }

    // ── mark / margin update (mirrors the book refresher) ──────────────

    fn on_mark_update(&mut self, out: &mut Vec<Command>, u: MarkUpdate) {
        let now = u.at_ms;
        if let Some((holdings, written)) = u.custody {
            self.book.holdings = holdings;
            self.book.written = written;
            self.book.recompute_covered();
        }
        if let Some(now_off) = u.risk_off {
            transition(out, PolicyState::RiskOff, self.risk_off, now_off);
            self.risk_off = now_off;
        }
        for (mi, m) in self.models.iter().enumerate() {
            if let Some(spot) = u.spot_by_model.get(mi).copied().flatten() {
                self.spots.insert(m.symbol.clone(), SpotSnapshot { spot, at_ms: now });
                self.markets[mi].spot = Some(SpotSnapshot { spot, at_ms: now });
            }
        }
        let pass = exposure::mark_book(MarkInputs {
            models: &self.models,
            holdings: &self.book.holdings,
            written: &self.book.written,
            spot_by_model: &u.spot_by_model,
            now_ms: now,
            stress_gap_down: self.cfg.stress_gap_down,
            stress_gap_up: self.cfg.stress_gap_up,
            quote_flash_capacity: self.cfg.capital.quote_flash_capacity,
            base_flash_capacity: self.cfg.capital.base_flash_capacity,
        });
        let mut exposure = pass.exposure;
        self.marks = pass.marks;

        // Theta accrual → P&L attribution.
        let dt_days = now.saturating_sub(self.last_theta_accrual_ms) as f64 / 86_400_000.0;
        self.last_theta_accrual_ms = now;
        if let Some(nav) = u.nav {
            self.book.nav = nav;
        }
        self.book.deployed = pass.deployed.max(0.0) as u64;
        self.book.expire_reservations(now);
        if dt_days > 0.0 && exposure.theta_cost_per_day > 0.0 {
            self.book.record_pnl(PnlLine::Theta, -exposure.theta_cost_per_day * dt_days, "accrual", now);
        }
        exposure.nav = self.book.nav as f64;
        exposure.reserved = self.book.reserved_total() as f64;
        exposure.premium_deployed = self.book.deployed as f64;
        let naked = self.book.naked_written_units();
        transition(out, PolicyState::LegacyWrittenInventory, self.naked_written_units > 0, naked > 0);
        self.naked_written_units = naked;
        let reserved = self.book.reserved_split();
        exposure.capital = limits::build_capital_snapshot(
            &self.cfg.capital,
            CapitalInputs {
                now_ms: now,
                appraised_nav: u.nav.map(|n| n as f64),
                appraisal_at: u.appraisal_at,
                free_settlement: u.free_settlement,
                free_underlying_by_asset: u.free_underlying_by_asset,
                premium_deployed: exposure.premium_deployed,
                call_premium_marked: exposure.call_premium,
                put_premium_marked: exposure.put_premium,
                premium_by_expiry_marked: &exposure.premium_by_expiry,
                call_strike_cash_marked: pass.call_strike_cash,
                put_underlying_value_marked: pass.put_underlying_value,
                exercise_demand_by_expiry_marked: &pass.exercise_demand_by_expiry,
                hedge_notional_marked: pass.hedge_notional,
                hedge_notional_by_expiry_marked: &pass.hedge_notional_by_expiry,
                reserved: &reserved,
                queued_withdrawal_value: u.queued_withdrawal_value,
                external: u.external,
                venue: self.venue_margin,
                initial_margin_fraction: self.cfg.hedge.initial_margin_fraction,
                stress_gap: self.cfg.stress_gap_down.max(self.cfg.stress_gap_up),
            },
        );
        self.drain_pnl(out);
        self.drain_reservations(out);
        let tripped = self.kill_switch.check(&self.cfg.limits, exposure.nav as u64, now);
        transition(out, PolicyState::KillSwitch, self.exposure.kill_switch, tripped);
        exposure.kill_switch = tripped;
        for (mi, m) in self.models.iter().enumerate() {
            self.markets[mi].book_delta_units =
                pass.delta_by_coin.get(&m.coin_type).copied().unwrap_or(0.0);
        }
        self.exposure = exposure;
    }

    // ── exits (mirrors `exits::tick`) ──────────────────────────────────

    fn on_exit_timer(&mut self, out: &mut Vec<Command>, mut wallet_cash: u64, now: u64) {
        let holdings = self.book.holdings.clone();
        let written = self.book.written.clone();
        for h in holdings {
            if h.amount() == 0 || h.expiry_ms <= now {
                continue;
            }
            let Some(mi) = self.models.iter().position(|m| m.coin_type == h.asset_coin_type) else {
                continue;
            };
            let Some(spot) = self.markets[mi].spot.map(|s| s.spot) else {
                continue;
            };

            // Step 0 (netting): a written position + same-bucket VaultMm
            // coin custody offset-close at zero market impact. One tx per
            // holding per tick; the custody re-sync picks up the shrunk
            // amounts.
            if self.cfg.exits.offset_close_enabled && self.cfg.curator_session {
                if let (Some(w), Some(cp)) = (
                    written.iter().find(|w| w.bucket_id == h.bucket_id && w.amount > 0),
                    h.coin_positions.first(),
                ) {
                    out.push(Command::OffsetClose {
                        bucket: h.bucket_id,
                        position_id: w.position_id,
                        coin_position_id: cp.position_id,
                        amount: w.amount.min(cp.amount),
                        is_put: h.is_put,
                    });
                    continue; // custody changed under us; re-ladder next tick
                }
            }

            let cost_wallet = exits::strike_cost(h.amount_wallet, h.strike, h.strike_scale);
            let action = exits::decide_exit(
                &self.cfg.exits,
                &self.models[mi],
                h.is_put,
                spot,
                h.strike_scaled(),
                h.expiry_ms,
                wallet_cash,
                cost_wallet,
                now,
            );
            if action == ExitAction::Hold {
                continue;
            }
            if action == ExitAction::ExercisePut {
                // The waterfall needs the spot-pool ladder: ask for it,
                // plan when it arrives (`Event::PutLiquidity`).
                let symbol = &self.models[mi].symbol;
                if !self.cfg.exits.spot_pools.contains_key(symbol) {
                    continue; // no allowlisted pool: holding (logged by the adapter)
                }
                self.markets[mi].pending_put_exercise.insert(h.bucket_id, (h.clone(), spot, now));
                out.push(Command::QueryPutLiquidity { bucket: h.bucket_id, market: mi });
                continue;
            }

            // Wallet leg (float coins: auction remnants / staged exits).
            if h.amount_wallet > 0 {
                let leg = match action {
                    ExitAction::ExerciseCash => {
                        wallet_cash = wallet_cash.saturating_sub(cost_wallet);
                        CallLeg::WalletCash { amount: h.amount_wallet, strike_cost: cost_wallet }
                    }
                    ExitAction::FlashExercise => CallLeg::WalletFlash { amount: h.amount_wallet },
                    ExitAction::Hold | ExitAction::ExercisePut => unreachable!(),
                };
                out.push(Command::ExecuteCallPtb { bucket: h.bucket_id, leg });
            }

            // Vault leg (coin-custody positions), minus whatever the
            // listings engine has committed to resting exchange asks.
            let listed = self.book.listed_units(&h.bucket_id);
            let vault_units = h
                .amount_vault
                .saturating_add(h.amount_coin_positions())
                .saturating_sub(listed);
            if vault_units == 0 {
                continue;
            }
            if !self.cfg.curator_session {
                continue; // curator refs unresolved; holding
            }
            // SO-418 risk gate: `exercise_call_coin` spends vault FREE
            // settlement to pay the strike — in risk-off nothing leaves
            // free balances except withdrawal fulfillment, so the session
            // aborts on-chain. Hold instead of burning gas.
            if self.risk_off {
                continue;
            }
            out.push(Command::ExecuteCallPtb { bucket: h.bucket_id, leg: CallLeg::VaultCoins });
        }
    }

    /// Plan the put waterfall for one holding now that its pool ladder
    /// is known (mirrors `exits::put::run`): wallet leg then vault leg,
    /// each laddered inside expiry, the first unexercisable slice ending
    /// its leg.
    fn on_put_liquidity(
        &mut self,
        out: &mut Vec<Command>,
        bucket: ObjectId,
        wallet_underlying: u64,
        vault_underlying: u64,
        pool: PoolLiquidity,
        now_ms: u64,
    ) {
        let Some(mi) = self
            .markets
            .iter()
            .position(|m| m.pending_put_exercise.contains_key(&bucket))
        else {
            return;
        };
        let Some((h, _spot, _decided_at)) = self.markets[mi].pending_put_exercise.remove(&bucket) else {
            return;
        };
        let cfg = &self.cfg.exits;
        let pcfg = &cfg.put;
        let remaining_ms = h.expiry_ms.saturating_sub(now_ms);
        let ladder = |units: u64| {
            put::ladder(
                units,
                cfg.max_slice,
                remaining_ms,
                pcfg.ladder_tx_secs * 1000,
                pcfg.expiry_margin_secs * 1000,
            )
        };

        // Wallet leg (float coins).
        if h.amount_wallet > 0 {
            let liq = PutLiquidity { own_underlying: wallet_underlying, pool: pool.clone() };
            for slice in ladder(h.amount_wallet) {
                match put::plan_slice(pcfg, slice, h.strike, h.strike_scale, self.cfg.settlement_decimals, &liq) {
                    Ok(plan) => out.push(Command::ExecutePutPtb { bucket, leg: PutLeg::Wallet, plan }),
                    Err(_) => break,
                }
            }
        }

        // Vault leg (coin-custody positions), minus resting exchange asks.
        let listed = self.book.listed_units(&bucket);
        let vault_units = h.amount_coin_positions().saturating_sub(listed);
        if vault_units == 0 || !self.cfg.curator_session || !self.cfg.deepbook_adapter || self.risk_off {
            return;
        }
        let liq = PutLiquidity { own_underlying: vault_underlying, pool };
        let mut budget = vault_units;
        for cp in &h.coin_positions {
            if budget == 0 {
                break;
            }
            let units = cp.amount.min(budget);
            for slice in ladder(units) {
                // Only the vault-underlying route exists for custody coins.
                match put::plan_slice(pcfg, slice, h.strike, h.strike_scale, self.cfg.settlement_decimals, &liq) {
                    Ok(plan) if plan.path == PutPath::VaultUnderlying => {
                        budget -= plan.amount;
                        out.push(Command::ExecutePutPtb {
                            bucket,
                            leg: PutLeg::VaultCoin { position_id: cp.position_id },
                            plan,
                        });
                    }
                    _ => break,
                }
            }
        }
    }

    /// A put slice landed: close the exercised units' share of the LONG
    /// perp hedge — a SELL of `|Δ| × units` on the market's primary venue
    /// (doc 08 §4.4: the unwind is not atomic with the chain).
    fn on_exercise_result(
        &mut self,
        out: &mut Vec<Command>,
        bucket: ObjectId,
        _leg: PutLeg,
        units: u64,
        ok: bool,
        now_ms: u64,
    ) {
        if !ok || units == 0 {
            return;
        }
        let Some(h) = self.book.holdings.iter().find(|h| h.bucket_id == bucket && h.is_put).cloned() else {
            return;
        };
        let Some(mi) = self.models.iter().position(|m| m.coin_type == h.asset_coin_type) else {
            return;
        };
        let Some(spot) = self.markets[mi].spot.map(|s| s.spot) else {
            return;
        };
        let t_years = h.expiry_ms.saturating_sub(now_ms) as f64 / 1000.0 / 86_400.0 / 365.0;
        let strike = h.strike_scaled();
        let (sigma, _) = self.models[mi].sigma(spot, strike, t_years);
        let delta = self.models[mi].greeks_per_unit(true, spot, strike, t_years, sigma).delta;
        // Held put: book delta = Δ × units (Δ < 0), hedge = −that (long);
        // closing sells it back, i.e. size = Δ × units.
        let size = delta * units as f64;
        if size == 0.0 {
            return;
        }
        let m = &mut self.markets[mi];
        m.next_order_id += 1;
        let order = HedgeOrder { id: m.next_order_id, size_units: size, spot };
        m.open_orders.submit(&order, now_ms);
        out.push(Command::SubmitHedgeOrder { market: mi, order });
    }

    // ── outboxes → commands ────────────────────────────────────────────

    fn drain_reservations(&mut self, out: &mut Vec<Command>) {
        for r in self.book.drain_reservation_transitions() {
            out.push(if r.state.is_live() {
                Command::ReservePremium(r)
            } else {
                Command::ReleasePremium(r)
            });
        }
    }

    fn drain_pnl(&mut self, out: &mut Vec<Command>) {
        for rec in self.book.drain_pnl_records() {
            out.push(Command::RecordPnl(rec));
        }
    }
}

/// Emit the policy transition, if any.
fn transition(out: &mut Vec<Command>, state: PolicyState, was: bool, now: bool) {
    if now && !was {
        out.push(Command::ActivatePolicy(state));
    } else if !now && was {
        out.push(Command::ClearPolicy(state));
    }
}

#[cfg(test)]
mod tests;
