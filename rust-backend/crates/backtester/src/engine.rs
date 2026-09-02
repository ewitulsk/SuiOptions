//! The event loop (doc 08 §6): a deterministic queue merges external
//! rows (bars, funding, vol index — one Arrow batch per source), the
//! observations they become after feed latency, the timers (mark / vol
//! sample, RFQ flow + quote lifecycle, hedge sample, margin check, NAV
//! sample), the commands the strategy submits, and the acknowledgements
//! / fills / liquidations / margin transfers that come back after venue
//! latency. Timers keep firing through capture holes (§6.4): cached
//! prices age, staleness gates fire, funding and expiry continue, and
//! anything that needed the missing truth is reported as bounded or
//! invalidated.
//!
//! Flow is pluggable (`FlowSource`: the constant injector or the PR N
//! generator), quotes live through the acceptance hazard (reserve →
//! accept/expire/revert → fill), resale is a labeled upside scenario, and
//! the capacity stats the solver gates on are sampled at the ledger stage
//! of every minute.
//!
//! Every instant is a `clock` newtype: `EventTime` for rows,
//! `ActionableTime` for observations, `CommandTime` for the strategy's
//! actions, `AcknowledgementTime`/`FillTime` for venue outcomes.
//!
//! Exercise (doc 08 §7.5/§7.6): the live exit ladder runs on the exits
//! cadence (daily check + near-expiry sweep); slices are routed exactly as
//! `exits.rs`/`exits/put.rs` route them, land after Sui inclusion +
//! indexer detection, and the non-atomic perp close follows. Unexercised
//! options expire worthless, as on chain.
//!
//! Prices (doc 08 §7.4): options are priced and marked at the DECISION
//! price (oracle proxy); the perp executes, marks, funds and margins at
//! the VENUE mark (bar path × basis).

use std::collections::{BTreeMap, VecDeque};

use anyhow::Result;
use pricing::american::{forgone_carry, remaining_time_value_call, AmericanInputs};
use pricing::desk::{expected_hedge_cost, v1_bid, BidContext, HedgeCostParams, V1BidParams};
use desk_core::book::{Reservation, ReservationState};
use serde::Serialize;

use crate::acceptance::{displayed_apy, AcceptanceModel, LiveQuote, Outcome};
use crate::clock::{ActionableTime, CommandTime, EventQueue, EventTime, FillTime, Key, Stage};
use crate::data::{Bar, FundingRow};
use crate::estimator::{SigmaReadout, WindowsEstimator};
use crate::exercise::{self, CallPath, ExerciseStats, PlanReject, PtbHazard, PutLiquidity};
use crate::flow_gen::{ConstantSource, FlowCtx, FlowGen, FlowSource, RfqEvent};
use crate::gaps::{Coverage, GapTracker};
use crate::latency::{LatencyConfig, LatencyModel, LatencyStage};
use crate::ledger::{self, option_id, Counters, ExercisePath, ExercisePlan, Ledger, LedgerEvent, OptionKind, OptionSpec, PerpPosition, Study};
use crate::margin::{entry_margin, topup_amount, IsolatedPosition};
use crate::merge::{EventSource, External, Merge, SliceSource};
use crate::model::{fair_per_unit, greeks_per_unit};
use crate::oracle::{Observation, OracleProxy};
use crate::rng::Pcg32;
use crate::scenario::Scenario;
use crate::stats::{RunStats, TrailingMin};
use crate::venue::{plan_hedge_order, HedgeCommand, HedgeEvent, HedgeOrder, MarketState, OpenOrders, OrderKind, SimPerpVenue, SimVenue, Timed, VenueEvent};
use crate::{MS_PER_DAY, MS_PER_YEAR_F};

/// One settled option, for the vol-P&L study (doc 09 §2.4).
#[derive(Clone, Debug, Serialize)]
pub struct SettledOption {
    pub id: u64,
    pub is_put: bool,
    pub strike: f64,
    pub opened_ms: i64,
    pub expiry_ms: i64,
    pub qty: f64,
    pub spot_open: f64,
    pub spot_close: f64,
    pub premium_paid: f64,
    pub payoff: f64,
    pub sigma_paid: f64,
    pub sigma_surface: f64,
    /// Realized vol over the option's life at the estimator's interval.
    pub sigma_realized: f64,
    /// ½·Γ·S²·(σ_r² − σ_paid²)·τ at entry greeks, per doc 09 §2.1.
    pub vol_pnl_proxy: f64,
    /// Doc 07 §5 "hedge P&L" analogue for this option's life:
    /// payoff − premium (option leg only).
    pub option_leg_pnl: f64,
}

/// A daily NAV sample.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct NavPoint {
    pub ts_ms: i64,
    pub spot: f64,
    pub nav: f64,
    pub cash: f64,
    pub option_marks: f64,
    pub perp_position: f64,
    pub net_delta_units: f64,
    pub premium_deployed_pct: f64,
    pub sigma_surface: Option<f64>,
    pub stale: bool,
    /// Venue margin ratio of the perp position (None when flat).
    pub margin_ratio: Option<f64>,
}

/// Venue/margin labels every summary carries (doc 08 §7.3).
#[derive(Clone, Debug, Serialize)]
pub struct VenueLabels {
    pub proxy_venue: bool,
    pub imr: f64,
    pub mmr: f64,
    pub leverage: f64,
    /// Partial liquidation is an assumption (not in the Bluefin docs).
    pub partial_liquidation_assumed: bool,
    pub basis_configured: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunOutput {
    pub nav_path: Vec<NavPoint>,
    pub settled: Vec<SettledOption>,
    /// The exact ledger (doc 08 §5.3): reconciled after every event.
    pub ledger: Ledger,
    /// Strategy declines that move no balance.
    pub counters: Counters,
    pub minutes_total: u64,
    pub minutes_with_bar: u64,
    pub minutes_stale: u64,
    pub funding_settlements: u64,
    pub turns: u64,
    pub max_drawdown: f64,
    pub nav_end: f64,
    pub spot_start: f64,
    pub spot_end: f64,
    pub stats: RunStats,
    pub flow_source: &'static str,
    pub acceptance: &'static str,
    /// Doc 08 §6.4: coverage, gaps and invalidated spans per feed.
    pub coverage: Coverage,
    /// Rows consumed per source, roster order (§6.5 reconciliation).
    pub source_rows: Vec<(String, u64)>,
    /// Timer firings by kind.
    pub timer_counts: BTreeMap<String, u64>,
    /// Events processed by stage.
    pub stage_counts: BTreeMap<String, u64>,
    /// FNV over the `(ms, stage, sub)` trace: the event-ordering fingerprint.
    pub trace_hash: String,
    pub latency_draws: u64,
    /// Outcomes still in flight when the run window closed.
    pub pending_outcomes: u64,
    pub latency: LatencyConfig,
    pub execution_assumption: String,
    pub venue_labels: VenueLabels,
    /// Lowest venue margin ratio seen while a position was open.
    pub min_margin_ratio: Option<f64>,
    /// Closest approach to liquidation: min `(MR − MMR) / MMR`.
    pub closest_margin_headroom: Option<f64>,
    pub first_liquidation_ms: Option<i64>,
    /// Doc 08 §7.5/§7.6 exercise counters and labels.
    pub exercise: ExerciseStats,
}

/// The external feeds of one run.
pub struct Sources {
    pub bars: Box<dyn EventSource>,
    pub funding: Box<dyn EventSource>,
    pub vol_index: Box<dyn EventSource>,
}

impl Sources {
    pub fn from_slices(bars: &[Bar], funding: &[FundingRow], vol_index: &[(i64, f64)]) -> Self {
        Self {
            bars: Box::new(SliceSource::bars(bars)),
            funding: Box::new(SliceSource::funding(funding)),
            vol_index: Box::new(SliceSource::vol_index(vol_index)),
        }
    }
}

/// Timer kinds (doc 08 §6.2: merged into the event stream, never
/// scheduled from wall clock). `sub` order inside one instant: mark →
/// flow → hedge → margin, the order the desk's tasks see a minute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Timer {
    /// Staleness, estimator readout, option marks, expiry settlement,
    /// resale, the displayed APY menu.
    Mark,
    /// RFQ arrivals from the flow source and the quote lifecycle.
    Flow,
    /// Exits cadence: the exercise ladder (doc 08 §7.5/§7.6).
    Exit,
    /// Hedge sample: bands decide, the clock only samples.
    Hedge,
    /// Margin check: top-up policy (doc 08 §7.3).
    Margin,
}

impl Timer {
    fn sub(self) -> u8 {
        match self {
            Timer::Mark => 0,
            Timer::Flow => 1,
            Timer::Exit => 2,
            Timer::Hedge => 3,
            Timer::Margin => 4,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Timer::Mark => "mark",
            Timer::Flow => "flow",
            Timer::Exit => "exit",
            Timer::Hedge => "hedge",
            Timer::Margin => "margin",
        }
    }
}

#[derive(Clone, Debug)]
enum Command {
    Hedge(HedgeCommand, OrderKind),
    /// Move `amount` of vault cash to the venue as margin.
    TopUp(f64),
    /// Submit one exercise PTB slice.
    Exercise(ExerciseCmd),
}

#[derive(Clone, Debug)]
enum Queued {
    Observation(Observation),
    Timer(Timer),
    Command(Command),
    Outcome(Timed),
    /// An exercise PTB detected by the indexer (success or atomic failure).
    Exercised(ExerciseCmd),
    /// Capacity stats, NAV sample and drawdown, after every outcome of
    /// the instant.
    NavSample,
}

fn v1_params(s: &Scenario) -> V1BidParams {
    V1BidParams {
        base_spread_volpts: s.bid.base_spread_volpts,
        size_penalty_volpts_per_pct_nav: s.bid.size_penalty_volpts_per_pct_nav,
        size_penalty_quadratic_from_pct: s.bid.size_penalty_quadratic_from_pct,
        inventory_penalty_max_volpts: s.bid.inventory_penalty_max_volpts,
        inventory_penalty_start_util: s.bid.inventory_penalty_start_util,
        max_single_fill_pct_nav: s.bid.max_single_fill_pct_nav,
        funding_income_credit: s.bid.funding_income_credit,
        // Composition limits (SO-445) are not modeled in v0.
        composition_penalty_volpts: 0.0,
    }
}

fn hedge_cost_params(s: &Scenario) -> HedgeCostParams {
    HedgeCostParams {
        slippage_bps: s.hedge.slippage_bps,
        taker_fee_bps: s.hedge.taker_fee_bps,
        fixed_fee_per_fill: s.hedge.fixed_fee_per_fill,
        rebalance_turnover_per_year: s.hedge.rebalance_turnover_per_year,
        margin_financing_rate_annual: s.hedge.margin_financing_rate_annual,
        initial_margin_fraction: s.hedge.initial_margin_fraction,
    }
}

/// Annualized funding from the latest settled row (rate per interval).
fn annualize(row: &FundingRow) -> f64 {
    if row.interval_hours <= 0.0 {
        0.0
    } else {
        row.rate * (8760.0 / row.interval_hours)
    }
}

/// CRR steps for the call time-value rule (mm-bot `desk::model::CRR_STEPS`).
const CRR_STEPS: usize = 128;

/// One exercise slice in flight: planned at command time on the route
/// the desk saw, settled at detection.
#[derive(Clone, Debug)]
struct ExerciseCmd {
    /// The ledger's pending-exercise op.
    op: u64,
    position_id: u64,
    units: f64,
    is_put: bool,
    path: &'static str,
    /// Net settlement after every modeled cost.
    net: f64,
    /// Route notional (acquisition or sale), for the turnover line.
    turnover: f64,
    delta_per_unit: f64,
    spot: f64,
    failed: bool,
}

/// One priced spec: fair, greeks, and the V1 bid (None = priced zero).
struct Priced {
    sigma: f64,
    fair: f64,
    delta: f64,
    gamma: f64,
    vega: f64,
    bid: Option<f64>,
    sigma_paid: f64,
}

/// Build the scenario's flow source (`flow.source`).
pub fn flow_source(s: &Scenario, start_ms: i64) -> Result<Box<dyn FlowSource>> {
    Ok(match s.flow.source.as_str() {
        "constant" => Box::new(ConstantSource::new(&s.flow, start_ms)?),
        "generated" => Box::new(FlowGen::new(&s.flow_gen, &s.flow, s.seed)?),
        other => anyhow::bail!("unknown flow.source {other}"),
    })
}

/// What the Mark timer computed this minute, shared with the Flow, Hedge
/// and Margin timers and the NAV sample of the same instant (the
/// observable cache, §6.2 step 3).
#[derive(Clone, Copy)]
struct MinuteCtx {
    ms: i64,
    spot: f64,
    stale: bool,
    readout: SigmaReadout,
    revalue_now: bool,
    book_changed: bool,
    filled: bool,
    resold: bool,
}

struct Engine<'a> {
    s: &'a Scenario,
    source: &'a mut dyn FlowSource,
    start_ms: i64,
    end_ms: i64,
    interval_ms: i64,
    revalue_ms: i64,
    tenor_years: f64,
    fee_wedge: f64,
    est: WindowsEstimator,
    oracle: OracleProxy,
    ledger: Ledger,
    /// Per-option study data keyed by position id (the ledger holds the
    /// quantities and marks).
    studies: BTreeMap<u64, Study>,
    counters: Counters,
    next_id: u64,
    next_op: u64,
    /// The ledger's perp market key.
    perp_key: String,
    /// Sent top-ups awaiting their venue outcome (FIFO: the transfer
    /// latency is constant).
    topup_ops: VecDeque<u64>,
    v1: V1BidParams,
    hedge_cost: HedgeCostParams,
    acceptance: AcceptanceModel,
    stats: RunStats,
    live: Vec<LiveQuote>,
    topup: TrailingMin,
    queue: EventQueue<Queued>,
    lat: LatencyModel,
    gaps: GapTracker,
    venue: SimPerpVenue,
    open: OpenOrders,
    next_order_id: u64,
    /// After a passive timeout the next order goes out as a taker.
    escalate: bool,
    market: MarketState,
    /// Last bar close (market truth) for marks while stale.
    last_spot: f64,
    spot_start: Option<f64>,
    price_samples: Vec<(i64, f64)>,
    last_sample_ms: i64,
    funding_annual: f64,
    funding_settlements: u64,
    index_vol: Option<f64>,
    last_revalue_ms: i64,
    apy_call: Option<f64>,
    apy_put: Option<f64>,
    cur: Option<MinuteCtx>,
    nav_path: Vec<NavPoint>,
    settled: Vec<SettledOption>,
    minutes_with_bar: u64,
    minutes_stale: u64,
    peak: f64,
    max_dd: f64,
    last_nav_day: i64,
    net_delta_units: f64,
    /// Rolling 24 h of accepted top-ups (doc 08 §0.4 cap).
    topups: VecDeque<(i64, f64)>,
    min_margin_ratio: Option<f64>,
    closest_headroom: Option<f64>,
    first_liquidation_ms: Option<i64>,
    hazard: PtbHazard,
    xstats: ExerciseStats,
    timer_counts: BTreeMap<String, u64>,
    stage_counts: BTreeMap<String, u64>,
    trace_hash: u64,
}

fn bump(map: &mut BTreeMap<String, u64>, key: &str) {
    *map.entry(key.to_string()).or_insert(0) += 1;
}

/// Engine instants are `i64` ms; the ledger's are `u64`.
fn ms_u64(ms: i64) -> u64 {
    ms.max(0) as u64
}

/// The ledger's reservation key of one live quote.
fn quote_key(q: &LiveQuote) -> String {
    format!("{}:{}:{}:{}", q.rfq.key.minute, q.rfq.key.is_put as u8, q.rfq.key.k, q.sent_ms)
}

/// The route label the exercise model chose → the ledger's PTB path.
fn ledger_path(label: &str) -> ExercisePath {
    match label {
        "call_cash" => ExercisePath::CallCash,
        "call_quote_flash" => ExercisePath::CallFlash,
        "vault_underlying" => ExercisePath::PutVaultUnderlying,
        "base_flash" => ExercisePath::PutBaseFlash,
        _ => ExercisePath::PutQuoteFlash,
    }
}

impl<'a> Engine<'a> {
    fn trace(&mut self, ms: i64, stage: Stage, sub: u8) {
        let mut h = self.trace_hash;
        for b in ms.to_le_bytes().into_iter().chain([stage as u8, sub]) {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        self.trace_hash = h;
        bump(&mut self.stage_counts, &format!("{stage:?}").to_lowercase());
    }

    fn schedule(&mut self, ms: i64, stage: Stage, sub: u8, ev: Queued) -> Key {
        self.queue.schedule(ms, stage, sub, ev)
    }

    fn schedule_outcomes(&mut self, timed: Vec<Timed>) {
        for t in timed {
            self.schedule(t.at_ms, Stage::Outcome, 0, Queued::Outcome(t));
        }
    }

    fn in_window(&self, ms: i64) -> bool {
        ms >= self.start_ms && ms < self.end_ms
    }

    /// The venue mark of the latest bar.
    fn mark(&self) -> f64 {
        self.venue.mark(&self.market)
    }

    fn perp(&self) -> PerpPosition {
        self.ledger.perps.get(&self.perp_key).copied().unwrap_or_default()
    }

    /// Apply one ledger event; a refusal is an engine bug (the one
    /// expected refusal, a reservation over capital, is handled in
    /// `on_flow`).
    fn apply(&mut self, ev: LedgerEvent) {
        if let Err(e) = self.ledger.apply(&ev) {
            panic!("ledger refused {ev:?}: {e}");
        }
    }

    fn next_op(&mut self) -> u64 {
        self.next_op += 1;
        self.next_op
    }

    fn account(&self) -> IsolatedPosition {
        let p = self.perp();
        IsolatedPosition { size: p.units, entry: p.entry, margin: p.collateral }
    }

    #[allow(clippy::too_many_arguments)]
    fn price(&self, readout: &SigmaReadout, now: i64, spot: f64, nav: f64, is_put: bool, strike: f64, expiry_ms: i64, qty: f64) -> Priced {
        let s = self.s;
        let carry = s.carry_yield;
        let t = (expiry_ms - now) as f64 / MS_PER_YEAR_F;
        let sigma = readout.surface.vol(spot, strike, t);
        let fair_pu = fair_per_unit(is_put, spot, strike, t, sigma, carry);
        let g = greeks_per_unit(is_put, spot, strike, t, sigma, carry);
        let fair = fair_pu * qty;
        let vega_book: f64 = self
            .studies
            .values()
            .map(|st| st.vega_open * self.ledger.options.get(&option_id(st.id)).map_or(0.0, |p| p.qty) / 100.0)
            .sum();
        let vega_util = if s.limits.vega_cap_nav_per_volpt > 0.0 { vega_book / (s.limits.vega_cap_nav_per_volpt * nav) } else { 0.0 };
        let ctx = BidContext {
            nav,
            premium_notional: fair,
            vega_utilization: vega_util,
            hedge_cost: expected_hedge_cost(
                self.perp().units,
                g.delta * qty,
                spot,
                self.funding_annual,
                s.bid.expected_holding_years,
                s.bid.funding_income_credit,
                &self.hedge_cost,
            ),
            composition_utilization: 0.0,
        };
        let fair_at = |sig: f64| fair_per_unit(is_put, spot, strike, t, sig, carry) * qty;
        let bid = v1_bid(fair_at, sigma, &ctx, &self.v1);
        // Recover the struck sigma for the study: the discount total.
        let discount = pricing::desk::v1_vol_discount(&ctx, &self.v1).map(|d| d.total).unwrap_or(0.0);
        Priced { sigma, fair, delta: g.delta, gamma: g.gamma, vega: g.vega, bid, sigma_paid: (sigma - discount).max(0.0) }
    }

    // ── external rows ──────────────────────────────────────────────────

    fn on_external(&mut self, row: External) {
        let ts = row.ts_ms();
        if self.in_window(ts) {
            self.gaps.observe(row.feed(), ts);
        }
        match row {
            External::Bar(bar) => self.on_bar(EventTime(bar.ts_ms), bar),
            External::Funding(f) => {
                if self.in_window(ts) {
                    self.on_funding(EventTime(ts), f);
                }
            }
            External::VolIndex { pct, .. } => {
                self.index_vol = Some(pct / 100.0);
                self.est.set_index_vol(self.index_vol);
            }
        }
    }

    fn on_bar(&mut self, at: EventTime, bar: Bar) {
        if self.in_window(at.ms()) {
            self.minutes_with_bar += 1;
        }
        self.last_spot = bar.close;
        self.spot_start.get_or_insert(bar.close);
        self.market = MarketState::from_bar(&bar);
        let mark = self.mark();
        self.apply(LedgerEvent::MarkPerp { market: self.perp_key.clone(), mark, at_ms: ms_u64(at.ms()) });
        let extra = self.lat.draw(LatencyStage::Observation);
        if let Some(obs) = self.oracle.observe(at.ms(), bar.close, extra) {
            let actionable = ActionableTime(obs.actionable_ms);
            self.schedule(actionable.ms(), Stage::Observable, 0, Queued::Observation(obs));
        }
        // Venue truth: resting orders, the mark, the risk engine.
        let account = self.account();
        let market = self.market;
        let timed = self.venue.on_bar(&market, &account, &mut self.lat);
        self.schedule_outcomes(timed);
        if let Some(mr) = account.margin_ratio(self.mark()) {
            self.min_margin_ratio = Some(self.min_margin_ratio.map_or(mr, |m| m.min(mr)));
            let h = (mr - self.s.margin.mmr) / self.s.margin.mmr;
            self.closest_headroom = Some(self.closest_headroom.map_or(h, |c| c.min(h)));
        }
    }

    /// Funding settles at the venue against the signed position at the
    /// venue mark (doc 08 §1 item 4, §7.4).
    fn on_funding(&mut self, at_: EventTime, row: FundingRow) {
        self.funding_annual = annualize(&row);
        let paid = row.rate * self.perp().units * self.mark();
        self.apply(LedgerEvent::Funding { market: self.perp_key.clone(), paid, at_ms: ms_u64(at_.ms()) });
        self.funding_settlements += 1;
    }

    // ── observable ─────────────────────────────────────────────────────

    /// The observation is now in the strategy's cache: the estimator and
    /// the study samples see it (never the raw bar).
    fn on_observation(&mut self, obs: Observation) {
        self.est.push(obs.event_ms, obs.price);
        if obs.event_ms.saturating_sub(self.last_sample_ms) >= self.interval_ms {
            self.last_sample_ms = obs.event_ms;
            self.price_samples.push((obs.event_ms, obs.price));
        }
    }

    // ── timers ─────────────────────────────────────────────────────────

    fn on_timer(&mut self, now: i64, t: Timer) {
        bump(&mut self.timer_counts, t.name());
        match t {
            Timer::Mark => self.on_mark(now),
            Timer::Flow => self.on_flow(now),
            Timer::Exit => self.on_exit(now),
            Timer::Hedge => self.on_hedge(now),
            Timer::Margin => self.on_margin(now),
        }
    }

    fn on_mark(&mut self, now: i64) {
        let s = self.s;
        self.gaps.tick(now);
        let decision = self.oracle.decision(now);
        let stale = decision.is_none();
        if stale {
            self.minutes_stale += 1;
        }
        let spot = decision.map(|d| d.price).unwrap_or(self.last_spot);
        let readout = self.est.surface(now);
        let revalue_now = now.saturating_sub(self.last_revalue_ms) >= self.revalue_ms;
        if revalue_now {
            self.last_revalue_ms = now;
        }

        // Settle expiries every minute: whatever is still held expires
        // worthless (a slice included before expiry but not yet detected
        // keeps a worthless shell in the ledger until it lands); then
        // mark every open option at the surface sigma (at the revalue
        // cadence).
        let carry = s.carry_yield;
        let expired: Vec<u64> = self.studies.values().filter(|st| now >= st.expiry_ms).map(|st| st.id).collect();
        let book_changed = !expired.is_empty();
        if book_changed && self.gaps.in_gap("spot", now) {
            // The settlement price is a cached value inside a capture
            // hole: applied at the last known price, and the span is
            // reported as bounded or invalidated per the gap policy.
            self.gaps.needed_truth("spot", now, "expiry settlement");
        }
        for id in expired {
            let (held, pending) =
                self.ledger.options.get(&option_id(id)).map_or((0.0, 0.0), |p| (p.free_units(), p.pending_units));
            let st = self.studies[&id].clone();
            self.expire_worthless(held, spot, &st);
            if pending <= 1e-12 {
                self.studies.remove(&id);
                self.close_position(now, spot, st);
            }
        }
        self.apply(LedgerEvent::ExpireOptions { at_ms: ms_u64(now) });
        if revalue_now {
            let marks: Vec<(String, f64)> = self
                .ledger
                .options
                .iter()
                .filter(|(_, p)| p.spec.expiry_ms > ms_u64(now))
                .map(|(k, p)| {
                    let t = (p.spec.expiry_ms as i64 - now) as f64 / MS_PER_YEAR_F;
                    let sigma = readout.surface.vol(spot, p.spec.strike, t);
                    (k.clone(), fair_per_unit(p.spec.kind.is_put(), spot, p.spec.strike, t, sigma, carry))
                })
                .collect();
            self.apply(LedgerEvent::MarkOptions { marks, at_ms: ms_u64(now) });
        }

        // Resale: a labeled upside scenario, off by default (doc 08 §8.5).
        let mut resold = false;
        if s.resale.enabled && revalue_now && !stale {
            let dt_days = self.revalue_ms as f64 / MS_PER_DAY as f64;
            let min_hold = (s.resale.min_holding_days * MS_PER_DAY as f64) as i64;
            let ids: Vec<u64> = self.studies.keys().copied().collect();
            for id in ids {
                let st = &self.studies[&id];
                if now - st.opened_ms < min_hold + s.resale.latency_ms {
                    continue;
                }
                let key = option_id(id);
                let Some(pos) = self.ledger.options.get(&key) else { continue };
                if pos.spec.expiry_ms <= ms_u64(now) || pos.pending_units > 1e-12 || pos.qty <= 1e-12 {
                    continue;
                }
                let demand = if st.is_put { s.resale.put_demand_per_day } else { s.resale.call_demand_per_day };
                let p_sell = 1.0 - (-demand * s.resale.fill_prob * dt_days).exp();
                let u = Pcg32::keyed(s.seed, &[id, (now / self.revalue_ms) as u64, 0x72]).uniform();
                if u < p_sell {
                    let (qty, proceeds, premium_paid) = (pos.qty, pos.value() * (1.0 - s.resale.price_discount), st.premium_paid);
                    let op = self.next_op();
                    self.apply(LedgerEvent::ResaleSubmitted { op, option: key, qty, expected_proceeds: proceeds, at_ms: ms_u64(now) });
                    self.apply(LedgerEvent::ResaleSettled { op, proceeds: Some(proceeds), at_ms: ms_u64(now) });
                    self.studies.remove(&id);
                    self.stats.resales += 1;
                    self.stats.resale_pnl += proceeds - premium_paid;
                    resold = true;
                }
            }
        }

        // Displayed APY menu for the arrival model (market mode).
        if revalue_now {
            if stale {
                self.apy_call = None;
                self.apy_put = None;
            } else {
                let nav_now = self.ledger.nav();
                let ctx0 = self.flow_ctx(now, spot, &readout, nav_now, stale);
                for (is_put, strike, expiry_ms) in self.source.indicative_specs(&ctx0) {
                    let qty = s.flow_gen.apy_reference_notional / spot;
                    let pr = self.price(&readout, now, spot, nav_now, is_put, strike, expiry_ms, qty);
                    let apy = pr.bid.map(|b| displayed_apy(is_put, b * self.fee_wedge, qty, spot, strike, (expiry_ms - now) as f64 / MS_PER_YEAR_F));
                    if is_put { self.apy_put = apy } else { self.apy_call = apy }
                }
            }
        }

        self.cur = Some(MinuteCtx { ms: now, spot, stale, readout, revalue_now, book_changed, filled: false, resold });
        self.schedule(now, Stage::Ledger, 0, Queued::NavSample);
        let next = now + 60_000;
        if next < self.end_ms {
            self.schedule(next, Stage::Timer, Timer::Mark.sub(), Queued::Timer(Timer::Mark));
            self.schedule(next, Stage::Timer, Timer::Flow.sub(), Queued::Timer(Timer::Flow));
            self.schedule(next, Stage::Timer, Timer::Hedge.sub(), Queued::Timer(Timer::Hedge));
        }
    }

    fn flow_ctx(&self, now: i64, spot: f64, readout: &SigmaReadout, nav: f64, stale: bool) -> FlowCtx {
        FlowCtx { now_ms: now, spot, sigma_atm: readout.surface.atm(self.tenor_years), nav, stale, apy_call: self.apy_call, apy_put: self.apy_put }
    }

    /// RFQs from the flow source (the constant injector retries a stale
    /// turn every minute — time is not skipped), quoted or declined with
    /// premium reserved, then the quote lifecycle: the acceptance hazard
    /// over the remaining TTL against the current option value; fill,
    /// expire, or revert.
    fn on_flow(&mut self, now: i64) {
        let s = self.s;
        let Some(ctx) = self.cur.filter(|c| c.ms == now) else { return };
        let (spot, stale, readout) = (ctx.spot, ctx.stale, ctx.readout);
        let carry = s.carry_yield;
        let nav_now = self.ledger.nav();
        let fctx = self.flow_ctx(now, spot, &readout, nav_now, stale);
        let reserved_total: f64 = self.live.iter().map(|q| q.bid).sum();
        for rfq in self.source.rfqs(&fctx) {
            self.stats.rfqs_offered += 1;
            if rfq.is_put { self.stats.rfqs_put += 1 } else { self.stats.rfqs_call += 1 }
            self.stats.volumes.offered_earn_notional += rfq.offered_notional;
            if stale {
                self.counters.declines_stale += 1;
                self.stats.declined.count_stale += 1;
                self.stats.declined.stale += rfq.offered_notional;
                continue;
            }
            let pr = self.price(&readout, now, spot, nav_now, rfq.is_put, rfq.strike, rfq.expiry_ms, rfq.qty);
            // Limits (doc 08 §0.4): total, per type, per expiry — live
            // reservations counted once in each numerator.
            let deployed = self.ledger.option_marks() + reserved_total;
            let by_type = ledger::premium_by_type(&self.ledger, rfq.is_put) + self.live.iter().filter(|q| q.rfq.is_put == rfq.is_put).map(|q| q.bid).sum::<f64>();
            let by_expiry = ledger::premium_by_expiry(&self.ledger).get(&rfq.expiry_ms).copied().unwrap_or(0.0)
                + self.live.iter().filter(|q| q.rfq.expiry_ms == rfq.expiry_ms).map(|q| q.bid).sum::<f64>();
            let type_cap = if rfq.is_put { s.limits.put_premium_max } else { s.limits.call_premium_max };
            let over_total = deployed + pr.fair > s.limits.premium_budget_hard * nav_now;
            let over_type = by_type + pr.fair > type_cap * nav_now;
            let over_expiry = by_expiry + pr.fair > s.limits.per_expiry_max * nav_now;
            if over_total || over_type || over_expiry {
                self.counters.declines_capacity += 1;
                self.stats.declined.count_capacity += 1;
                self.stats.declined.capacity += rfq.offered_notional;
                self.stats.declined.count_total_cap += over_total as u64;
                self.stats.declined.count_expiry_cap += over_expiry as u64;
                if over_type {
                    if rfq.is_put { self.stats.declined.count_put_cap += 1 } else { self.stats.declined.count_call_cap += 1 }
                }
                continue;
            }
            let Some(bid) = pr.bid else {
                self.counters.declines_priced_zero += 1;
                self.stats.declined.count_priced_zero += 1;
                self.stats.declined.priced_zero += rfq.offered_notional;
                continue;
            };
            let q = self.acceptance.open(rfq, now, bid, bid * self.fee_wedge, spot, pr.fair, pr.sigma, pr.sigma_paid, (pr.delta, pr.gamma, pr.vega));
            // The premium is reserved on the ledger for the quote's life;
            // free settlement that cannot back it is a capacity decline.
            let reservation = Reservation {
                key: quote_key(&q),
                nonce: None,
                amount: bid.round() as u64,
                is_put: rfq.is_put,
                expiry_ms: ms_u64(rfq.expiry_ms),
                exercise_cash: 0.0,
                hedge_notional: 0.0,
                quoted_at_ms: ms_u64(q.sent_ms),
                expires_ms: ms_u64(q.valid_until_ms),
                state: ReservationState::Quoted,
                state_at_ms: ms_u64(now),
            };
            if self.ledger.apply(&LedgerEvent::Reserve(reservation)).is_err() {
                self.counters.declines_capacity += 1;
                self.stats.declined.count_capacity += 1;
                self.stats.declined.capacity += rfq.offered_notional;
                continue;
            }
            self.stats.quotes_sent += 1;
            self.stats.volumes.quoted_earn_notional += rfq.offered_notional;
            self.stats.sample_apy(rfq.is_put, q.displayed_apy);
            self.live.push(q);
        }

        let mut filled = false;
        let mut i = 0;
        while i < self.live.len() {
            let current_fair = {
                let q = &self.live[i];
                let t = (q.rfq.expiry_ms - now) as f64 / MS_PER_YEAR_F;
                let sigma = readout.surface.vol(spot, q.rfq.strike, t);
                fair_per_unit(q.rfq.is_put, spot, q.rfq.strike, t, sigma, carry) * q.rfq.qty
            };
            let outcome = self.acceptance.step(&mut self.live[i], now, current_fair);
            let Some(outcome) = outcome else {
                i += 1;
                continue;
            };
            let q = self.live.remove(i);
            let key = quote_key(&q);
            match outcome {
                Outcome::Expired => {
                    self.apply(LedgerEvent::ReservationTransition { key, state: ReservationState::Expired, at_ms: ms_u64(now) });
                    self.stats.quotes_expired += 1;
                }
                Outcome::Reverted => {
                    self.apply(LedgerEvent::ReservationTransition { key, state: ReservationState::Reverted, at_ms: ms_u64(now) });
                    self.stats.quotes_reverted += 1;
                }
                Outcome::Filled(_) => {
                    let rfq: RfqEvent = q.rfq;
                    filled = true;
                    self.stats.quotes_accepted += 1;
                    self.stats.volumes.accepted_earn_notional += rfq.offered_notional;
                    self.stats.volumes.premium_turnover += q.bid;
                    if rfq.is_put { self.stats.fills_put += 1 } else { self.stats.fills_call += 1 }
                    let id = self.next_id;
                    self.next_id += 1;
                    self.apply(LedgerEvent::OptionBought {
                        option: option_id(id),
                        spec: Some(OptionSpec { kind: OptionKind::of(rfq.is_put), strike: rfq.strike, expiry_ms: ms_u64(rfq.expiry_ms) }),
                        qty: rfq.qty,
                        premium: q.bid,
                        mark_per_unit: current_fair / rfq.qty,
                        at_ms: ms_u64(now),
                    });
                    self.apply(LedgerEvent::ReservationTransition { key, state: ReservationState::Filled, at_ms: ms_u64(now) });
                    self.studies.insert(id, Study {
                        id, is_put: rfq.is_put, strike: rfq.strike, expiry_ms: rfq.expiry_ms, premium_paid: q.bid,
                        sigma_paid: q.sigma_paid, sigma_surface: q.sigma_quote, opened_ms: now, spot_open: spot, delta_open: q.delta,
                        gamma_open: q.gamma, vega_open: q.vega, writer_net_premium: q.writer_net,
                        qty_open: rfq.qty, last_check_ms: i64::MIN / 2, exercise_net: 0.0,
                    });
                }
            }
        }
        if let Some(c) = self.cur.as_mut() {
            c.filled = filled;
        }
    }

    /// Hedge sample: bands not clocks; no orders on a stale price. Net
    /// delta is recomputed at the revalue cadence or when the book
    /// changed. Orders working past the timeout are cancelled (and the
    /// next order goes out as a taker when `passive_timeout_to_taker`);
    /// the working remainder counts against the band so a slow fill is
    /// not re-submitted every minute (mm-bot `plan_hedge_order`).
    fn on_hedge(&mut self, now: i64) {
        let s = self.s;
        let Some(ctx) = self.cur.filter(|c| c.ms == now) else { return };
        let spot = ctx.spot;
        let carry = s.carry_yield;
        if ctx.revalue_now || ctx.filled || ctx.book_changed || ctx.resold {
            self.net_delta_units = self
                .ledger
                .options
                .values()
                .map(|p| {
                    let t = (p.spec.expiry_ms as i64 - now) as f64 / MS_PER_YEAR_F;
                    let sigma = ctx.readout.surface.vol(spot, p.spec.strike, t);
                    greeks_per_unit(p.spec.kind.is_put(), spot, p.spec.strike, t, sigma, carry).delta * p.qty
                })
                .sum::<f64>();
        }
        if ctx.stale {
            return;
        }
        for id in self.open.stale(now, s.venue.order_timeout_secs * 1000) {
            let at = CommandTime(now + self.lat.draw(LatencyStage::Strategy));
            self.schedule(at.ms(), Stage::Command, 0, Queued::Command(Command::Hedge(HedgeCommand::Cancel(id), OrderKind::Taker)));
            if s.venue.passive_timeout_to_taker {
                self.escalate = true;
            }
        }
        let nav_now = self.ledger.nav();
        let perp = self.perp();
        let pct = if self.funding_annual < s.hedge.funding_widen_threshold { s.hedge.band_wide_pct_nav } else { s.hedge.band_pct_nav };
        let band_units = (pct / 100.0) * nav_now / spot;
        let working = self.open.working_units();
        if let Some(mut size) = plan_hedge_order(self.net_delta_units, perp.units, working, band_units) {
            // Venue capacity: an assumption until measured. The target is
            // clamped once the band has decided to trade.
            let cap = s.venue.max_hedge_notional;
            let mut target = -self.net_delta_units;
            if cap > 0.0 && target.abs() * spot > cap {
                target = target.signum() * cap / spot;
                self.stats.venue_cap_hits += 1;
                size = target - (perp.units + working);
                if size == 0.0 {
                    return;
                }
            }
            // No risk without margin: an order that grows the exposure must
            // be fundable from free cash at the entry ratio, else it is not
            // sent (counted; the band re-plans next minute).
            let after = perp.units + working + size;
            if after.abs() > (perp.units + working).abs() {
                let need = entry_margin(after.abs() * self.mark(), &s.margin) - perp.collateral;
                if need > self.ledger.settlement {
                    self.counters.hedge_declines_margin += 1;
                    return;
                }
            }
            let order = HedgeOrder { id: self.next_order_id, size_units: size, spot };
            self.next_order_id += 1;
            let kind = if s.venue.is_passive() && !self.escalate { OrderKind::Passive } else { OrderKind::Taker };
            self.escalate = false;
            let at = CommandTime(now + self.lat.draw(LatencyStage::Strategy));
            self.open.submit(&order, at.ms());
            self.schedule(at.ms(), Stage::Command, 0, Queued::Command(Command::Hedge(HedgeCommand::Submit(order), kind)));
        }
    }

    /// Margin check (doc 08 §7.3): below the trigger ratio, move cash to
    /// the venue to restore the target, inside the 24 h cap and the free
    /// cash. The transfer lands after `topup_transfer_ms`.
    fn on_margin(&mut self, now: i64) {
        let s = self.s;
        let next = now + s.margin.check_secs.max(1) * 1000;
        if next < self.end_ms {
            self.schedule(next, Stage::Timer, Timer::Margin.sub(), Queued::Timer(Timer::Margin));
        }
        let want = topup_amount(&self.account(), self.mark(), &s.margin);
        if want <= 0.0 {
            return;
        }
        while self.topups.front().is_some_and(|(t, _)| now - *t >= MS_PER_DAY) {
            self.topups.pop_front();
        }
        let used: f64 = self.topups.iter().map(|(_, a)| a).sum();
        let cap = s.margin.max_topup_24h_pct_nav * self.ledger.nav() - used;
        let amount = want.min(cap).min(self.ledger.settlement);
        if amount <= 0.0 {
            self.counters.topup_declines += 1;
            return;
        }
        self.topups.push_back((now, amount));
        let at = CommandTime(now + self.lat.draw(LatencyStage::Strategy));
        self.schedule(at.ms(), Stage::Command, 0, Queued::Command(Command::TopUp(amount)));
    }

    // ── exercise (doc 08 §7.5/§7.6, PR M) ──────────────────────────────

    /// The exits tick: every held option is checked once per
    /// `check_interval_secs`, and every sweep inside `near_expiry_hours`
    /// (the redundant keeper). A wanted exercise is laddered and each
    /// slice routed exactly as the live policy routes it; the PTB is a
    /// command that lands after Sui inclusion + indexer detection.
    fn on_exit(&mut self, now: i64) {
        let s = self.s;
        let x = &s.exercise;
        let next = now + x.sweep_interval_secs.max(1) * 1000;
        if next < self.end_ms {
            self.schedule(next, Stage::Timer, Timer::Exit.sub(), Queued::Timer(Timer::Exit));
        }
        let Some(ctx) = self.cur.filter(|c| c.ms == now) else { return };
        if ctx.stale {
            return;
        }
        let spot = ctx.spot;
        let route = exercise::route_at(x, spot);
        let policy = exercise::PolicyConfig::from(x);
        // Free settlement for call cash exercises this tick (reservations
        // and in-flight slices already spent).
        let reserved: f64 = self.live.iter().map(|q| q.bid).sum();
        let mut free_cash = (self.ledger.settlement - reserved).max(0.0);
        let mut own_underlying = exercise::units_raw(x.vault_free_underlying_units);
        let ids: Vec<u64> = self.studies.keys().copied().collect();
        for id in ids {
            let p = self.studies[&id].clone();
            let key = option_id(id);
            let held = self.ledger.options.get(&key).map_or(0.0, |o| o.free_units());
            if held <= 0.0 || now >= p.expiry_ms {
                continue;
            }
            let hours = (p.expiry_ms - now) as f64 / 3_600_000.0;
            let near = hours <= x.near_expiry_hours;
            if !near && now - p.last_check_ms < x.check_interval_secs * 1000 {
                continue;
            }
            self.studies.get_mut(&id).expect("present").last_check_ms = now;
            let itm = if p.is_put { spot < p.strike } else { spot > p.strike };
            if !itm {
                continue;
            }
            let t = (p.expiry_ms - now) as f64 / MS_PER_YEAR_F;
            let sigma = ctx.readout.surface.vol(spot, p.strike, t);
            let inputs = AmericanInputs { spot, strike: p.strike, t_years: t, sigma: sigma.max(1e-6), rate: 0.0, carry_yield: s.carry_yield };
            // ITM inside the sweep window is always attempted; the CRR
            // time-value rule only decides the early (daily) case.
            let wanted = if near {
                true
            } else if p.is_put {
                let fair = fair_per_unit(true, spot, p.strike, t, sigma, s.carry_yield);
                let tv = (fair - (p.strike - spot).max(0.0)).max(0.0);
                // Carry on the strike cash at the (zero) settlement rate.
                exercise::put_exercise_wanted(x.near_expiry_hours, x.carry_mult, spot, p.strike, hours, 0.0, tv)
            } else {
                let carry = forgone_carry(&inputs);
                let tv = remaining_time_value_call(&inputs, CRR_STEPS);
                exercise::call_exercise_wanted(x.near_expiry_hours, x.carry_mult, spot, p.strike, hours, carry, tv)
            };
            if !wanted {
                continue;
            }
            let delta = greeks_per_unit(p.is_put, spot, p.strike, t, sigma, s.carry_yield).delta;
            let strike = exercise::strike_raw(p.strike);
            // PR N's per-exercise flash/router ceiling (an assumption the
            // solver gates on): counted when the held notional exceeds it.
            let cap = s.venue.flash_max_notional_per_exercise;
            if cap > 0.0 && held * spot > cap {
                self.stats.flash_cap_hits += 1;
            }
            let slices = exercise::ladder(exercise::units_raw(held), exercise::units_raw(x.max_slice_units), (p.expiry_ms - now) as u64, x.ladder_tx_secs * 1000, x.expiry_margin_secs * 1000);
            if slices.len() > 1 {
                self.stats.exercise_laddered += 1;
            }
            for slice in slices {
                let (path, net_raw, turnover_raw) = if p.is_put {
                    let liq = PutLiquidity { own_underlying, pool: route.pool.clone() };
                    match exercise::plan_slice(&policy, slice, strike, exercise::STRIKE_SCALE, exercise::SETTLEMENT_DECIMALS, &liq) {
                        Ok(plan) => (plan.path.label(), plan.expected_net as i128, plan.max_quote_in),
                        Err(r) => {
                            self.reject(&r);
                            break;
                        }
                    }
                } else {
                    match exercise::plan_call_slice(&policy, slice, strike, exercise::STRIKE_SCALE, exercise::SETTLEMENT_DECIMALS, exercise::settle_raw(free_cash), &route.pool, &route.bids) {
                        Ok(plan) => {
                            if plan.path == CallPath::Cash {
                                free_cash -= exercise::raw_settle(plan.cost as i128);
                            }
                            (plan.path.label(), plan.net, plan.proceeds)
                        }
                        Err(r) => {
                            self.reject(&r);
                            break;
                        }
                    }
                };
                // The vault-underlying route repurchases what it delivers;
                // the own balance is unchanged for the next slice.
                let _ = &mut own_underlying;
                let units = slice as f64 / 10f64.powi(exercise::UNDERLYING_DECIMALS as i32);
                let (net, turnover) = (exercise::raw_settle(net_raw), exercise::raw_settle(turnover_raw as i128));
                let ledger_path = ledger_path(path);
                // The modeled route: net settlement after every cost; a
                // flash path borrows and repays the route notional inside
                // the PTB. The cash call path sells the received spot on
                // the route (`call_cash_path_sells_spot`), so no
                // underlying lands.
                let flash = if matches!(ledger_path, ExercisePath::CallCash | ExercisePath::PutVaultUnderlying) { 0.0 } else { turnover };
                let op = self.next_op();
                self.apply(LedgerEvent::ExerciseSubmitted {
                    op,
                    plan: ExercisePlan {
                        option: key.clone(),
                        path: ledger_path,
                        qty: units,
                        asset: s.asset.clone(),
                        settlement_out: (-net).max(0.0),
                        settlement_in: net.max(0.0),
                        underlying_in: 0.0,
                        underlying_out: 0.0,
                        flash_borrowed: flash,
                        flash_repaid: flash,
                        route_notional: turnover,
                        gas: 0.0,
                    },
                    at_ms: ms_u64(now),
                });
                self.xstats.slices_submitted += 1;
                let cmd = ExerciseCmd {
                    op,
                    position_id: p.id,
                    units,
                    is_put: p.is_put,
                    path,
                    net,
                    turnover,
                    delta_per_unit: delta,
                    spot,
                    failed: false,
                };
                let at = CommandTime(now + self.lat.draw(LatencyStage::Strategy));
                self.schedule(at.ms(), Stage::Command, 0, Queued::Command(Command::Exercise(cmd)));
            }
        }
    }

    fn reject(&mut self, r: &PlanReject) {
        self.xstats.reject(r.label());
        if matches!(r, PlanReject::Capacity) {
            self.stats.flash_cap_hits += 1;
        }
    }

    /// The PTB lands (or fails) at Sui inclusion; the desk sees it at
    /// indexer detection. A failed PTB aborts atomically: no balance moves.
    fn on_exercised(&mut self, at_: FillTime, x: ExerciseCmd) {
        let key = option_id(x.position_id);
        if !self.studies.contains_key(&x.position_id) {
            return;
        }
        self.apply(LedgerEvent::ExerciseSettled { op: x.op, ok: !x.failed, actual: None, at_ms: ms_u64(at_.ms()) });
        if x.failed {
            self.xstats.ptb_failed += 1;
            self.stats.exercise_failed += 1;
            if !self.ledger.options.contains_key(&key) {
                let st = self.studies.remove(&x.position_id).expect("present");
                self.close_position(at_.ms(), x.spot, st);
            }
            return;
        }
        self.studies.get_mut(&x.position_id).expect("present").exercise_net += x.net;
        self.stats.volumes.exercise_spot_turnover += x.turnover;
        if x.is_put { self.stats.exercised_put += 1 } else { self.stats.exercised_call += 1 }
        self.xstats.path(x.path);
        if !self.ledger.options.contains_key(&key) {
            let st = self.studies.remove(&x.position_id).expect("present");
            self.close_position(at_.ms(), x.spot, st);
        }
        let at = at_;
        // The Sui PTB is atomic; the perp close is not: send it now, after
        // the configured extra delay, through the normal hedge path (the
        // working order counts against the band so the rebalancer does
        // not double up).
        let size = x.delta_per_unit * x.units;
        if size.abs() >= self.s.venue.min_order_units {
            let delay = self.s.exercise.hedge_close_delay_ms.max(0) + self.lat.draw(LatencyStage::Strategy);
            let order = HedgeOrder { id: self.next_order_id, size_units: size, spot: x.spot };
            self.next_order_id += 1;
            self.open.submit(&order, at.ms() + delay);
            self.schedule(at.ms() + delay, Stage::Command, 0, Queued::Command(Command::Hedge(HedgeCommand::Submit(order), OrderKind::Taker)));
            self.xstats.hedge_close_delay_ms_sum += delay;
            self.xstats.hedge_closes += 1;
        }
    }

    /// A position leaves the book: exercised in full, or expired (an
    /// unexercised option is worthless on chain — `burn_expired_option`).
    fn close_position(&mut self, now: i64, spot: f64, p: Study) {
        self.stats.expiries_settled += 1;
        let payoff = p.exercise_net;
        let life: Vec<(i64, f64)> = self.price_samples.iter().copied().filter(|(t, _)| *t >= p.opened_ms && *t <= now).collect();
        let sigma_realized = crate::estimator::realized_vol(&life, now - p.opened_ms + 1, now).unwrap_or(0.0);
        let tau = (p.expiry_ms - p.opened_ms) as f64 / MS_PER_YEAR_F;
        let vol_pnl_proxy = 0.5 * p.gamma_open * p.spot_open * p.spot_open * (sigma_realized.powi(2) - p.sigma_paid.powi(2)) * tau * p.qty_open;
        self.settled.push(SettledOption {
            id: p.id, is_put: p.is_put, strike: p.strike, opened_ms: p.opened_ms, expiry_ms: p.expiry_ms, qty: p.qty_open,
            spot_open: p.spot_open, spot_close: spot, premium_paid: p.premium_paid, payoff, sigma_paid: p.sigma_paid,
            sigma_surface: p.sigma_surface, sigma_realized, vol_pnl_proxy, option_leg_pnl: payoff - p.premium_paid,
        });
    }

    /// Expiry: whatever is still held expires worthless; the intrinsic
    /// value it carried is reported as the exercise model's miss.
    fn expire_worthless(&mut self, held: f64, spot: f64, p: &Study) {
        if held > 1e-12 {
            self.xstats.expired_unexercised += 1;
            let intrinsic = if p.is_put { (p.strike - spot).max(0.0) } else { (spot - p.strike).max(0.0) };
            // Only economically exercisable value counts as a miss: the
            // policy leaves sub-threshold intrinsic to expire by design.
            if intrinsic * held >= self.s.exercise.min_profit_usd {
                self.xstats.expired_unexercised_itm += 1;
                self.xstats.expired_itm_value += intrinsic * held;
            }
        }
    }

    // ── commands and outcomes ──────────────────────────────────────────

    fn on_command(&mut self, at: CommandTime, cmd: Command) {
        match cmd {
            Command::Hedge(c, kind) => {
                if let HedgeCommand::Submit(o) = &c {
                    self.apply(LedgerEvent::HedgeSubmitted {
                        op: o.id,
                        market: self.perp_key.clone(),
                        size_units: o.size_units,
                        spot: o.spot,
                        at_ms: ms_u64(at.ms()),
                    });
                }
                let arrival = at.ms() + self.lat.draw(LatencyStage::VenueSubmit);
                let market = self.market;
                let account = self.account();
                let timed = self.venue.execute(c, kind, arrival, &market, &account, &mut self.lat);
                self.schedule_outcomes(timed);
            }
            Command::Exercise(mut x) => {
                // Pre-simulated and submitted: it fails atomically (hazard)
                // or lands at inclusion; the desk sees either at detection.
                x.failed = self.hazard.fails();
                let landed = at.ms() + self.lat.draw(LatencyStage::SuiInclusion);
                let detected = landed + self.lat.draw(LatencyStage::IndexerDetection);
                self.schedule(detected, Stage::Outcome, 0, Queued::Exercised(x));
            }
            Command::TopUp(amount) => {
                // Cash leaves the vault now; it is margin once it lands.
                let op = self.next_op();
                self.topup_ops.push_back(op);
                self.apply(LedgerEvent::MarginTopUpSent { op, market: self.perp_key.clone(), amount, at_ms: ms_u64(at.ms()) });
                let arrival = at.ms() + self.s.margin.topup_transfer_ms.max(0);
                let timed = self.venue.topup(amount, arrival);
                self.schedule_outcomes(timed);
            }
        }
    }

    fn on_outcome(&mut self, at: FillTime, t: Timed) {
        match t.ev {
            VenueEvent::Hedge(ev) => {
                match &ev {
                    HedgeEvent::Rejected { order, .. } => {
                        self.apply(LedgerEvent::HedgeResolved { op: *order, rejected: true, at_ms: ms_u64(at.ms()) });
                    }
                    HedgeEvent::Cancelled(order) => {
                        self.apply(LedgerEvent::HedgeResolved { op: *order, rejected: false, at_ms: ms_u64(at.ms()) });
                    }
                    _ => {}
                }
                let partial = matches!(ev, HedgeEvent::PartiallyFilled(_));
                if let Some((fill, _)) = self.open.apply(&ev) {
                    self.apply_fill(at, fill.order, partial, fill.size_units, fill.price, t.fee, t.reference);
                }
            }
            VenueEvent::Liquidated { size_closed, price, penalty, full } => {
                let before = self.perp().units;
                self.apply(LedgerEvent::Liquidation {
                    market: self.perp_key.clone(),
                    size_closed,
                    price,
                    penalty,
                    full,
                    at_ms: ms_u64(at.ms()),
                });
                self.stats.volumes.hedge_turnover += size_closed.abs() * price;
                self.stats.liquidations += 1;
                self.first_liquidation_ms.get_or_insert(at.ms());
                if !full {
                    self.sync_collateral(before, size_closed, price);
                }
                // The venue dropped every working order.
                self.open = OpenOrders::default();
            }
            VenueEvent::TopUp { accepted, .. } => {
                let op = self.topup_ops.pop_front().expect("a top-up outcome matches a sent transfer");
                self.apply(LedgerEvent::MarginTopUpLanded { op, accepted, at_ms: ms_u64(at.ms()) });
            }
        }
    }

    /// A fill reaches the ledger: realized P&L on the closed slice, the
    /// venue's fee, slippage as the signed distance from the mark at
    /// execution (a passive fill inside the mark is negative), gas per
    /// rebalance, then the isolated margin is re-synced to the position.
    #[allow(clippy::too_many_arguments)]
    fn apply_fill(&mut self, at_: FillTime, op: u64, partial: bool, size: f64, price: f64, fee: f64, reference: f64) {
        let s = self.s;
        let notional = size.abs() * reference;
        let slip = (price - reference) * size.signum();
        let before = self.perp().units;
        self.apply(LedgerEvent::PerpFill {
            op: Some(op),
            market: self.perp_key.clone(),
            size_units: size,
            price,
            fee,
            reference,
            gas: s.exercise.gas_per_rebalance,
            passive: slip < 0.0,
            partial,
            at_ms: ms_u64(at_.ms()),
        });
        // The fill carried the mark at execution; the account is marked
        // at the current bar.
        let mark = self.mark();
        self.apply(LedgerEvent::MarkPerp { market: self.perp_key.clone(), mark, at_ms: ms_u64(at_.ms()) });
        self.stats.volumes.hedge_turnover += notional;
        if self.ledger.lines.hedge_fills == 1 {
            self.stats.initial_hedge_margin = self.perp().units.abs() * reference * s.hedge.initial_margin_fraction;
        }
        self.sync_collateral(before, size, price);
    }

    /// Isolated margin follows the position the way the venue assigns
    /// it: a fill that extends the position posts entry margin on the
    /// added notional (from cash, bounded by it); a fill that reduces it
    /// releases the closed slice's share of the margin; a reversal does
    /// both. Losses erode the ratio — only a top-up (or the risk engine)
    /// restores it.
    fn sync_collateral(&mut self, before: f64, size: f64, price: f64) {
        let closed = if before == 0.0 || before.signum() == size.signum() { 0.0 } else { size.abs().min(before.abs()) };
        let market = self.perp_key.clone();
        if closed > 0.0 {
            let release = self.perp().collateral * closed / before.abs();
            self.apply(LedgerEvent::MarginMoved { market: market.clone(), amount: -release, at_ms: self.ledger.last_event_ms });
        }
        let added = size.abs() - closed;
        if added > 0.0 {
            let post = entry_margin(added * price, &self.s.margin).min(self.ledger.settlement.max(0.0));
            self.apply(LedgerEvent::MarginMoved { market, amount: post, at_ms: self.ledger.last_event_ms });
        }
    }

    // ── ledger stage ───────────────────────────────────────────────────

    /// Capacity stats (reservations, premium at risk, margin, gates), NAV,
    /// drawdown and the daily sample — after every outcome of the minute.
    fn on_nav_sample(&mut self, now: i64) {
        let Some(ctx) = self.cur.filter(|c| c.ms == now) else { return };
        let spot = ctx.spot;
        let mark = self.mark();
        let reserved: f64 = self.live.iter().map(|q| q.bid).sum();
        self.stats.sample_reserved(reserved);
        // The margin actually posted at the venue (doc 08 §7.3).
        let margin_req = self.perp().collateral;
        self.stats.peak_hedge_margin = self.stats.peak_hedge_margin.max(margin_req);
        self.stats.peak_24h_margin_topup = self.stats.peak_24h_margin_topup.max(self.topup.push(now, margin_req));
        let free = self.ledger.settlement - reserved;
        self.stats.min_free_settlement = self.stats.min_free_settlement.min(free);
        self.stats.min_margin_headroom = self.stats.min_margin_headroom.min(free);
        if ctx.revalue_now || ctx.filled {
            let marks = self.ledger.option_marks();
            self.stats.peak_premium_at_risk_total = self.stats.peak_premium_at_risk_total.max(marks + reserved);
            let call_res: f64 = self.live.iter().filter(|q| !q.rfq.is_put).map(|q| q.bid).sum();
            self.stats.peak_premium_at_risk_call = self.stats.peak_premium_at_risk_call.max(ledger::premium_by_type(&self.ledger, false) + call_res);
            self.stats.peak_premium_at_risk_put = self.stats.peak_premium_at_risk_put.max(ledger::premium_by_type(&self.ledger, true) + reserved - call_res);
            let mut by_expiry: BTreeMap<i64, f64> = ledger::premium_by_expiry(&self.ledger);
            for q in &self.live {
                *by_expiry.entry(q.rfq.expiry_ms).or_default() += q.bid;
            }
            let peak_exp = by_expiry.values().copied().fold(0.0, f64::max);
            self.stats.peak_expiry_premium_at_risk = self.stats.peak_expiry_premium_at_risk.max(peak_exp);
            self.stats.peak_capital_deployed = self.stats.peak_capital_deployed.max(marks + reserved + margin_req);
        }

        let nav = self.ledger.nav();
        self.peak = self.peak.max(nav);
        if self.peak > 0.0 {
            self.max_dd = self.max_dd.max((self.peak - nav) / self.peak);
        }
        let day = now.div_euclid(MS_PER_DAY);
        if day != self.last_nav_day {
            self.last_nav_day = day;
            self.nav_path.push(NavPoint {
                ts_ms: now, spot, nav, cash: self.ledger.settlement, option_marks: self.ledger.option_marks(),
                perp_position: self.perp().units, net_delta_units: self.net_delta_units,
                premium_deployed_pct: if nav > 0.0 { self.ledger.option_marks() / nav } else { 0.0 },
                sigma_surface: if ctx.readout.fallback { None } else { Some(ctx.readout.surface.atm(self.tenor_years)) },
                stale: ctx.stale,
                margin_ratio: self.account().margin_ratio(mark),
            });
        }
    }

    fn dispatch(&mut self, key: Key, ev: Queued) {
        self.trace(key.ms, key.stage, key.sub);
        match ev {
            Queued::Observation(o) => self.on_observation(o),
            Queued::Timer(t) => self.on_timer(key.ms, t),
            Queued::Command(c) => self.on_command(CommandTime(key.ms), c),
            Queued::Outcome(t) => self.on_outcome(FillTime(key.ms), t),
            Queued::Exercised(x) => self.on_exercised(FillTime(key.ms), x),
            Queued::NavSample => self.on_nav_sample(key.ms),
        }
    }
}

/// In-memory run (tests, sweeps, the solver): the slices become sources.
pub fn run(s: &Scenario, bars: &[Bar], funding: &[FundingRow], vol_index: &[(i64, f64)]) -> Result<RunOutput> {
    let start_ms = crate::data::date_start_ms(&s.from)?;
    let mut source = flow_source(s, start_ms)?;
    run_with(s, bars, funding, vol_index, source.as_mut())
}

/// In-memory run with an explicit flow source.
pub fn run_with(s: &Scenario, bars: &[Bar], funding: &[FundingRow], vol_index: &[(i64, f64)], source: &mut dyn FlowSource) -> Result<RunOutput> {
    anyhow::ensure!(!bars.is_empty(), "no bars for {}/{} in {}..{}", s.spot_exchange, s.spot_symbol, s.from, s.to);
    if s.estimator.kind == "vol_index" {
        anyhow::ensure!(!vol_index.is_empty(), "estimator.kind = vol_index needs a vol_index series");
    }
    run_sources_with(s, Sources::from_slices(bars, funding, vol_index), source)
}

/// The replay over pull sources (one batch per source in memory).
pub fn run_sources(s: &Scenario, sources: Sources) -> Result<RunOutput> {
    let start_ms = crate::data::date_start_ms(&s.from)?;
    let mut source = flow_source(s, start_ms)?;
    run_sources_with(s, sources, source.as_mut())
}

pub fn run_sources_with(s: &Scenario, sources: Sources, source: &mut dyn FlowSource) -> Result<RunOutput> {
    anyhow::ensure!(
        matches!(s.venue.execution_assumption.as_str(), "taker_only" | "optimistic" | "central" | "conservative"),
        "unknown venue.execution_assumption {}",
        s.venue.execution_assumption
    );
    let start_ms = crate::data::date_start_ms(&s.from)?;
    let end_ms = crate::data::date_start_ms(&s.to)? + MS_PER_DAY;
    let mut ext = Merge::new(vec![sources.bars, sources.funding, sources.vol_index])?;
    let venue = SimPerpVenue::new(s.venue.clone(), s.margin.clone(), s.hedge.slippage_bps, s.hedge.taker_fee_bps, s.hedge.fixed_fee_per_fill);
    let execution_assumption = venue.execution_assumption().to_string();
    let acceptance = AcceptanceModel::new(s.acceptance.clone(), s.seed);
    let acceptance_label = acceptance.label();
    let mut e = Engine {
        s,
        source,
        start_ms,
        end_ms,
        interval_ms: s.estimator.sample_interval_s * 1000,
        revalue_ms: s.revalue_interval_min.max(1) * 60_000,
        tenor_years: s.flow.tenor_days / 365.0,
        fee_wedge: 1.0 - s.fees.protocol_premium_fee_bps / 10_000.0,
        est: WindowsEstimator::new(s.estimator.clone(), s.flow.tenor_days),
        oracle: OracleProxy::new(s.oracle.clone()),
        ledger: Ledger::new(s.nav0),
        studies: BTreeMap::new(),
        counters: Counters::default(),
        next_id: 0,
        next_op: 0,
        perp_key: s.asset.clone(),
        topup_ops: VecDeque::new(),
        v1: v1_params(s),
        hedge_cost: hedge_cost_params(s),
        acceptance,
        stats: RunStats::default(),
        live: Vec::new(),
        topup: TrailingMin::new(MS_PER_DAY),
        queue: EventQueue::default(),
        lat: LatencyModel::new(s.latency.clone()),
        gaps: GapTracker::new(s.gaps.clone(), start_ms, end_ms),
        venue,
        open: OpenOrders::default(),
        next_order_id: 1,
        escalate: false,
        market: MarketState { ts_ms: i64::MIN, spot: 0.0, low: 0.0, high: 0.0, volume: 0.0 },
        last_spot: 0.0,
        spot_start: None,
        price_samples: Vec::new(),
        last_sample_ms: i64::MIN,
        funding_annual: 0.0,
        funding_settlements: 0,
        index_vol: None,
        last_revalue_ms: i64::MIN,
        apy_call: None,
        apy_put: None,
        cur: None,
        nav_path: Vec::new(),
        settled: Vec::new(),
        minutes_with_bar: 0,
        minutes_stale: 0,
        peak: s.nav0,
        max_dd: 0.0,
        last_nav_day: i64::MIN,
        net_delta_units: 0.0,
        topups: VecDeque::new(),
        min_margin_ratio: None,
        closest_headroom: None,
        first_liquidation_ms: None,
        hazard: PtbHazard::new(s.seed, s.exercise.ptb_failure_prob),
        xstats: ExerciseStats::default(),
        timer_counts: BTreeMap::new(),
        stage_counts: BTreeMap::new(),
        trace_hash: 0xcbf2_9ce4_8422_2325,
    };
    e.schedule(start_ms, Stage::Timer, Timer::Mark.sub(), Queued::Timer(Timer::Mark));
    e.schedule(start_ms, Stage::Timer, Timer::Flow.sub(), Queued::Timer(Timer::Flow));
    e.schedule(start_ms, Stage::Timer, Timer::Hedge.sub(), Queued::Timer(Timer::Hedge));
    e.schedule(start_ms, Stage::Timer, Timer::Exit.sub(), Queued::Timer(Timer::Exit));
    e.schedule(start_ms, Stage::Timer, Timer::Margin.sub(), Queued::Timer(Timer::Margin));

    // Pull external rows and queued events in `(ms, stage)` order; an
    // external row at the same instant as a queued event runs first
    // (stage 0). Rows before `from` warm the oracle and estimator only.
    loop {
        let ext_ts = ext.peek_ts().filter(|t| *t < end_ms);
        let key = e.queue.peek_key();
        match (ext_ts, key) {
            (None, None) => break,
            (Some(t), Some(k)) if t <= k.ms => {
                let row = ext.next_row()?.expect("peeked");
                e.trace(t, Stage::External, 0);
                e.on_external(row);
            }
            (Some(t), None) => {
                let row = ext.next_row()?.expect("peeked");
                e.trace(t, Stage::External, 0);
                e.on_external(row);
            }
            (_, Some(k)) => {
                if k.ms >= end_ms {
                    break;
                }
                let (key, ev) = e.queue.pop().expect("peeked");
                e.dispatch(key, ev);
            }
        }
    }
    anyhow::ensure!(e.spot_start.is_some(), "no bars for {}/{} in {}..{}", s.spot_exchange, s.spot_symbol, s.from, s.to);
    // Quotes still live at the end never reached a terminal event.
    e.stats.quotes_expired += e.live.len() as u64;
    e.counters.declines_stale += e.source.stale_declines();
    let pending_outcomes = e.queue.len() as u64;
    let spot_end = e.last_spot;
    let nav_end = e.ledger.nav();
    let spot_start = e.spot_start.unwrap_or(spot_end);
    let coverage = e.gaps.finish();
    Ok(RunOutput {
        nav_path: e.nav_path,
        settled: e.settled,
        ledger: e.ledger,
        counters: e.counters,
        minutes_total: ((end_ms - start_ms) / 60_000) as u64,
        minutes_with_bar: e.minutes_with_bar,
        minutes_stale: e.minutes_stale,
        funding_settlements: e.funding_settlements,
        turns: e.source.turns(),
        max_drawdown: e.max_dd,
        nav_end,
        spot_start,
        spot_end,
        stats: e.stats,
        flow_source: e.source.label(),
        acceptance: acceptance_label,
        coverage,
        source_rows: ext.row_counts(),
        timer_counts: e.timer_counts,
        stage_counts: e.stage_counts,
        trace_hash: format!("{:016x}", e.trace_hash),
        latency_draws: e.lat.draws(),
        pending_outcomes,
        latency: s.latency.clone(),
        execution_assumption,
        venue_labels: VenueLabels {
            proxy_venue: true,
            imr: s.margin.imr,
            mmr: s.margin.mmr,
            leverage: s.margin.leverage,
            partial_liquidation_assumed: s.margin.partial_close > 0.0,
            basis_configured: !s.venue.basis.is_empty(),
        },
        min_margin_ratio: e.min_margin_ratio,
        closest_margin_headroom: e.closest_headroom,
        first_liquidation_ms: e.first_liquidation_ms,
        exercise: e.xstats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::Scenario;
    use crate::synth::synthetic_bars;

    #[allow(clippy::field_reassign_with_default)]
    fn scenario() -> Scenario {
        let mut s = Scenario::default();
        s.from = "2025-01-01".into();
        s.to = "2025-03-11".into();
        s.nav0 = 1_000_000.0;
        s.flow.tenor_days = 30.0;
        // Doc 07 framing: the whole 30% budget sits in one expiry and the
        // bid is fair − 5 vol points with no size penalty.
        s.limits.per_expiry_max = 0.30;
        s.limits.call_premium_max = 0.30;
        s.bid.size_penalty_volpts_per_pct_nav = 0.0;
        s.latency = LatencyConfig::zero();
        s
    }

    fn funding_rows(days: i64, start: i64) -> Vec<FundingRow> {
        (0..days * 3).map(|i| FundingRow { ts_ms: start + i * 8 * 3_600_000, rate: 0.0001, interval_hours: 8.0 }).collect()
    }

    fn close(a: f64, b: f64, rel: f64) -> bool {
        (a - b).abs() <= rel * a.abs().max(b.abs()).max(1.0)
    }

    /// The cash identity every run must satisfy (doc 08 §1 item 9):
    /// premium, payoff, hedge realized, funding, fees, gas, forfeited
    /// margin and the collateral parked at the venue.
    fn assert_cash_identity(s: &Scenario, a: &RunOutput) {
        let l = &a.ledger.lines;
        let cash_expected = s.nav0 - l.premium_paid + l.option_payoff + l.hedge_realized - l.funding_paid - l.hedge_fees - l.maker_fees - l.gas
            - l.liquidation_loss
            - a.ledger.perp_collateral();
        assert!((a.ledger.settlement - cash_expected).abs() < 1e-6, "cash {} vs identity {}", a.ledger.settlement, cash_expected);
        // The exact ledger reconciles after the full replay (doc 08 §12
        // item 1; every event is asserted in debug builds).
        assert_eq!(a.ledger.check_accounting(), Vec::new(), "nav {} explained {}", a.ledger.nav(), a.ledger.nav_explained());
        assert!((a.nav_end - a.ledger.nav()).abs() < 1e-6);
    }

    #[test]
    fn ledger_reconciles_every_day_and_is_deterministic() {
        let s = scenario();
        let start = crate::data::date_start_ms(&s.from).unwrap();
        let bars = synthetic_bars(70, start);
        let funding = funding_rows(70, start);
        let a = run(&s, &bars, &funding, &[]).unwrap();
        let b = run(&s, &bars, &funding, &[]).unwrap();
        assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap(), "not deterministic");
        assert_eq!(a.trace_hash, b.trace_hash);
        assert!(a.turns >= 2, "{}", a.turns);
        assert!(a.ledger.lines.fills >= 2, "fills {} declines cap {} stale {} zero {}", a.ledger.lines.fills, a.counters.declines_capacity, a.counters.declines_stale, a.counters.declines_priced_zero);
        assert!(a.funding_settlements > 0);
        assert!(a.ledger.lines.hedge_fills > 0, "hedge never traded");
        assert_cash_identity(&s, &a);
        for p in &a.nav_path {
            assert!(p.nav.is_finite() && p.cash.is_finite());
        }
        assert!((a.nav_end - (a.ledger.settlement + a.ledger.option_marks() + a.ledger.perp_collateral() + a.ledger.perp_unrealized())).abs() < 1e-6);
        assert!(a.settled.iter().all(|o| o.sigma_realized > 0.0));
        assert_eq!(a.pending_outcomes, 0);
        assert_eq!(a.ledger.lines.liquidations, 0);
        assert!(a.min_margin_ratio.is_some_and(|m| m > s.margin.mmr));
        assert!(a.closest_margin_headroom.is_some_and(|h| h > 0.0));
        assert!(a.nav_path.iter().any(|p| p.margin_ratio.is_some()), "margin is posted on the open hedge");
        // The six volumes are all reported; instant acceptance means
        // quoted == accepted and premium turnover == premium paid.
        let (v, l) = (&a.stats.volumes, &a.ledger.lines);
        assert!(v.offered_earn_notional > 0.0);
        assert_eq!(v.quoted_earn_notional, v.accepted_earn_notional);
        assert!((v.premium_turnover - l.premium_paid).abs() < 1e-6);
        assert!((v.hedge_turnover - l.hedge_turnover_notional).abs() < 1e-6);
        assert!((v.exercise_spot_turnover - l.exercise_turnover_notional).abs() < 1e-6);
        assert_eq!(a.flow_source, "constant");
        assert_eq!(a.acceptance, "instant");
    }

    /// v0 (SO-439, `4af188ad`) summary on this synthetic path, captured
    /// before the event-queue rewrite. PR K reproduced it to 1e-9 with
    /// fills at the decision price; PR L executes on the bar path (the
    /// venue's truth) and posts isolated margin; PR M replaces the
    /// at-expiry settlement with the near-expiry sweep through the
    /// modeled route (exercised up to 24 h early, sold into the ladder,
    /// the hedge closed on detection), so the option leg and the hedge
    /// fill count move. What must not move: turns, option fills, the
    /// premium paid; NAV stays within 2% of NAV0 and every ITM option is
    /// exercised. Doc 10 §2 turnover/cost tables rest on this.
    #[test]
    fn zero_latency_taker_replay_stays_within_tolerance_of_v0() {
        let s = scenario();
        let start = crate::data::date_start_ms(&s.from).unwrap();
        let bars = synthetic_bars(70, start);
        let funding = funding_rows(70, start);
        let a = run(&s, &bars, &funding, &[]).unwrap();
        let m = crate::report::summarize(&s, &a);
        assert_eq!(m.execution_assumption, "taker_only");
        assert!(m.labels.taker_only);
        assert_eq!((m.turns, m.fills, m.declines_stale), (3, 3, 1));
        assert!((m.hedge_fills as i64 - 207).abs() <= 40, "{}", m.hedge_fills);
        assert!(close(m.nav_end, 884_850.14, 0.02), "{}", m.nav_end);
        assert!(close(m.premium_paid, 509_138.52, 0.01), "{}", m.premium_paid);
        assert!(close(m.funding_paid, -17_697.32, 0.05), "{}", m.funding_paid);
        assert!(close(m.hedge_fees, 11_997.53, 0.20), "{}", m.hedge_fees);
        assert!(close(m.max_drawdown, 0.1411, 0.15), "{}", m.max_drawdown);
        assert_eq!(m.exercise_stats.expired_unexercised_itm, 0);
        assert!(m.exercise_stats.paths.contains_key("call_cash"), "{:?}", m.exercise_stats);
        assert!(close(m.mean_sigma_realized, 0.435_539_138_349_790_4, 0.01), "{}", m.mean_sigma_realized);
        let mut s = scenario();
        s.flow.call_share = 0.5;
        s.limits.put_premium_max = 0.30;
        let b = run(&s, &bars, &funding, &[]).unwrap();
        let m = crate::report::summarize(&s, &b);
        assert_eq!(m.fills, 6);
        assert!((m.hedge_fills as i64 - 199).abs() <= 40, "{}", m.hedge_fills);
        assert!(close(m.nav_end, 861_564.87, 0.03), "{}", m.nav_end);
        assert_eq!(m.exercise_stats.expired_unexercised_itm, 0, "{:?}", m.exercise_stats);
    }

    /// Doc 08 P2 gate: a known synthetic week replays with source row
    /// counts reconciled, stable timer counts and a monotone trace.
    #[test]
    fn known_week_reconciles_rows_timers_and_ordering() {
        let mut s = scenario();
        s.to = "2025-01-07".into();
        s.vol_index_symbol = "X".into();
        let start = crate::data::date_start_ms(&s.from).unwrap();
        let bars = synthetic_bars(7, start);
        let funding = funding_rows(7, start);
        let index: Vec<(i64, f64)> = (0..7 * 24).map(|h| (start + h * 3_600_000, 60.0)).collect();
        let a = run(&s, &bars, &funding, &index).unwrap();
        assert_eq!(a.source_rows, vec![("spot".to_string(), 7 * 1440), ("funding".to_string(), 21), ("vol_index".to_string(), 168)]);
        assert_eq!(a.coverage.feeds["spot"].rows, 7 * 1440);
        assert_eq!(a.funding_settlements, 21);
        assert_eq!(a.timer_counts["mark"], 7 * 1440);
        assert_eq!(a.timer_counts["flow"], 7 * 1440);
        assert_eq!(a.timer_counts["hedge"], 7 * 1440);
        assert_eq!(a.timer_counts["margin"], 7 * 1440);
        assert_eq!(a.stage_counts["external"], 7 * 1440 + 21 + 168);
        assert_eq!(a.stage_counts["ledger"], 7 * 1440);
        // One turn at the start (retried once: the first minute is stale
        // under the 2 s oracle latency) — the next turn is past the week.
        assert_eq!((a.turns, a.counters.declines_stale), (1, 1));
        assert!(a.coverage.gaps.is_empty());
        assert_eq!(a.latency_draws, 0);
        let b = run(&s, &bars, &funding, &index).unwrap();
        assert_eq!(a.trace_hash, b.trace_hash);
        // With latencies on, the trace changes but stays reproducible.
        s.latency = LatencyConfig::default();
        let c = run(&s, &bars, &funding, &index).unwrap();
        let d = run(&s, &bars, &funding, &index).unwrap();
        assert_ne!(a.trace_hash, c.trace_hash);
        assert_eq!(c.trace_hash, d.trace_hash);
        assert!(c.latency_draws > 0);
        assert_eq!(c.ledger.lines.hedge_fills, a.ledger.lines.hedge_fills, "taker fills still complete under latency");
    }

    /// Staleness declines match the oracle model: with a 2 s publish
    /// latency and a 180 s max age, a hole of H minutes yields exactly
    /// H − 2 stale minutes inside it plus the minute the feed returns.
    #[test]
    fn staleness_matches_the_oracle_model_through_a_hole() {
        let mut s = scenario();
        s.to = "2025-01-03".into();
        let start = crate::data::date_start_ms(&s.from).unwrap();
        let mut bars = synthetic_bars(3, start);
        let hole_start = start + MS_PER_DAY + 6 * 3_600_000;
        let hole_minutes = 90;
        bars.retain(|b| !(b.ts_ms >= hole_start && b.ts_ms < hole_start + hole_minutes * 60_000));
        let out = run(&s, &bars, &[], &[]).unwrap();
        assert_eq!(out.minutes_with_bar, 3 * 1440 - hole_minutes as u64);
        // First run minute (nothing actionable yet) + the hole's stale span.
        assert_eq!(out.minutes_stale, 1 + (hole_minutes as u64 - 2));
        assert_eq!(out.coverage.gaps.len(), 1);
        let g = &out.coverage.gaps[0];
        assert_eq!((g.start_ms, g.end_ms), (hole_start - 60_000 + s.gaps.max_gap_ms, hole_start + hole_minutes * 60_000));
    }

    /// Doc 08 §6.4 gate: a capture hole advances the flow timer, funding,
    /// and expiry; risk never freezes, and the expiry that settled on a
    /// cached price is reported as an invalidated span.
    #[test]
    fn capture_hole_advances_timers_funding_and_expiry_and_reports_it() {
        let s = scenario();
        let start = crate::data::date_start_ms(&s.from).unwrap();
        let mut bars = synthetic_bars(70, start);
        // Remove the second half of day 29 and all of day 30 — the first
        // expiry (day 30) and the second turn land inside the hole.
        bars.retain(|b| !(b.ts_ms >= start + 29 * MS_PER_DAY + MS_PER_DAY / 2 && b.ts_ms < start + 31 * MS_PER_DAY));
        let funding = funding_rows(70, start);
        let out = run(&s, &bars, &funding, &[]).unwrap();
        assert_eq!(out.minutes_total, 70 * 1440);
        assert_eq!(out.minutes_with_bar, 70 * 1440 - 2160);
        assert_eq!(out.timer_counts["mark"], 70 * 1440, "timers ran through the hole");
        assert_eq!(out.funding_settlements, 210, "funding settled through the hole");
        // The turn that lands in the hole is declined every minute until
        // the price is fresh again, then filled — never skipped in time.
        assert!(out.counters.declines_stale > 1000, "{}", out.counters.declines_stale);
        assert!(out.turns >= 2);
        // The first turn's expiry (day 30 + the stale first minute)
        // settled inside the hole on the cached price.
        let hole = (start + 29 * MS_PER_DAY + MS_PER_DAY / 2)..(start + 31 * MS_PER_DAY);
        let expiry = out.settled.iter().map(|o| o.expiry_ms).find(|e| hole.contains(e)).expect("an expiry inside the hole");
        let inv = &out.coverage.invalidated_spans;
        assert!(inv.iter().any(|i| i.reason == "expiry settlement" && i.start_ms <= expiry && i.end_ms >= expiry && !i.bounded), "{inv:?}");
        assert_eq!(out.coverage.gaps.len(), 1);
        assert!(out.coverage.feeds["spot"].fraction < 1.0);
        // NAV kept being sampled (stale) through the hole.
        assert!(out.nav_path.iter().any(|p| p.stale && p.ts_ms >= start + 30 * MS_PER_DAY));
        // Bound policy: same outcome, labeled bounded instead.
        let mut s2 = scenario();
        s2.gaps.policy = "bound".into();
        let out2 = run(&s2, &bars, &funding, &[]).unwrap();
        assert!(out2.coverage.invalidated_spans.iter().all(|i| i.bounded));
        assert_eq!(out2.nav_end, out.nav_end);
    }

    #[test]
    fn generated_flow_with_hazard_acceptance_reserves_then_fills_or_expires() {
        let mut s = scenario();
        s.to = "2025-01-15".into();
        s.flow.source = "generated".into();
        s.acceptance.mode = "hazard".into();
        s.revalue_interval_min = 15;
        s.limits.per_expiry_max = 0.10;
        s.limits.call_premium_max = 0.20;
        let start = crate::data::date_start_ms(&s.from).unwrap();
        let bars = synthetic_bars(15, start);
        let a = run(&s, &bars, &[], &[]).unwrap();
        let b = run(&s, &bars, &[], &[]).unwrap();
        assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap(), "not deterministic");
        let st = &a.stats;
        assert!(st.rfqs_offered > 20, "{}", st.rfqs_offered);
        assert!(st.rfqs_call > 0 && st.rfqs_put > 0);
        assert!(st.quotes_sent > 0);
        assert!(st.quotes_accepted > 0, "{st:?}");
        assert!(st.quotes_expired > 0, "some quotes must expire under the hazard: {st:?}");
        assert_eq!(st.quotes_sent, st.quotes_accepted + st.quotes_expired + st.quotes_reverted);
        assert!(st.peak_reserved > 0.0, "premium must be reserved while quotes are live");
        assert!(st.volumes.accepted_earn_notional < st.volumes.quoted_earn_notional);
        assert!(st.volumes.quoted_earn_notional <= st.volumes.offered_earn_notional);
        assert_eq!(a.flow_source, "generated_market");
        assert_eq!(a.acceptance, "hazard_ttl");
        assert_cash_identity(&s, &a);
        assert_eq!(st.resales, 0);
        // A wider bid accepts less of the same flow (common random numbers).
        let mut wide = s.clone();
        wide.bid.base_spread_volpts = 0.30;
        let w = run(&wide, &bars, &[], &[]).unwrap();
        assert!(w.stats.quotes_accepted < st.quotes_accepted, "wide {} vs base {}", w.stats.quotes_accepted, st.quotes_accepted);
    }

    // ── doc 08 P3 gates (PR L) ─────────────────────────────────────────

    /// Funding matches the settlement rows for a SHORT (call hedge:
    /// receives under positive funding) and a LONG (put hedge: pays),
    /// against the signed position at the venue mark: Σ rate × position ×
    /// mark over the rows, replayed from the daily position path bound.
    #[test]
    fn funding_matches_settlements_for_short_and_long_hedges() {
        for (call_share, expect_sign) in [(1.0, -1.0), (0.0, 1.0)] {
            let mut s = scenario();
            s.to = "2025-01-10".into();
            s.flow.call_share = call_share;
            s.limits.put_premium_max = 0.30;
            let start = crate::data::date_start_ms(&s.from).unwrap();
            let bars = synthetic_bars(10, start);
            let funding = funding_rows(10, start);
            let out = run(&s, &bars, &funding, &[]).unwrap();
            assert_eq!(out.funding_settlements, 30);
            assert!(out.ledger.lines.funding_paid * expect_sign > 0.0, "call_share {call_share}: funding_paid {}", out.ledger.lines.funding_paid);
            let pos_max = out.nav_path.iter().map(|p| p.perp_position.abs()).fold(0.0, f64::max);
            let mark_max = bars.iter().map(|b| b.close).fold(0.0, f64::max);
            assert!(out.ledger.lines.funding_paid.abs() <= 30.0 * 0.0001 * pos_max * mark_max * 1.5);
            assert_cash_identity(&s, &out);
        }
    }

    /// Doc 08 P3 gate: a crash proxy on the MARK liquidates the long put
    /// hedge on the crash bar; the run reports the count, the forfeited
    /// margin, the first liquidation instant and the closest headroom,
    /// and the ledger still reconciles. The same path with a mark basis
    /// that offsets the crash on the mark does not liquidate on that
    /// bar: the risk engine uses marks, not trades.
    #[test]
    fn crash_on_marks_liquidates_and_is_reported() {
        let mut s = scenario();
        s.to = "2025-01-20".into();
        s.flow.call_share = 0.0;
        s.limits.put_premium_max = 0.30;
        s.margin.topup_trigger_mr = 0.0; // no top-ups: let the crash bite
        let start = crate::data::date_start_ms(&s.from).unwrap();
        let crash = start + 5 * MS_PER_DAY;
        let mut bars = synthetic_bars(20, start);
        // Day 5: the bar path gaps down 30% (a long perp at 10x is bust).
        for b in bars.iter_mut().filter(|b| b.ts_ms >= crash) {
            for px in [&mut b.open, &mut b.high, &mut b.low, &mut b.close] {
                *px *= 0.7;
            }
        }
        let funding = funding_rows(20, start);
        let out = run(&s, &bars, &funding, &[]).unwrap();
        assert!(out.ledger.lines.liquidations >= 1, "{:?}", out.ledger.lines);
        assert_eq!(out.first_liquidation_ms, Some(crash), "liquidated on the crash bar");
        assert!(out.ledger.lines.liquidation_loss >= 0.0);
        assert!(out.closest_margin_headroom.is_some_and(|h| h < 0.0), "{:?}", out.closest_margin_headroom);
        assert!(out.min_margin_ratio.is_some_and(|m| m < s.margin.mmr));
        assert_cash_identity(&s, &out);
        let m = crate::report::summarize(&s, &out);
        assert_eq!(m.liquidations, out.ledger.lines.liquidations);
        assert_eq!(m.stats.liquidations, out.ledger.lines.liquidations);
        assert_eq!(m.first_liquidation_ms, Some(crash));
        assert!(!m.venue_labels.partial_liquidation_assumed);
        // +50% mark basis from the crash bar: the mark RISES 5% while the
        // trades fall 30%; a long is not liquidated on that bar.
        s.venue.basis = vec![crate::scenario::BasisPoint { from_ms: crash, bps: 5_000.0 }];
        let out2 = run(&s, &bars, &funding, &[]).unwrap();
        assert!(out2.first_liquidation_ms.is_none_or(|t| t > crash + MS_PER_DAY), "{:?}", out2.first_liquidation_ms);
        assert!(out2.venue_labels.basis_configured);
        assert_cash_identity(&s, &out2);
        // A desk that cannot fund entry margin sends no order (no cascade).
        assert!(out.counters.hedge_declines_margin + out.ledger.lines.hedge_fills > 0);
    }

    /// Margin top-ups keep the hedge alive through a slow slide when the
    /// policy is on and land after the transfer latency; a venue outage
    /// refuses them, the cash returns, and the position is liquidated
    /// during the outage instead.
    #[test]
    fn topups_restore_margin_and_outages_refuse_them() {
        let mut s = scenario();
        s.to = "2025-01-20".into();
        s.flow.call_share = 0.0;
        s.limits.put_premium_max = 0.30;
        s.margin.topup_trigger_mr = 0.06;
        // A 1× NAV put book: the long hedge's slide loss is fundable from
        // cash (at 3× NAV the trapped option gains cannot fund the margin
        // and the liquidation is the correct outcome — doc 08 §7.3).
        s.flow.notional_nav_multiple = 1.0;
        // The doc 08 §0.4 24 h cap (10% NAV) would bind on this slide and
        // liquidate anyway; lift it so the test isolates the transfer path.
        s.margin.max_topup_24h_pct_nav = 0.5;
        let start = crate::data::date_start_ms(&s.from).unwrap();
        let slide_start = start + 5 * MS_PER_DAY;
        let mut bars = synthetic_bars(20, start);
        // A slow slide: −0.4% per hour for two days, then flat lower.
        for b in bars.iter_mut().filter(|b| b.ts_ms >= slide_start) {
            let hours = ((b.ts_ms - slide_start) as f64 / 3_600_000.0).min(48.0);
            let f = (1.0 - 0.004f64).powf(hours);
            for px in [&mut b.open, &mut b.high, &mut b.low, &mut b.close] {
                *px *= f;
            }
        }
        let funding = funding_rows(20, start);
        let out = run(&s, &bars, &funding, &[]).unwrap();
        assert!(out.ledger.lines.margin_topups >= 1, "{:?}", out.ledger.lines);
        assert!(out.ledger.lines.topup_total > 0.0);
        assert_eq!(out.ledger.lines.topup_rejects, 0);
        assert_eq!(out.ledger.lines.liquidations, 0, "top-ups keep the hedge alive: {:?}", out.ledger.lines);
        assert_cash_identity(&s, &out);
        // Outage from six hours into the slide: transfers refused, cash
        // returned, and the risk engine (which keeps running) liquidates.
        s.margin.outages = vec![[slide_start + 6 * 3_600_000, slide_start + 3 * MS_PER_DAY]];
        let out2 = run(&s, &bars, &funding, &[]).unwrap();
        assert!(out2.ledger.lines.topup_rejects >= 1, "{:?}", out2.ledger.lines);
        assert!(out2.ledger.lines.liquidations >= 1, "{:?}", out2.ledger.lines);
        assert!(out2.first_liquidation_ms.is_some_and(|t| t >= slide_start + 6 * 3_600_000));
        assert_cash_identity(&s, &out2);
        let m = crate::report::summarize(&s, &out2);
        assert_eq!(m.topup_rejects, out2.ledger.lines.topup_rejects);
    }

    // ── doc 08 P3 gates (PR M) ─────────────────────────────────────────

    /// A rally leaves the calls ITM: the near-expiry sweep exercises every
    /// slice before expiry (cash first, then the quote flash once the free
    /// settlement is spent), each slice reconciles to the ledger, the
    /// non-atomic hedge close follows each detection, and nothing ITM
    /// expires unexercised. With the PTB hazard at 1 every slice fails
    /// atomically: no balance moves, the options expire worthless, and
    /// the miss is reported.
    #[test]
    fn call_sweep_exercises_itm_before_expiry_and_failed_ptbs_move_nothing() {
        let mut s = scenario();
        s.to = "2025-02-05".into();
        s.flow.tenor_days = 14.0;
        let start = crate::data::date_start_ms(&s.from).unwrap();
        let mut bars = synthetic_bars(36, start);
        for b in bars.iter_mut().filter(|b| b.ts_ms >= start + 7 * MS_PER_DAY) {
            for px in [&mut b.open, &mut b.high, &mut b.low, &mut b.close] {
                *px *= 1.3;
            }
        }
        let funding = funding_rows(36, start);
        let out = run(&s, &bars, &funding, &[]).unwrap();
        let x = &out.exercise;
        assert!(x.paths.get("call_cash").copied().unwrap_or(0) > 0, "{x:?}");
        assert!(x.paths.get("call_quote_flash").copied().unwrap_or(0) > 0, "cash runs out inside the ladder: {x:?}");
        assert_eq!(x.expired_unexercised_itm, 0, "{x:?}");
        assert_eq!(x.ptb_failed, 0);
        assert!(x.hedge_closes > 0);
        assert_eq!(x.hedge_close_delay_ms_mean(), Some(0.0), "zero latency: the close is immediate");
        assert!(out.ledger.lines.option_payoff > 0.0);
        assert!(out.settled.iter().any(|o| o.payoff > 0.0 && !o.is_put));
        assert_cash_identity(&s, &out);
        let m = crate::report::summarize(&s, &out);
        assert_eq!(m.labels.exercise, "american_sweep");
        assert!(m.exercise_min_profit_rule.starts_with("max(USD 10"));
        assert!(m.flash_capacity_assumed);
        // Latency on: the close follows detection by the strategy latency
        // plus the configured extra delay, and it is reported.
        let mut l = s.clone();
        l.latency = LatencyConfig::default();
        l.exercise.hedge_close_delay_ms = 5_000;
        let out_l = run(&l, &bars, &funding, &[]).unwrap();
        assert!(out_l.exercise.hedge_close_delay_ms_mean().is_some_and(|d| d >= 5_000.0), "{:?}", out_l.exercise);
        assert_eq!(out_l.exercise.expired_unexercised_itm, 0);
        assert_cash_identity(&l, &out_l);
        // Every PTB fails: atomic, nothing moves, the ITM value is lost.
        let mut f = s.clone();
        f.exercise.ptb_failure_prob = 1.0;
        let out_f = run(&f, &bars, &funding, &[]).unwrap();
        let xf = &out_f.exercise;
        assert!(xf.ptb_failed > 0 && xf.paths.is_empty(), "{xf:?}");
        assert!(xf.expired_unexercised_itm > 0 && xf.expired_itm_value > 0.0, "{xf:?}");
        assert_eq!(out_f.ledger.lines.option_payoff, 0.0);
        assert_eq!(out_f.ledger.lines.exercise_turnover_notional, 0.0);
        assert_eq!(xf.hedge_closes, 0);
        assert_cash_identity(&f, &out_f);
        assert_eq!(out_f.ledger.lines.fills, out.ledger.lines.fills);
    }

    /// A crash leaves the puts ITM: the sweep routes every slice through
    /// the live three-path waterfall — vault underlying when the vault
    /// holds it, else the base flash while the pool has base, else the
    /// quote flash, else a capacity reject that leaves the put to expire —
    /// and each reconciles to the ledger.
    #[test]
    fn put_sweep_routes_like_the_live_waterfall() {
        let mut base = scenario();
        base.to = "2025-02-05".into();
        base.flow.tenor_days = 14.0;
        base.flow.call_share = 0.0;
        base.limits.put_premium_max = 0.30;
        let start = crate::data::date_start_ms(&base.from).unwrap();
        let mut bars = synthetic_bars(36, start);
        for b in bars.iter_mut().filter(|b| b.ts_ms >= start + 7 * MS_PER_DAY) {
            for px in [&mut b.open, &mut b.high, &mut b.low, &mut b.close] {
                *px *= 0.7;
            }
        }
        let funding = funding_rows(36, start);
        let run_with = |f: &dyn Fn(&mut Scenario)| {
            let mut s = base.clone();
            f(&mut s);
            let out = run(&s, &bars, &funding, &[]).unwrap();
            assert_cash_identity(&s, &out);
            out
        };
        // Default: no vault underlying, base capacity present → base flash.
        let a = run_with(&|_| {});
        assert!(a.exercise.paths.get("base_flash").copied().unwrap_or(0) > 0, "{:?}", a.exercise);
        assert!(!a.exercise.paths.contains_key("vault_underlying"));
        assert_eq!(a.exercise.expired_unexercised_itm, 0, "{:?}", a.exercise);
        assert!(a.ledger.lines.option_payoff > 0.0);
        assert!(a.exercise.hedge_closes > 0);
        // Vault holds the underlying → the first route.
        let b = run_with(&|s| s.exercise.vault_free_underlying_units = 5_000_000.0);
        assert!(b.exercise.paths.get("vault_underlying").copied().unwrap_or(0) > 0, "{:?}", b.exercise);
        assert!(!b.exercise.paths.contains_key("base_flash"));
        // No base in the pool → quote flash.
        let c = run_with(&|s| s.exercise.pool_base_balance_units = 0.0);
        assert!(c.exercise.paths.get("quote_flash").copied().unwrap_or(0) > 0, "{:?}", c.exercise);
        assert!(!c.exercise.paths.contains_key("base_flash"));
        // Neither flash side → capacity reject; the puts expire worthless.
        let d = run_with(&|s| {
            s.exercise.pool_base_balance_units = 0.0;
            s.exercise.pool_quote_balance = 0.0;
        });
        assert!(d.exercise.paths.is_empty(), "{:?}", d.exercise);
        assert!(d.exercise.rejects.get("capacity").copied().unwrap_or(0) > 0, "{:?}", d.exercise);
        assert!(d.exercise.expired_unexercised_itm > 0);
        assert!(d.stats.flash_cap_hits > 0);
        assert_eq!(d.ledger.lines.option_payoff, 0.0);
        // The same three decisions, at the same price, reconcile the same:
        // the route cost does not depend on which balance funds it.
        assert!((a.ledger.lines.option_payoff - b.ledger.lines.option_payoff).abs() < 1e-6);
        assert!((a.ledger.lines.option_payoff - c.ledger.lines.option_payoff).abs() < 1e-6);
    }

    /// Passive execution assumptions run end to end: orders rest, time
    /// out, escalate to takers, and the labels say which assumption.
    #[test]
    fn passive_assumptions_label_and_escalate() {
        for assumption in ["optimistic", "central", "conservative"] {
            let mut s = scenario();
            s.to = "2025-01-10".into();
            s.venue.execution_assumption = assumption.into();
            s.venue.queue_depth_units = 10.0;
            s.venue.passive_participation = 0.5;
            let start = crate::data::date_start_ms(&s.from).unwrap();
            let bars = synthetic_bars(10, start);
            let out = run(&s, &bars, &funding_rows(10, start), &[]).unwrap();
            assert_eq!(out.execution_assumption, assumption);
            assert!(out.ledger.lines.hedge_fills > 0, "{assumption}: {:?}", out.ledger.lines);
            assert_cash_identity(&s, &out);
            let m = crate::report::summarize(&s, &out);
            assert!(!m.labels.taker_only);
            if assumption == "conservative" {
                // Flat synthetic bars never trade through: everything
                // escalates to takers after the timeout.
                assert!(out.ledger.lines.cancels > 0);
                assert_eq!(out.ledger.lines.passive_fills, 0);
            }
        }
    }
}
