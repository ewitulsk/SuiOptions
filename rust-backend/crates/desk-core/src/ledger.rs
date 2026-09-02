//! The exact ledger (doc 08 §2.3 / §5.3, SO-451): ONE deterministic,
//! serde-serializable, I/O-free accounting record the live desk and the
//! backtester both keep, event by event.
//!
//! ```text
//! NAV = settlement cash
//!     + spot (underlying) inventory
//!     + exact option marks
//!     + perp collateral (+ margin in transit)
//!     + perp unrealized P&L
//!     − outstanding liabilities (queued withdrawals, flash loans)
//! ```
//!
//! Every [`LedgerEvent`] is double-entry: the balance it moves and the
//! P&L / equity line it posts to move together, so after every event
//!
//! ```text
//! nav0 + Σ realized lines + option mark change + perp unrealized + equity flows
//!     == assets − liabilities
//! ```
//!
//! ([`Ledger::check`] returns every violated invariant; debug builds
//! assert them after each event). The lines are the ledger's OWN
//! accounting decomposition — the attribution layer of doc 08 §9.1
//! explains them, it never defines NAV.
//!
//! Live and simulation differ only in where truth comes from: the
//! backtester applies venue / route outcomes it computed; the live desk
//! applies the same events from chain / venue observations and, on every
//! custody re-sync, books whatever the chain says that the ledger did not
//! predict to the `resync_*` equity lines (deposits, withdrawals, fees
//! the desk did not model). The residuals are reported, never hidden —
//! that is the reconciliation status `/desk/state` serves.
//!
//! Units: settlement raw (live) or settlement USD (sim) for every money
//! figure; option / underlying quantities in raw units — the ledger is
//! unit-agnostic, exact to the caller's scale, `f64` throughout with a
//! relative tolerance ([`TOLERANCE`]) on the reconciliations.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::book::{Reservation, ReservationState};

/// Relative tolerance of the reconciliations (`|a − b| ≤ TOLERANCE ×
/// max(1, |a|, |b|)`): floating sums over ~10⁵ events on ~10⁹-scale raw
/// units stay well inside it; a real accounting error does not.
pub const TOLERANCE: f64 = 1e-6;

/// Option line key: the bucket id (hex) on chain, any unique id in
/// simulation.
pub type OptionId = String;
/// Perp market key: the underlying symbol.
pub type Market = String;
/// Caller-assigned id of one pending operation.
pub type OpId = u64;

// ── positions ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionKind {
    Call,
    Put,
}

impl OptionKind {
    pub fn of(is_put: bool) -> Self {
        if is_put {
            OptionKind::Put
        } else {
            OptionKind::Call
        }
    }
    pub fn is_put(self) -> bool {
        self == OptionKind::Put
    }
}

/// The economics of one option line.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptionSpec {
    pub kind: OptionKind,
    /// Strike in settlement per underlying unit (scaled).
    pub strike: f64,
    pub expiry_ms: u64,
}

/// One long call / put line: exact quantity, cost basis of the units
/// still held, the last mark, and the units whose exercise is in flight.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptionPosition {
    pub spec: OptionSpec,
    /// Underlying units held.
    pub qty: f64,
    /// Premium paid for the units still held (pro-rata reduced on exit).
    pub cost_basis: f64,
    pub mark_per_unit: f64,
    /// Units inside a pending exercise / resale operation.
    pub pending_units: f64,
}

impl OptionPosition {
    pub fn value(&self) -> f64 {
        self.qty * self.mark_per_unit
    }
    /// Units not committed to a pending operation.
    pub fn free_units(&self) -> f64 {
        self.qty - self.pending_units
    }
}

/// One signed perp account (isolated margin): average-entry accounting,
/// collateral posted, realized so far, and the traded cash flow the
/// realized + unrealized reconciliation is checked against.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PerpPosition {
    /// Signed units: positive long, negative short.
    pub units: f64,
    /// Average entry price of the open units.
    pub entry: f64,
    /// Margin posted at the venue.
    pub collateral: f64,
    /// Last venue mark.
    pub mark: f64,
    /// Cumulative realized P&L (fills, reversals, liquidations).
    pub realized: f64,
    /// Cumulative funding paid (negative = received).
    pub funding_paid: f64,
    /// Cumulative venue fees.
    pub fees: f64,
    /// `−Σ size × price` over every fill: the cash the open + closed
    /// units cost. `realized + unrealized == cash_flow + units × mark`.
    pub cash_flow: f64,
}

impl PerpPosition {
    pub fn unrealized(&self) -> f64 {
        self.units * (self.mark - self.entry)
    }

    /// Venue account value: collateral + unrealized.
    pub fn equity(&self) -> f64 {
        self.collateral + self.unrealized()
    }

    /// Apply a signed fill at `px`; returns the realized P&L on any
    /// closed slice (a reversal closes the whole position and opens the
    /// remainder at `px`).
    pub fn fill(&mut self, units: f64, px: f64) -> f64 {
        if units == 0.0 {
            return 0.0;
        }
        self.cash_flow -= units * px;
        let pos = self.units;
        let mut realized = 0.0;
        if pos == 0.0 || pos.signum() == units.signum() {
            let new = pos + units;
            self.entry = (self.entry * pos.abs() + px * units.abs()) / new.abs();
            self.units = new;
        } else {
            let close = units.abs().min(pos.abs());
            realized = (px - self.entry) * close * pos.signum();
            let new = pos + units;
            if new.abs() <= 1e-12 {
                self.units = 0.0;
                self.entry = 0.0;
            } else if new.signum() != pos.signum() {
                self.units = new;
                self.entry = px;
            } else {
                self.units = new;
            }
        }
        self.realized += realized;
        realized
    }
}

/// Spot (underlying) inventory of one asset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnderlyingBalance {
    pub units: f64,
    /// Settlement per unit.
    pub mark: f64,
}

impl UnderlyingBalance {
    pub fn value(&self) -> f64 {
        self.units * self.mark
    }
}

/// Queued withdrawal shares and what they are owed at the last
/// observation (the settlement-value liability).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedWithdrawals {
    pub shares: f64,
    /// `None` when the queue could not be valued (missing pps): the
    /// liability is then carried at the last valued figure.
    pub value: Option<f64>,
    pub observed_at_ms: u64,
}

/// The vault's external (hedge-venue) account as the chain attests it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalAccount {
    /// Settlement released to the venue and not yet returned.
    pub exposure: f64,
    /// Attested equity of the account (`None` = never attested).
    pub attested_equity: Option<f64>,
    pub attested_at_ms: Option<u64>,
    /// Total release budget (settlement) and the 24 h release window.
    pub total_budget: f64,
    pub daily_release_limit: f64,
    pub daily_release_used: f64,
    pub window_start_ms: u64,
    pub observed_at_ms: u64,
}

impl ExternalAccount {
    pub fn budget_remaining(&self) -> f64 {
        (self.total_budget - self.exposure).max(0.0)
    }
    pub fn daily_release_remaining(&self) -> f64 {
        (self.daily_release_limit - self.daily_release_used).max(0.0)
    }
    /// Age of the attested equity at `now_ms`, `None` when never attested.
    pub fn equity_age_ms(&self, now_ms: u64) -> Option<u64> {
        self.attested_at_ms.map(|t| now_ms.saturating_sub(t))
    }
}

// ── pending operations ─────────────────────────────────────────────────

/// Which atomic exercise PTB a pending exercise runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExercisePath {
    /// `bucket::exercise` funded from settlement cash; the underlying
    /// stays in the vault.
    CallCash,
    /// Flash-borrow the strike, exercise, sell the underlying, repay.
    CallFlash,
    /// Deliver the vault's own underlying against the strike.
    PutVaultUnderlying,
    /// Flash-borrow the underlying, exercise, buy it back, repay.
    PutBaseFlash,
    /// Flash-borrow settlement, buy the underlying, exercise, repay.
    PutQuoteFlash,
}

/// The exact asset movements one exercise PTB makes when it lands. A
/// flash path borrows `flash_borrowed` inside the PTB and must repay
/// exactly `flash_repaid ≥ flash_borrowed`; the ledger applies the whole
/// plan atomically or not at all.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExercisePlan {
    pub option: OptionId,
    pub path: ExercisePath,
    /// Option units exercised (removed exactly).
    pub qty: f64,
    pub asset: String,
    /// Settlement leaving the vault (strike cost, route cost).
    pub settlement_out: f64,
    /// Settlement arriving (strike payout, sale proceeds).
    pub settlement_in: f64,
    /// Underlying units received / delivered.
    pub underlying_in: f64,
    pub underlying_out: f64,
    /// Settlement value borrowed and repaid inside the PTB.
    pub flash_borrowed: f64,
    pub flash_repaid: f64,
    /// Spot route notional (acquisition or sale) — the turnover line.
    pub route_notional: f64,
    pub gas: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingHedge {
    pub market: Market,
    pub size_units: f64,
    pub spot: f64,
    pub submitted_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingMargin {
    pub market: Market,
    /// Settlement already debited, credited to the venue when it lands.
    pub amount: f64,
    pub sent_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingResale {
    pub option: OptionId,
    pub qty: f64,
    pub expected_proceeds: f64,
    pub submitted_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingExercise {
    pub plan: ExercisePlan,
    pub submitted_ms: u64,
}

/// Everything submitted and not yet resolved.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pending {
    /// Signed quotes out with the taker, not yet accepted / closed.
    pub quotes: BTreeSet<String>,
    pub hedges: BTreeMap<OpId, PendingHedge>,
    pub margin: BTreeMap<OpId, PendingMargin>,
    pub resales: BTreeMap<OpId, PendingResale>,
    pub exercises: BTreeMap<OpId, PendingExercise>,
}

impl Pending {
    /// Margin debited from settlement and not yet at the venue: an asset
    /// in transit.
    pub fn margin_in_transit(&self) -> f64 {
        self.margin.values().map(|m| m.amount).sum()
    }
    /// Settlement the pending exercises will spend when they land.
    pub fn committed_spend(&self) -> f64 {
        self.exercises.values().map(|x| x.plan.settlement_out + x.plan.gas).sum()
    }
    pub fn len(&self) -> usize {
        self.quotes.len() + self.hedges.len() + self.margin.len() + self.resales.len() + self.exercises.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ── lines ──────────────────────────────────────────────────────────────

/// Cumulative accounting lines. Money lines are signed NAV effects
/// (a cost is negative) except the `*_paid` / fee / gas / penalty
/// figures, which are the classic positive-cost sums the reports print.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lines {
    /// Gross premium paid for options (positive).
    pub premium_paid: f64,
    /// Entry edge: mark at fill − premium paid (signed NAV effect).
    pub spread: f64,
    /// Cumulative change of option marks (signed; expiry to zero included).
    pub option_mark: f64,
    /// Settlement realized by exercise / resale, net of route costs and
    /// the option mark given up (signed NAV effect).
    pub option_exit: f64,
    /// Cash realized at exercise / resale, net of exercise costs
    /// (positive; the v0 `option_payoff` report line).
    pub option_payoff: f64,
    /// Route / strike costs paid at exercise (positive).
    pub exercise_costs: f64,
    /// Perp realized P&L (signed).
    pub hedge_realized: f64,
    /// Funding paid on the signed position (positive = paid).
    pub funding_paid: f64,
    /// Taker fees, maker fees (positive).
    pub hedge_fees: f64,
    pub maker_fees: f64,
    /// Signed distance from the reference mark × |size| (memo line: it
    /// is inside `hedge_realized` / unrealized, never a balance move).
    pub hedge_slippage: f64,
    pub gas: f64,
    /// Margin forfeited to liquidations / partial-liquidation penalties.
    pub liquidation_loss: f64,
    /// Other penalties (protocol / venue), positive.
    pub penalties: f64,
    pub hedge_turnover_notional: f64,
    pub exercise_turnover_notional: f64,
    pub topup_total: f64,
    // counts
    pub fills: u64,
    pub hedge_fills: u64,
    pub taker_fills: u64,
    pub passive_fills: u64,
    pub partial_fills: u64,
    pub cancels: u64,
    pub hedge_rejects: u64,
    pub liquidations: u64,
    pub margin_topups: u64,
    pub topup_rejects: u64,
    pub exercises: u64,
    pub exercise_failures: u64,
    pub resales: u64,
    pub expired_worthless: u64,
}

impl Lines {
    /// The realized lines' signed NAV effect.
    pub fn realized(&self) -> f64 {
        self.spread + self.option_exit + self.hedge_realized - self.funding_paid - self.hedge_fees - self.maker_fees
            - self.gas
            - self.liquidation_loss
            - self.penalties
    }
}

/// Equity flows: NAV changes that are not P&L — what the chain / venue
/// said on re-sync that the ledger had not predicted (deposits,
/// withdrawals, unmodeled fees) and the withdrawal-queue liability.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EquityFlows {
    pub resync_settlement: f64,
    pub resync_underlying: f64,
    pub resync_options: f64,
    pub resync_perp: f64,
    /// −Δ(queued withdrawal liability).
    pub withdrawal_queue: f64,
}

impl EquityFlows {
    pub fn total(&self) -> f64 {
        self.resync_settlement + self.resync_underlying + self.resync_options + self.resync_perp + self.withdrawal_queue
    }
    /// Everything the ledger could not explain from its own events.
    pub fn residual(&self) -> f64 {
        self.resync_settlement + self.resync_underlying + self.resync_options + self.resync_perp
    }
}

// ── events ─────────────────────────────────────────────────────────────

/// One re-synced option line (custody truth).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptionSync {
    pub option: OptionId,
    pub spec: OptionSpec,
    pub qty: f64,
    /// Mark to carry the line at (`None` keeps the last mark).
    pub mark_per_unit: Option<f64>,
}

/// What the ledger applies. Every variant carries the instant it is
/// actionable at.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum LedgerEvent {
    // ── observations (chain / venue truth) ──
    /// Settlement and underlying balances as observed; the difference to
    /// the ledger's expectation is an equity flow.
    ResyncBalances {
        settlement: Option<f64>,
        /// asset → (units, mark).
        underlying: Vec<(String, f64, f64)>,
        at_ms: u64,
    },
    /// Option custody as observed: lines not listed are gone; pending
    /// exercises / resales on the listed lines are superseded by truth.
    ResyncOptions { positions: Vec<OptionSync>, at_ms: u64 },
    /// Venue truth for one perp: the signed units (and the mark when the
    /// readback carries one). Realized P&L is the ledger's own
    /// average-entry figure over the fills it applied; the venue's is
    /// compared, never copied (no double count).
    ResyncPerp { market: Market, units: f64, mark: Option<f64>, at_ms: u64 },
    QueuedWithdrawals(QueuedWithdrawals),
    External(ExternalAccount),
    // ── marks ──
    MarkOptions { marks: Vec<(OptionId, f64)>, at_ms: u64 },
    MarkUnderlying { asset: String, mark: f64, at_ms: u64 },
    MarkPerp { market: Market, mark: f64, at_ms: u64 },
    /// Every line past expiry is worthless (`burn_expired_option`).
    ExpireOptions { at_ms: u64 },
    // ── reservations ──
    Reserve(Reservation),
    ReservationTransition { key: String, state: ReservationState, at_ms: u64 },
    /// Boot: re-install a live reservation without the capacity check.
    RestoreReservation(Reservation),
    // ── options ──
    OptionBought {
        option: OptionId,
        /// `None` when the bucket's economics are not yet known (a fill
        /// detected before its custody sync); the sync fills them in.
        spec: Option<OptionSpec>,
        qty: f64,
        premium: f64,
        mark_per_unit: f64,
        at_ms: u64,
    },
    ResaleSubmitted { op: OpId, option: OptionId, qty: f64, expected_proceeds: f64, at_ms: u64 },
    /// `proceeds = None` = the resale did not happen.
    ResaleSettled { op: OpId, proceeds: Option<f64>, at_ms: u64 },
    ExerciseSubmitted { op: OpId, plan: ExercisePlan, at_ms: u64 },
    /// `ok = false`: the PTB aborted — nothing moves. `actual` overrides
    /// the submitted plan's movements when the landed outcome is known.
    ExerciseSettled { op: OpId, ok: bool, actual: Option<ExercisePlan>, at_ms: u64 },
    // ── perps ──
    HedgeSubmitted { op: OpId, market: Market, size_units: f64, spot: f64, at_ms: u64 },
    /// A fill (partial or full). `reference` is the mark at execution
    /// (slippage attribution); `passive` marks a maker fill.
    PerpFill { op: Option<OpId>, market: Market, size_units: f64, price: f64, fee: f64, reference: f64, gas: f64, passive: bool, partial: bool, at_ms: u64 },
    /// The order left the venue without (further) fills.
    HedgeResolved { op: OpId, rejected: bool, at_ms: u64 },
    Funding { market: Market, paid: f64, at_ms: u64 },
    /// Settlement → collateral now (`amount < 0` releases).
    MarginMoved { market: Market, amount: f64, at_ms: u64 },
    MarginTopUpSent { op: OpId, market: Market, amount: f64, at_ms: u64 },
    MarginTopUpLanded { op: OpId, accepted: bool, at_ms: u64 },
    Liquidation { market: Market, size_closed: f64, price: f64, penalty: f64, full: bool, at_ms: u64 },
    // ── misc ──
    Gas { amount: f64, at_ms: u64 },
    Penalty { amount: f64, at_ms: u64 },
}

impl LedgerEvent {
    pub fn at_ms(&self) -> u64 {
        match self {
            LedgerEvent::ResyncBalances { at_ms, .. }
            | LedgerEvent::ResyncOptions { at_ms, .. }
            | LedgerEvent::ResyncPerp { at_ms, .. }
            | LedgerEvent::MarkOptions { at_ms, .. }
            | LedgerEvent::MarkUnderlying { at_ms, .. }
            | LedgerEvent::MarkPerp { at_ms, .. }
            | LedgerEvent::ExpireOptions { at_ms }
            | LedgerEvent::ReservationTransition { at_ms, .. }
            | LedgerEvent::OptionBought { at_ms, .. }
            | LedgerEvent::ResaleSubmitted { at_ms, .. }
            | LedgerEvent::ResaleSettled { at_ms, .. }
            | LedgerEvent::ExerciseSubmitted { at_ms, .. }
            | LedgerEvent::ExerciseSettled { at_ms, .. }
            | LedgerEvent::HedgeSubmitted { at_ms, .. }
            | LedgerEvent::PerpFill { at_ms, .. }
            | LedgerEvent::HedgeResolved { at_ms, .. }
            | LedgerEvent::Funding { at_ms, .. }
            | LedgerEvent::MarginMoved { at_ms, .. }
            | LedgerEvent::MarginTopUpSent { at_ms, .. }
            | LedgerEvent::MarginTopUpLanded { at_ms, .. }
            | LedgerEvent::Liquidation { at_ms, .. }
            | LedgerEvent::Gas { at_ms, .. }
            | LedgerEvent::Penalty { at_ms, .. } => *at_ms,
            LedgerEvent::QueuedWithdrawals(q) => q.observed_at_ms,
            LedgerEvent::External(e) => e.observed_at_ms,
            LedgerEvent::Reserve(r) | LedgerEvent::RestoreReservation(r) => r.state_at_ms,
        }
    }
}

/// Why an event was refused (the ledger is unchanged).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum LedgerError {
    /// A live reservation already holds this key.
    DuplicateReservation(String),
    /// reservations + committed spend + amount would exceed available capital.
    ExceedsAvailableCapital { requested: f64, available: f64 },
    UnknownOption(OptionId),
    UnknownOp(OpId),
    /// More units than the line holds free.
    InsufficientUnits { option: OptionId, requested: f64, free: f64 },
    /// The PTB would not repay its flash loan: aborted, nothing moved.
    FlashNotRepaid { borrowed: f64, repaid: f64 },
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LedgerError::DuplicateReservation(k) => write!(f, "duplicate reservation key {k}"),
            LedgerError::ExceedsAvailableCapital { requested, available } => {
                write!(f, "reservation {requested} exceeds available capital {available}")
            }
            LedgerError::UnknownOption(o) => write!(f, "unknown option {o}"),
            LedgerError::UnknownOp(o) => write!(f, "unknown pending op {o}"),
            LedgerError::InsufficientUnits { option, requested, free } => {
                write!(f, "{option}: {requested} units requested, {free} free")
            }
            LedgerError::FlashNotRepaid { borrowed, repaid } => {
                write!(f, "flash loan not repaid: borrowed {borrowed}, repaid {repaid}")
            }
        }
    }
}

impl std::error::Error for LedgerError {}

/// One violated invariant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Violation {
    pub invariant: &'static str,
    pub detail: String,
}

/// Premium usage over reservations + holdings, the way the capital
/// policy counts it: each reservation and each marked line lands once in
/// the total, once on its side, once at its expiry.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PremiumUsage {
    pub call: f64,
    pub put: f64,
    pub total: f64,
    pub by_expiry: BTreeMap<u64, f64>,
    pub reserved_call: f64,
    pub reserved_put: f64,
    pub reserved_total: f64,
    pub marked_call: f64,
    pub marked_put: f64,
}

// ── the ledger ─────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ledger {
    /// NAV the ledger opened at (all of it settlement until re-synced).
    pub nav0: f64,
    pub settlement: f64,
    pub underlying: BTreeMap<String, UnderlyingBalance>,
    pub options: BTreeMap<OptionId, OptionPosition>,
    pub perps: BTreeMap<Market, PerpPosition>,
    /// LIVE reservations by request id.
    pub reservations: BTreeMap<String, Reservation>,
    pub queued_withdrawals: QueuedWithdrawals,
    pub external: ExternalAccount,
    pub pending: Pending,
    /// Flash value borrowed inside the PTB being applied; zero between
    /// events by construction.
    pub flash_outstanding: f64,
    pub lines: Lines,
    pub equity_flows: EquityFlows,
    pub events_applied: u64,
    pub last_event_ms: u64,
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() <= TOLERANCE * a.abs().max(b.abs()).max(1.0)
}

impl Ledger {
    pub fn new(nav0: f64) -> Self {
        Self {
            nav0,
            settlement: nav0,
            underlying: BTreeMap::new(),
            options: BTreeMap::new(),
            perps: BTreeMap::new(),
            reservations: BTreeMap::new(),
            queued_withdrawals: QueuedWithdrawals::default(),
            external: ExternalAccount::default(),
            pending: Pending::default(),
            flash_outstanding: 0.0,
            lines: Lines::default(),
            equity_flows: EquityFlows::default(),
            events_applied: 0,
            last_event_ms: 0,
        }
    }

    // ── views ─────────────────────────────────────────────────────────

    pub fn option_marks(&self) -> f64 {
        self.options.values().map(OptionPosition::value).sum()
    }

    pub fn underlying_value(&self) -> f64 {
        self.underlying.values().map(UnderlyingBalance::value).sum()
    }

    pub fn perp_collateral(&self) -> f64 {
        self.perps.values().map(|p| p.collateral).sum()
    }

    pub fn perp_unrealized(&self) -> f64 {
        self.perps.values().map(PerpPosition::unrealized).sum()
    }

    pub fn perp_realized(&self) -> f64 {
        self.perps.values().map(|p| p.realized).sum()
    }

    /// Queued withdrawals at their last valuation.
    pub fn withdrawal_liability(&self) -> f64 {
        self.queued_withdrawals.value.unwrap_or(0.0)
    }

    pub fn assets(&self) -> f64 {
        self.settlement
            + self.underlying_value()
            + self.option_marks()
            + self.perp_collateral()
            + self.pending.margin_in_transit()
            + self.perp_unrealized()
    }

    pub fn liabilities(&self) -> f64 {
        self.withdrawal_liability() + self.flash_outstanding
    }

    /// NAV = assets − liabilities (doc 08 §2.3).
    pub fn nav(&self) -> f64 {
        self.assets() - self.liabilities()
    }

    /// NAV explained by the ledger's own postings: `nav0 + realized
    /// lines + option mark change + perp unrealized + equity flows`.
    pub fn nav_explained(&self) -> f64 {
        self.nav0 + self.lines.realized() + self.lines.option_mark + self.perp_unrealized() + self.equity_flows.total()
    }

    pub fn reserved_total(&self) -> f64 {
        self.reservations.values().map(|r| r.amount as f64).sum()
    }

    /// Settlement the desk can still commit: free settlement less the
    /// live reservations and the spend pending exercises will make.
    pub fn available_capital(&self) -> f64 {
        self.settlement - self.reserved_total() - self.pending.committed_spend()
    }

    /// Premium usage (marked holdings + live reservations), by side and
    /// expiry.
    pub fn premium_usage(&self) -> PremiumUsage {
        let mut u = PremiumUsage::default();
        for r in self.reservations.values() {
            let a = r.amount as f64;
            if r.is_put {
                u.reserved_put += a;
            } else {
                u.reserved_call += a;
            }
            u.reserved_total += a;
            *u.by_expiry.entry(r.expiry_ms).or_default() += a;
        }
        for p in self.options.values() {
            let v = p.value();
            if p.spec.kind.is_put() {
                u.marked_put += v;
            } else {
                u.marked_call += v;
            }
            *u.by_expiry.entry(p.spec.expiry_ms).or_default() += v;
        }
        u.call = u.reserved_call + u.marked_call;
        u.put = u.reserved_put + u.marked_put;
        u.total = u.reserved_total + u.marked_call + u.marked_put;
        u
    }

    /// The pending exercise for `option` of exactly `qty` units, oldest
    /// first (the live result event carries no op id).
    pub fn find_pending_exercise(&self, option: &str, qty: f64) -> Option<OpId> {
        self.pending
            .exercises
            .iter()
            .find(|(_, x)| x.plan.option == option && near(x.plan.qty, qty))
            .map(|(op, _)| *op)
    }

    // ── invariants ────────────────────────────────────────────────────

    /// Every violated invariant (empty = reconciled): the accounting
    /// identities of [`Ledger::check_accounting`] plus the capital rule
    /// (`reservations + committed spend ≤ available capital`), which a
    /// cash outflow AFTER a reservation can breach — reported, never
    /// asserted, because it is the desk's signal to stop quoting, not an
    /// accounting error.
    pub fn check(&self) -> Vec<Violation> {
        let mut out = self.check_accounting();
        let reserved = self.reserved_total();
        let committed = self.pending.committed_spend();
        if reserved + committed > self.settlement + TOLERANCE * self.settlement.abs().max(1.0) {
            out.push(Violation {
                invariant: "reservations + committed spend ≤ available capital",
                detail: format!("reserved {reserved} + committed {committed} > settlement {}", self.settlement),
            });
        }
        out
    }

    /// The accounting identities every event must preserve (debug builds
    /// assert these after each `apply`).
    pub fn check_accounting(&self) -> Vec<Violation> {
        let mut out = Vec::new();
        let (nav, explained) = (self.nav(), self.nav_explained());
        if !near(nav, explained) {
            out.push(Violation {
                invariant: "assets − liabilities = NAV",
                detail: format!("assets − liabilities = {nav}, explained = {explained}"),
            });
        }
        let u = self.premium_usage();
        let by_expiry: f64 = u.by_expiry.values().sum();
        if !near(u.call + u.put, u.total) || !near(by_expiry, u.total) {
            out.push(Violation {
                invariant: "call + put = total = Σ expiry premium usage",
                detail: format!("call {} + put {} vs total {} vs Σexpiry {by_expiry}", u.call, u.put, u.total),
            });
        }
        if self.flash_outstanding != 0.0 {
            out.push(Violation {
                invariant: "flash liabilities are zero between events",
                detail: format!("outstanding {}", self.flash_outstanding),
            });
        }
        for (id, p) in &self.options {
            if p.qty < -TOLERANCE || p.pending_units < -TOLERANCE || p.pending_units > p.qty + TOLERANCE * p.qty.abs().max(1.0) {
                out.push(Violation {
                    invariant: "option quantities are non-negative and cover their pending units",
                    detail: format!("{id}: qty {} pending {}", p.qty, p.pending_units),
                });
            }
            if p.cost_basis < -TOLERANCE * p.cost_basis.abs().max(1.0) {
                out.push(Violation { invariant: "option cost basis is non-negative", detail: format!("{id}: {}", p.cost_basis) });
            }
        }
        for (m, p) in &self.perps {
            let lhs = p.realized + p.unrealized();
            let rhs = p.cash_flow + p.units * p.mark;
            if !near(lhs, rhs) {
                out.push(Violation {
                    invariant: "perp realized + unrealized = traded cash flow + units × mark",
                    detail: format!("{m}: realized {} + unrealized {} vs {rhs}", p.realized, p.unrealized()),
                });
            }
            if p.collateral < -TOLERANCE * p.collateral.abs().max(1.0) {
                out.push(Violation { invariant: "perp collateral is non-negative", detail: format!("{m}: {}", p.collateral) });
            }
        }
        for (op, x) in &self.pending.exercises {
            match self.options.get(&x.plan.option) {
                Some(p) if p.pending_units + TOLERANCE * p.qty.abs().max(1.0) >= x.plan.qty => {}
                _ => out.push(Violation {
                    invariant: "pending exercises are backed by their option line",
                    detail: format!("op {op}: {} × {}", x.plan.option, x.plan.qty),
                }),
            }
        }
        out
    }

    /// `check()` as a result.
    pub fn verify(&self) -> Result<(), Vec<Violation>> {
        let v = self.check();
        if v.is_empty() {
            Ok(())
        } else {
            Err(v)
        }
    }

    // ── apply ─────────────────────────────────────────────────────────

    /// Apply one event. On `Err` no balance has moved (an
    /// `ExerciseSettled` whose flash loan is not repaid is recorded as a
    /// failed PTB — the op is closed and counted — and returns
    /// [`LedgerError::FlashNotRepaid`]). In debug builds every accepted
    /// event is followed by the accounting check.
    pub fn apply(&mut self, ev: &LedgerEvent) -> Result<(), LedgerError> {
        let at = ev.at_ms();
        self.apply_inner(ev)?;
        self.events_applied += 1;
        self.last_event_ms = self.last_event_ms.max(at);
        debug_assert!(
            self.check_accounting().is_empty(),
            "ledger invariants violated after {ev:?}: {:?}",
            self.check_accounting()
        );
        Ok(())
    }

    fn drop_if_empty(&mut self, option: &str) {
        if self.options.get(option).is_some_and(|p| p.qty <= 1e-12 && p.pending_units <= 1e-12) {
            self.options.remove(option);
        }
    }

    fn apply_inner(&mut self, ev: &LedgerEvent) -> Result<(), LedgerError> {
        match ev {
            LedgerEvent::ResyncBalances { settlement, underlying, .. } => {
                if let Some(s) = settlement {
                    self.equity_flows.resync_settlement += s - self.settlement;
                    self.settlement = *s;
                }
                for (asset, units, mark) in underlying {
                    let b = self.underlying.entry(asset.clone()).or_default();
                    let before = b.value();
                    b.units = *units;
                    b.mark = *mark;
                    self.equity_flows.resync_underlying += b.value() - before;
                }
            }
            LedgerEvent::ResyncOptions { positions, .. } => {
                let mut seen = BTreeSet::new();
                for s in positions {
                    seen.insert(s.option.clone());
                    let p = self.options.entry(s.option.clone()).or_insert(OptionPosition {
                        spec: s.spec,
                        qty: 0.0,
                        cost_basis: 0.0,
                        mark_per_unit: s.mark_per_unit.unwrap_or(0.0),
                        pending_units: 0.0,
                    });
                    p.spec = s.spec;
                    if let Some(m) = s.mark_per_unit {
                        // A mark change is P&L, not a flow.
                        self.lines.option_mark += (m - p.mark_per_unit) * p.qty;
                        p.mark_per_unit = m;
                    }
                    let before = p.value();
                    if p.qty > 0.0 {
                        p.cost_basis *= s.qty / p.qty;
                    } else {
                        p.cost_basis = 0.0;
                    }
                    p.qty = s.qty;
                    p.pending_units = 0.0;
                    self.equity_flows.resync_options += p.value() - before;
                }
                let gone: Vec<OptionId> = self.options.keys().filter(|k| !seen.contains(*k)).cloned().collect();
                for k in gone {
                    let p = self.options.remove(&k).expect("present");
                    self.equity_flows.resync_options -= p.value();
                }
                // Custody truth supersedes whatever was in flight on the
                // synced lines.
                self.pending.exercises.clear();
                self.pending.resales.clear();
            }
            LedgerEvent::ResyncPerp { market, units, mark, .. } => {
                let p = self.perps.entry(market.clone()).or_default();
                if let Some(m) = mark {
                    p.mark = *m;
                }
                if !near(p.units, *units) {
                    // Units the ledger never saw filled (or saw filled and
                    // the venue did not): carried in at the mark, the
                    // closed side realized like any fill.
                    let px = if p.mark > 0.0 { p.mark } else { p.entry };
                    let realized = p.fill(*units - p.units, px);
                    self.settlement += realized;
                    self.lines.hedge_realized += realized;
                }
            }
            LedgerEvent::QueuedWithdrawals(q) => {
                let before = self.withdrawal_liability();
                let value = q.value.or(self.queued_withdrawals.value);
                self.queued_withdrawals = QueuedWithdrawals { value, ..*q };
                self.equity_flows.withdrawal_queue -= self.withdrawal_liability() - before;
            }
            LedgerEvent::External(e) => self.external = *e,
            LedgerEvent::MarkOptions { marks, .. } => {
                for (id, m) in marks {
                    if let Some(p) = self.options.get_mut(id) {
                        self.lines.option_mark += (m - p.mark_per_unit) * p.qty;
                        p.mark_per_unit = *m;
                    }
                }
            }
            LedgerEvent::MarkUnderlying { asset, mark, .. } => {
                if let Some(b) = self.underlying.get_mut(asset) {
                    // Spot inventory is marked through the same line as
                    // the options it backs (unrealized).
                    self.lines.option_mark += (mark - b.mark) * b.units;
                    b.mark = *mark;
                }
            }
            LedgerEvent::MarkPerp { market, mark, .. } => {
                let p = self.perps.entry(market.clone()).or_default();
                p.mark = *mark;
            }
            LedgerEvent::ExpireOptions { at_ms } => {
                let expired: Vec<OptionId> =
                    self.options.iter().filter(|(_, p)| p.spec.expiry_ms <= *at_ms).map(|(k, _)| k.clone()).collect();
                for k in expired {
                    let p = self.options.get_mut(&k).expect("present");
                    self.lines.option_mark -= p.value();
                    if p.free_units() > 1e-12 {
                        self.lines.expired_worthless += 1;
                    }
                    // A PTB included before expiry may still be awaiting
                    // detection: its units stay as a worthless shell the
                    // settlement closes; everything else is gone.
                    p.mark_per_unit = 0.0;
                    p.cost_basis = 0.0;
                    p.qty = p.pending_units;
                    if p.qty <= 1e-12 {
                        self.options.remove(&k);
                    }
                }
            }
            LedgerEvent::Reserve(r) => {
                if self.reservations.contains_key(&r.key) {
                    return Err(LedgerError::DuplicateReservation(r.key.clone()));
                }
                let available = self.available_capital();
                if r.amount as f64 > available + TOLERANCE * available.abs().max(1.0) {
                    return Err(LedgerError::ExceedsAvailableCapital { requested: r.amount as f64, available });
                }
                if r.state.is_live() {
                    self.reservations.insert(r.key.clone(), r.clone());
                    if r.state == ReservationState::Quoted {
                        self.pending.quotes.insert(r.key.clone());
                    }
                }
            }
            LedgerEvent::RestoreReservation(r) => {
                if r.state.is_live() {
                    self.reservations.insert(r.key.clone(), r.clone());
                    if r.state == ReservationState::Quoted {
                        self.pending.quotes.insert(r.key.clone());
                    }
                }
            }
            LedgerEvent::ReservationTransition { key, state, at_ms } => {
                if let Some(mut r) = self.reservations.remove(key) {
                    r.state = *state;
                    r.state_at_ms = *at_ms;
                    self.pending.quotes.remove(key);
                    if state.is_live() {
                        self.reservations.insert(key.clone(), r);
                    }
                }
            }
            LedgerEvent::OptionBought { option, spec, qty, premium, mark_per_unit, .. } => {
                let p = self.options.entry(option.clone()).or_insert(OptionPosition {
                    spec: spec.unwrap_or(OptionSpec { kind: OptionKind::Call, strike: 0.0, expiry_ms: u64::MAX }),
                    qty: 0.0,
                    cost_basis: 0.0,
                    mark_per_unit: *mark_per_unit,
                    pending_units: 0.0,
                });
                if let Some(s) = spec {
                    p.spec = *s;
                }
                // Existing units re-mark to the fill's mark (P&L), then
                // the bought units enter at it (entry edge).
                self.lines.option_mark += (mark_per_unit - p.mark_per_unit) * p.qty;
                p.mark_per_unit = *mark_per_unit;
                p.qty += qty;
                p.cost_basis += premium;
                self.settlement -= premium;
                self.lines.premium_paid += premium;
                self.lines.spread += mark_per_unit * qty - premium;
                self.lines.fills += 1;
            }
            LedgerEvent::ResaleSubmitted { op, option, qty, expected_proceeds, at_ms } => {
                let p = self.options.get_mut(option).ok_or_else(|| LedgerError::UnknownOption(option.clone()))?;
                let free = p.free_units();
                if *qty > free + TOLERANCE * free.abs().max(1.0) {
                    return Err(LedgerError::InsufficientUnits { option: option.clone(), requested: *qty, free });
                }
                p.pending_units += qty;
                self.pending.resales.insert(
                    *op,
                    PendingResale { option: option.clone(), qty: *qty, expected_proceeds: *expected_proceeds, submitted_ms: *at_ms },
                );
            }
            LedgerEvent::ResaleSettled { op, proceeds, .. } => {
                let r = self.pending.resales.remove(op).ok_or(LedgerError::UnknownOp(*op))?;
                let p = self.options.get_mut(&r.option).ok_or_else(|| LedgerError::UnknownOption(r.option.clone()))?;
                p.pending_units -= r.qty;
                if let Some(proceeds) = proceeds {
                    let given_up = p.mark_per_unit * r.qty;
                    p.cost_basis -= if p.qty > 0.0 { p.cost_basis * r.qty / p.qty } else { 0.0 };
                    p.qty -= r.qty;
                    self.settlement += proceeds;
                    self.lines.option_exit += proceeds - given_up;
                    self.lines.option_payoff += proceeds;
                    self.lines.resales += 1;
                    self.drop_if_empty(&r.option);
                }
            }
            LedgerEvent::ExerciseSubmitted { op, plan, at_ms } => {
                let p = self.options.get_mut(&plan.option).ok_or_else(|| LedgerError::UnknownOption(plan.option.clone()))?;
                let free = p.free_units();
                if plan.qty > free + TOLERANCE * free.abs().max(1.0) {
                    return Err(LedgerError::InsufficientUnits { option: plan.option.clone(), requested: plan.qty, free });
                }
                p.pending_units += plan.qty;
                self.pending.exercises.insert(*op, PendingExercise { plan: plan.clone(), submitted_ms: *at_ms });
            }
            LedgerEvent::ExerciseSettled { op, ok, actual, .. } => {
                let x = self.pending.exercises.remove(op).ok_or(LedgerError::UnknownOp(*op))?;
                let plan = actual.as_ref().unwrap_or(&x.plan);
                if let Some(p) = self.options.get_mut(&x.plan.option) {
                    p.pending_units = (p.pending_units - x.plan.qty).max(0.0);
                }
                if !*ok {
                    // Atomic abort: nothing moved.
                    self.lines.exercise_failures += 1;
                    self.drop_if_empty(&x.plan.option);
                    return Ok(());
                }
                // The PTB runs on a trial copy: borrow, move, repay; it
                // lands only if the flash loan is exactly repaid.
                let mut trial = self.clone();
                trial.flash_outstanding += plan.flash_borrowed;
                {
                    let p = trial.options.get_mut(&plan.option).ok_or_else(|| LedgerError::UnknownOption(plan.option.clone()))?;
                    if plan.qty > p.qty + TOLERANCE * p.qty.abs().max(1.0) {
                        return Err(LedgerError::InsufficientUnits { option: plan.option.clone(), requested: plan.qty, free: p.qty });
                    }
                    let given_up = p.mark_per_unit * plan.qty;
                    p.cost_basis -= if p.qty > 0.0 { p.cost_basis * plan.qty / p.qty } else { 0.0 };
                    p.qty -= plan.qty;
                    trial.lines.option_exit -= given_up;
                }
                trial.settlement += plan.settlement_in - plan.settlement_out - plan.gas;
                let mut underlying_delta_value = 0.0;
                if plan.underlying_in != 0.0 || plan.underlying_out != 0.0 {
                    let b = trial.underlying.entry(plan.asset.clone()).or_default();
                    let before = b.value();
                    b.units += plan.underlying_in - plan.underlying_out;
                    underlying_delta_value = b.value() - before;
                }
                trial.flash_outstanding -= plan.flash_repaid;
                if trial.flash_outstanding.abs() > TOLERANCE * plan.flash_borrowed.abs().max(1.0) {
                    self.lines.exercise_failures += 1;
                    self.drop_if_empty(&x.plan.option);
                    return Err(LedgerError::FlashNotRepaid { borrowed: plan.flash_borrowed, repaid: plan.flash_repaid });
                }
                trial.flash_outstanding = 0.0;
                trial.lines.option_exit += plan.settlement_in - plan.settlement_out + underlying_delta_value;
                trial.lines.option_payoff += plan.settlement_in - plan.settlement_out + underlying_delta_value;
                trial.lines.exercise_costs += plan.settlement_out;
                trial.lines.gas += plan.gas;
                trial.lines.exercise_turnover_notional += plan.route_notional;
                trial.lines.exercises += 1;
                trial.drop_if_empty(&plan.option);
                *self = trial;
            }
            LedgerEvent::HedgeSubmitted { op, market, size_units, spot, at_ms } => {
                self.pending.hedges.insert(
                    *op,
                    PendingHedge { market: market.clone(), size_units: *size_units, spot: *spot, submitted_ms: *at_ms },
                );
            }
            LedgerEvent::PerpFill { op, market, size_units, price, fee, reference, gas, passive, partial, .. } => {
                let p = self.perps.entry(market.clone()).or_default();
                let realized = p.fill(*size_units, *price);
                let reference = if *reference > 0.0 { *reference } else { *price };
                p.mark = reference;
                p.fees += fee;
                // Realized P&L, the venue fee and gas settle to cash.
                self.settlement += realized - fee - gas;
                self.lines.hedge_realized += realized;
                if *passive {
                    self.lines.maker_fees += fee;
                    self.lines.passive_fills += 1;
                } else {
                    self.lines.hedge_fees += fee;
                    self.lines.taker_fills += 1;
                }
                if *partial {
                    self.lines.partial_fills += 1;
                }
                self.lines.gas += gas;
                self.lines.hedge_slippage += size_units.abs() * (price - reference) * size_units.signum();
                self.lines.hedge_turnover_notional += size_units.abs() * reference;
                self.lines.hedge_fills += 1;
                if let Some(op) = op {
                    if !*partial {
                        self.pending.hedges.remove(op);
                    }
                }
            }
            LedgerEvent::HedgeResolved { op, rejected, .. } => {
                if self.pending.hedges.remove(op).is_some() {
                    if *rejected {
                        self.lines.hedge_rejects += 1;
                    } else {
                        self.lines.cancels += 1;
                    }
                }
            }
            LedgerEvent::Funding { market, paid, .. } => {
                let p = self.perps.entry(market.clone()).or_default();
                p.funding_paid += paid;
                self.settlement -= paid;
                self.lines.funding_paid += paid;
            }
            LedgerEvent::MarginMoved { market, amount, .. } => {
                let p = self.perps.entry(market.clone()).or_default();
                p.collateral += amount;
                self.settlement -= amount;
            }
            LedgerEvent::MarginTopUpSent { op, market, amount, at_ms } => {
                self.settlement -= amount;
                self.pending.margin.insert(*op, PendingMargin { market: market.clone(), amount: *amount, sent_ms: *at_ms });
            }
            LedgerEvent::MarginTopUpLanded { op, accepted, .. } => {
                let m = self.pending.margin.remove(op).ok_or(LedgerError::UnknownOp(*op))?;
                if *accepted {
                    self.perps.entry(m.market).or_default().collateral += m.amount;
                    self.lines.margin_topups += 1;
                    self.lines.topup_total += m.amount;
                } else {
                    self.settlement += m.amount;
                    self.lines.topup_rejects += 1;
                }
            }
            LedgerEvent::Liquidation { market, size_closed, price, penalty, full, .. } => {
                let p = self.perps.entry(market.clone()).or_default();
                let realized = p.fill(*size_closed, *price);
                p.mark = *price;
                self.settlement += realized;
                self.lines.hedge_realized += realized;
                self.lines.hedge_turnover_notional += size_closed.abs() * price;
                p.collateral -= penalty;
                self.lines.liquidation_loss += penalty;
                self.lines.liquidations += 1;
                if *full {
                    self.settlement += p.collateral;
                    p.collateral = 0.0;
                }
                // The venue dropped every working order on this market.
                let m = market.clone();
                self.pending.hedges.retain(|_, h| h.market != m);
            }
            LedgerEvent::Gas { amount, .. } => {
                self.settlement -= amount;
                self.lines.gas += amount;
            }
            LedgerEvent::Penalty { amount, .. } => {
                self.settlement -= amount;
                self.lines.penalties += amount;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
