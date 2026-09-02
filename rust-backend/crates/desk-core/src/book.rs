//! The desk's book — single source of truth (00-plan Phase 2), pure.
//!
//! Tracks held option inventory (vault custody + wallet float), written
//! positions, NAV, the reservation ledger (`reservations + deployed ≤ NAV`
//! before every quote), and realized P&L attribution counters
//! (spread / scalp / theta / funding). Reconstruction from vault custody,
//! fill detection against the indexer feed and the metrics/JSONL P&L
//! sinks are `services/mm-bot`'s `desk::book` (chain readers and
//! recorders); this module never does I/O.
//!
//! Reservations (SO-444, doc 08 §4.6) are keyed by quote/request id with
//! explicit `quoted → accepted | reverted | expired | filled`
//! transitions. The map here holds the LIVE ones; every transition is
//! queued in an outbox the desk persists to the history DB, and boot
//! re-installs the still-live rows after reconciling them against chain
//! fills ([`reconcile_reservations`]). P&L records queue the same way
//! ([`Book::drain_pnl_records`]).

use std::collections::HashMap;

use protocol_types::ids::ObjectId;
use serde::{Deserialize, Serialize};

use crate::model::Greeks;

/// One VaultMm coin-custody position: option coins stored AS a vault
/// position (`receive_mm_option_coin` sweeps). These exit via the
/// curator-session entries (`exercise_*_coin`, `close_offset_*`,
/// `release_coin_to_balances`), all keyed by the position id.
#[derive(Clone, Debug)]
pub struct CoinPosition {
    pub position_id: ObjectId,
    pub amount: u64,
}

/// One held option line (long calls/puts bought from retail).
#[derive(Clone, Debug)]
pub struct Holding {
    pub bucket_id: ObjectId,
    /// The bucket's fungible option-coin type.
    pub option_coin_type: String,
    pub asset_coin_type: String,
    pub settlement_coin_type: String,
    pub is_put: bool,
    pub strike: u128,
    pub strike_scale: u8,
    pub expiry_ms: u64,
    /// Units held in the VAULT's free balances (auction-win redemptions
    /// land here; exits sell them via the deepbook-adapter taker swap).
    pub amount_vault: u64,
    /// Units held in the bot wallet (auction winnings pending sweep, or
    /// coins staged for exit execution).
    pub amount_wallet: u64,
    /// Units custodied as VaultMm coin POSITIONS (writer-flow sweeps),
    /// per position object.
    pub coin_positions: Vec<CoinPosition>,
}

impl Holding {
    pub fn amount(&self) -> u64 {
        self.amount_vault
            .saturating_add(self.amount_wallet)
            .saturating_add(self.amount_coin_positions())
    }
    /// Units across the VaultMm coin-custody positions.
    pub fn amount_coin_positions(&self) -> u64 {
        self.coin_positions.iter().map(|c| c.amount).sum()
    }
    pub fn strike_scaled(&self) -> f64 {
        self.strike as f64 / 10f64.powi(self.strike_scale as i32)
    }
}

/// One written (short) option line — V2 trader flow.
#[derive(Clone, Debug)]
pub struct Written {
    pub bucket_id: ObjectId,
    /// The vault-custodied `Position` object id (offset-close target).
    pub position_id: ObjectId,
    /// Canonical underlying coin type (selects the market model).
    pub asset_coin_type: String,
    pub is_put: bool,
    pub strike: u128,
    pub strike_scale: u8,
    pub expiry_ms: u64,
    pub amount: u64,
    /// Of `amount`, how many units are covered by a held long in the same
    /// series (netting). `amount - covered` is naked short budget usage.
    pub covered: u64,
}

impl Written {
    pub fn naked(&self) -> u64 {
        self.amount.saturating_sub(self.covered)
    }
    pub fn strike_scaled(&self) -> f64 {
        self.strike as f64 / 10f64.powi(self.strike_scale as i32)
    }
}

/// Reservation lifecycle (doc 08 §4.6, SO-444): `quoted → accepted |
/// reverted | expired | filled`. `Quoted` and `Accepted` are LIVE (hold
/// capacity); the rest are terminal. A fill is ground truth and wins
/// over an expiry recorded earlier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationState {
    Quoted,
    Accepted,
    Reverted,
    Expired,
    Filled,
}

impl ReservationState {
    pub fn is_live(self) -> bool {
        matches!(self, ReservationState::Quoted | ReservationState::Accepted)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ReservationState::Quoted => "quoted",
            ReservationState::Accepted => "accepted",
            ReservationState::Reverted => "reverted",
            ReservationState::Expired => "expired",
            ReservationState::Filled => "filled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "quoted" => ReservationState::Quoted,
            "accepted" => ReservationState::Accepted,
            "reverted" => ReservationState::Reverted,
            "expired" => ReservationState::Expired,
            "filled" => ReservationState::Filled,
            _ => return None,
        })
    }
}

/// A premium reservation keyed by quote/request id, durable in the desk
/// history DB and mirrored here (SO-444). Amounts are settlement raw.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reservation {
    /// WS: the service request id. Legacy/auction: `legacy-<n>`.
    pub key: String,
    /// The signed quote nonce — the `(Put)WriteExecuted` join key.
    pub nonce: Option<u64>,
    /// Premium reserved.
    pub amount: u64,
    pub is_put: bool,
    /// Option expiry (per-expiry numerator).
    pub expiry_ms: u64,
    /// Strike cash (calls) / underlying value (puts) the fill would need
    /// at exercise, and the hedge notional it would need — the capacity
    /// numerators (`limits::ReservedSplit`).
    pub exercise_cash: f64,
    pub hedge_notional: f64,
    pub quoted_at_ms: u64,
    /// Reservation TTL: quote `valid_until` + fill-detection grace.
    pub expires_ms: u64,
    pub state: ReservationState,
    pub state_at_ms: u64,
}

/// Realized P&L attribution counters, settlement raw units.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Pnl {
    pub spread: f64,
    pub scalp: f64,
    pub theta: f64,
    pub funding: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PnlLine {
    Spread,
    Scalp,
    Theta,
    Funding,
}

/// One realized P&L attribution record — the JSONL row shape the desk
/// appends (`services/mm-bot` `desk::book::flush_pnl`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PnlRecord {
    pub ts_ms: u64,
    pub line: PnlLine,
    pub amount: f64,
    pub note: String,
}

/// Aggregated greeks for a set of positions, in book units:
/// delta/gamma in underlying raw units, vega/theta in settlement raw.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GreeksAgg {
    pub delta_units: f64,
    pub gamma_units: f64,
    /// Premium change per 1.0 of vol (divide by 100 for per vol pt).
    pub vega: f64,
    /// Premium change per calendar day (negative = decay cost) —
    /// `pricing::Greeks::theta` convention.
    pub theta_per_day: f64,
}

impl GreeksAgg {
    fn add(&mut self, g: &Greeks, amount: f64, sign: f64) {
        self.delta_units += sign * g.delta * amount;
        self.gamma_units += sign * g.gamma * amount;
        self.vega += sign * g.vega * amount;
        self.theta_per_day += sign * g.theta * amount;
    }
}

/// Why a reservation was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReserveError {
    /// reservations + deployed + amount would exceed NAV.
    ExceedsNav,
    /// A live reservation already holds this key.
    DuplicateKey,
}

/// Boot reconciliation of durable reservations against chain fills
/// (doc 08 §4.6): a live row whose nonce a detected fill carries is
/// `filled`; one past its TTL is `expired`; the rest are restored live.
/// Returns the reservations to restore and the transitions to persist.
pub fn reconcile_reservations(
    rows: Vec<Reservation>,
    filled_nonces: &std::collections::HashSet<u64>,
    now_ms: u64,
) -> (Vec<Reservation>, Vec<Reservation>) {
    let mut live = Vec::new();
    let mut transitions = Vec::new();
    for mut r in rows {
        if !r.state.is_live() {
            continue;
        }
        if r.nonce.is_some_and(|n| filled_nonces.contains(&n)) {
            r.state = ReservationState::Filled;
            r.state_at_ms = now_ms;
            transitions.push(r);
        } else if r.expires_ms <= now_ms {
            r.state = ReservationState::Expired;
            r.state_at_ms = now_ms;
            transitions.push(r);
        } else {
            live.push(r);
        }
    }
    (live, transitions)
}

/// The book. Wrapped in a lock by the desk; all methods are synchronous.
#[derive(Debug)]
pub struct Book {
    /// NAV in settlement raw units (see module docs for the source).
    pub nav: u64,
    /// Mark-to-model premium currently deployed in held options,
    /// settlement raw. Refreshed by the desk's book-refresh tick.
    pub deployed: u64,
    pub holdings: Vec<Holding>,
    pub written: Vec<Written>,
    /// LIVE reservations by key. Terminal transitions leave the map via
    /// the outbox ([`Book::drain_reservation_transitions`]).
    reservations: HashMap<String, Reservation>,
    /// Transitions not yet persisted (quoted rows included) — the desk
    /// drains and writes them to the history DB.
    reservation_outbox: Vec<Reservation>,
    /// Units per bucket committed to resting exchange asks (SO-416) —
    /// the listings engine writes; exits/quoting subtract so the same
    /// inventory is never double-committed.
    listed_units: HashMap<ObjectId, u64>,
    next_reservation_id: u64,
    pub pnl: Pnl,
    /// P&L records not yet sunk (metrics + JSONL) — the desk drains them
    /// right after each `record_pnl`; the kernel turns them into commands.
    pnl_outbox: Vec<PnlRecord>,
}

/// What a new keyed reservation carries (`Book::reserve_quote`).
#[derive(Clone, Debug)]
pub struct QuoteReservation {
    pub key: String,
    pub nonce: Option<u64>,
    pub amount: u64,
    pub is_put: bool,
    pub expiry_ms: u64,
    pub exercise_cash: f64,
    pub hedge_notional: f64,
    /// Reservation TTL from `now` (quote TTL + detection grace).
    pub ttl_ms: u64,
}

impl Book {
    pub fn new(nav: u64) -> Self {
        Self {
            nav,
            deployed: 0,
            holdings: Vec::new(),
            written: Vec::new(),
            reservations: HashMap::new(),
            reservation_outbox: Vec::new(),
            listed_units: HashMap::new(),
            next_reservation_id: 1,
            pnl: Pnl::default(),
            pnl_outbox: Vec::new(),
        }
    }

    // ── reservation ledger ────────────────────────────────────────────

    pub fn reserved_total(&self) -> u64 {
        self.reservations.values().map(|r| r.amount).sum()
    }

    /// Reserve premium for an outstanding signed quote under its
    /// request id (doc 08 §4.6). Enforces `reservations + deployed ≤
    /// NAV` and one live reservation per key.
    pub fn reserve_quote(&mut self, q: QuoteReservation, now_ms: u64) -> Result<(), ReserveError> {
        self.expire_reservations(now_ms);
        if self.reservations.contains_key(&q.key) {
            return Err(ReserveError::DuplicateKey);
        }
        let committed = self.reserved_total() as u128 + self.deployed as u128 + q.amount as u128;
        if committed > self.nav as u128 {
            return Err(ReserveError::ExceedsNav);
        }
        let r = Reservation {
            key: q.key,
            nonce: q.nonce,
            amount: q.amount,
            is_put: q.is_put,
            expiry_ms: q.expiry_ms,
            exercise_cash: q.exercise_cash,
            hedge_notional: q.hedge_notional,
            quoted_at_ms: now_ms,
            expires_ms: now_ms.saturating_add(q.ttl_ms),
            state: ReservationState::Quoted,
            state_at_ms: now_ms,
        };
        self.reservation_outbox.push(r.clone());
        self.reservations.insert(r.key.clone(), r);
        Ok(())
    }

    /// Legacy un-keyed reservation (the retired auction channel): the
    /// same ledger under a process-local `legacy-<n>` key; the returned
    /// id releases it via [`Book::release_reservation`].
    pub fn reserve(&mut self, amount: u64, ttl_ms: u64, now_ms: u64) -> Result<u64, ReserveError> {
        let id = self.next_reservation_id;
        self.reserve_quote(
            QuoteReservation {
                key: format!("legacy-{id}"),
                nonce: None,
                amount,
                is_put: false,
                expiry_ms: 0,
                exercise_cash: 0.0,
                hedge_notional: 0.0,
                ttl_ms,
            },
            now_ms,
        )?;
        self.next_reservation_id += 1;
        Ok(id)
    }

    /// Legacy release: the reservation reverts (the bid never became a
    /// fill the desk pays for).
    pub fn release_reservation(&mut self, id: u64, now_ms: u64) {
        self.transition(&format!("legacy-{id}"), ReservationState::Reverted, now_ms);
    }

    /// The taker took the quote (execution submitted, not yet observed
    /// on chain). Capacity stays held.
    pub fn accept_reservation(&mut self, key: &str, now_ms: u64) -> bool {
        self.transition(key, ReservationState::Accepted, now_ms)
    }

    /// The quote never reached the taker (sign/send failure) or its
    /// execution failed: free the capacity now.
    pub fn revert_reservation(&mut self, key: &str, now_ms: u64) -> bool {
        self.transition(key, ReservationState::Reverted, now_ms)
    }

    /// A chain fill carrying this quote nonce landed: the premium is now
    /// custody (it reaches `deployed` on the next custody re-sync).
    /// Returns the key it closed, if a live reservation carried the nonce.
    pub fn fill_reservation_by_nonce(&mut self, nonce: u64, now_ms: u64) -> Option<String> {
        let key = self
            .reservations
            .values()
            .find(|r| r.nonce == Some(nonce))
            .map(|r| r.key.clone())?;
        self.transition(&key, ReservationState::Filled, now_ms);
        Some(key)
    }

    fn transition(&mut self, key: &str, to: ReservationState, now_ms: u64) -> bool {
        let Some(mut r) = self.reservations.remove(key) else {
            return false;
        };
        r.state = to;
        r.state_at_ms = now_ms;
        if to.is_live() {
            self.reservations.insert(r.key.clone(), r.clone());
        }
        self.reservation_outbox.push(r);
        true
    }

    /// Re-install a still-live reservation from durable state at boot
    /// (no capacity check — it was already granted).
    pub fn restore_reservation(&mut self, r: Reservation) {
        if r.state.is_live() {
            self.reservations.insert(r.key.clone(), r);
        }
    }

    /// Take every transition recorded since the last drain, oldest
    /// first, for the durable ledger.
    pub fn drain_reservation_transitions(&mut self) -> Vec<Reservation> {
        std::mem::take(&mut self.reservation_outbox)
    }

    /// Snapshot of the live reservations (`/desk/state`), soonest-expiry
    /// first.
    pub fn reservations_snapshot(&self) -> Vec<Reservation> {
        let mut out: Vec<Reservation> = self.reservations.values().cloned().collect();
        out.sort_by(|a, b| a.expires_ms.cmp(&b.expires_ms).then_with(|| a.key.cmp(&b.key)));
        out
    }

    /// Move TTL-elapsed live reservations to `expired`.
    pub fn expire_reservations(&mut self, now_ms: u64) {
        let stale: Vec<String> = self
            .reservations
            .values()
            .filter(|r| r.expires_ms <= now_ms)
            .map(|r| r.key.clone())
            .collect();
        for key in stale {
            self.transition(&key, ReservationState::Expired, now_ms);
        }
    }

    /// The live reservations aggregated for the capital policy: each
    /// lands once in the total, once on its side, once at its expiry.
    pub fn reserved_split(&self) -> super::limits::ReservedSplit {
        let mut s = super::limits::ReservedSplit::default();
        for r in self.reservations.values() {
            let a = r.amount as f64;
            s.total += a;
            if r.is_put {
                s.puts += a;
                s.put_underlying_value += r.exercise_cash;
            } else {
                s.calls += a;
                s.call_strike_cash += r.exercise_cash;
            }
            *s.by_expiry.entry(r.expiry_ms).or_default() += a;
            *s.exercise_demand_by_expiry.entry(r.expiry_ms).or_default() += r.exercise_cash;
            s.hedge_notional += r.hedge_notional;
            *s.hedge_notional_by_expiry.entry(r.expiry_ms).or_default() += r.hedge_notional;
        }
        s
    }

    // ── inventory ─────────────────────────────────────────────────────

    /// Record how many of a bucket's units rest as an exchange ask
    /// (SO-416). One ask per holding, so the value REPLACES any previous
    /// commitment; 0 clears it.
    pub fn set_listed_units(&mut self, bucket: ObjectId, units: u64) {
        if units == 0 {
            self.listed_units.remove(&bucket);
        } else {
            self.listed_units.insert(bucket, units);
        }
    }

    /// Units of this bucket currently committed to a resting ask.
    pub fn listed_units(&self, bucket: &ObjectId) -> u64 {
        self.listed_units.get(bucket).copied().unwrap_or(0)
    }

    /// Net naked short units across all written lines (V2 budget).
    pub fn naked_written_units(&self) -> u64 {
        self.written.iter().map(Written::naked).sum()
    }

    /// Re-derive each written line's `covered` from the current holdings:
    /// held coins in the SAME bucket offset written amounts (allocated in
    /// ledger order when several lines share a bucket). Call after any
    /// holdings or written refresh; the remainder is the naked budget.
    pub fn recompute_covered(&mut self) {
        let mut avail: HashMap<ObjectId, u64> = HashMap::new();
        for h in &self.holdings {
            *avail.entry(h.bucket_id).or_default() += h.amount();
        }
        for w in &mut self.written {
            let a = avail.entry(w.bucket_id).or_default();
            w.covered = w.amount.min(*a);
            *a -= w.covered;
        }
    }

    /// Net greeks per expiry (ms) bucket and in total. Longs count
    /// positive, written shorts negative. `marks` maps bucket_id →
    /// per-unit greeks (computed by the caller via [`MarketModel`], so
    /// this stays pure and unit-testable).
    pub fn net_greeks(
        &self,
        per_unit: &HashMap<ObjectId, Greeks>,
    ) -> (HashMap<u64, GreeksAgg>, GreeksAgg) {
        let mut by_expiry: HashMap<u64, GreeksAgg> = HashMap::new();
        let mut total = GreeksAgg::default();
        for h in &self.holdings {
            if let Some(g) = per_unit.get(&h.bucket_id) {
                let amt = h.amount() as f64;
                by_expiry.entry(h.expiry_ms).or_default().add(g, amt, 1.0);
                total.add(g, amt, 1.0);
            }
        }
        for w in &self.written {
            if let Some(g) = per_unit.get(&w.bucket_id) {
                let amt = w.amount as f64;
                by_expiry.entry(w.expiry_ms).or_default().add(g, amt, -1.0);
                total.add(g, amt, -1.0);
            }
        }
        (by_expiry, total)
    }

    // ── P&L attribution ───────────────────────────────────────────────

    /// Record a realized P&L line: bumps the counter and queues the
    /// record for the desk's sinks (metrics + JSONL).
    pub fn record_pnl(&mut self, line: PnlLine, amount: f64, note: &str, now_ms: u64) {
        match line {
            PnlLine::Spread => self.pnl.spread += amount,
            PnlLine::Scalp => self.pnl.scalp += amount,
            PnlLine::Theta => self.pnl.theta += amount,
            PnlLine::Funding => self.pnl.funding += amount,
        }
        self.pnl_outbox.push(PnlRecord { ts_ms: now_ms, line, amount, note: note.to_string() });
    }

    /// Take every P&L record queued since the last drain, oldest first.
    pub fn drain_pnl_records(&mut self) -> Vec<PnlRecord> {
        std::mem::take(&mut self.pnl_outbox)
    }

    /// The running total of one attribution line.
    pub fn pnl_line(&self, line: PnlLine) -> f64 {
        match line {
            PnlLine::Spread => self.pnl.spread,
            PnlLine::Scalp => self.pnl.scalp,
            PnlLine::Theta => self.pnl.theta,
            PnlLine::Funding => self.pnl.funding,
        }
    }
}

/// Which side of a detected fill the desk was on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillSide {
    /// The desk paid premium and holds the option (V1 flows).
    Bought,
    /// The desk received premium and holds the short (V2 flow).
    Wrote,
}

/// One fill the desk participated in, normalized across event shapes.
#[derive(Clone, Debug, PartialEq)]
pub struct DetectedFill {
    pub sequence: u64,
    pub bucket_id: ObjectId,
    pub side: FillSide,
    /// Underlying units filled.
    pub amount: u64,
    /// Premium paid (Bought: the gross premium our collateral released /
    /// our winning bid) or received (Wrote: net of protocol fee),
    /// settlement raw.
    pub premium: u64,
    /// Join key back to the RFQ funnel row this fill closes (SO-425).
    pub link: FillLink,
}

/// How a detected fill relates back to the quote that authorized it.
#[derive(Clone, Debug, PartialEq)]
pub enum FillLink {
    /// WS-signed quote: the quote nonce echoed by `(Put)WriteExecuted`.
    WsQuote { nonce: u64 },
    /// Auction win: the redeemed `BidTicket` id.
    AuctionTicket { ticket: ObjectId },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(b: u8) -> ObjectId {
        ObjectId::new([b; 32])
    }

    fn holding(bucket: u8, expiry: u64, amount: u64) -> Holding {
        Holding {
            bucket_id: oid(bucket),
            option_coin_type: "0x1::c::C".into(),
            asset_coin_type: "0x1::a::A".into(),
            settlement_coin_type: "0x1::s::S".into(),
            is_put: false,
            strike: 100,
            strike_scale: 0,
            expiry_ms: expiry,
            amount_vault: amount,
            amount_wallet: 0,
            coin_positions: Vec::new(),
        }
    }

    fn written(bucket: u8, expiry: u64, amount: u64) -> Written {
        Written {
            bucket_id: oid(bucket),
            position_id: oid(bucket ^ 0x80),
            asset_coin_type: "0x1::a::A".into(),
            is_put: false,
            strike: 100,
            strike_scale: 0,
            expiry_ms: expiry,
            amount,
            covered: 0,
        }
    }

    #[test]
    fn reservations_enforce_nav_bound() {
        let mut b = Book::new(1_000);
        b.deployed = 300;
        let r1 = b.reserve(400, 30_000, 0).unwrap();
        // 300 deployed + 400 reserved + 400 more > 1000 → refused.
        assert_eq!(b.reserve(400, 30_000, 0), Err(ReserveError::ExceedsNav));
        // Exactly filling the gap is fine.
        assert!(b.reserve(300, 30_000, 0).is_ok());
        b.release_reservation(r1, 0);
        assert_eq!(b.reserved_total(), 300);
    }

    #[test]
    fn reservations_ttl_expire() {
        let mut b = Book::new(1_000);
        b.reserve(900, 10_000, 0).unwrap();
        assert_eq!(b.reserve(900, 10_000, 5_000), Err(ReserveError::ExceedsNav));
        // Past the TTL the stale reservation frees its budget.
        assert!(b.reserve(900, 10_000, 20_000).is_ok());
    }

    // ── keyed, durable reservations (SO-444) ───────────────────────────

    fn quote_res(key: &str, nonce: u64, amount: u64, is_put: bool, expiry: u64) -> QuoteReservation {
        QuoteReservation {
            key: key.into(),
            nonce: Some(nonce),
            amount,
            is_put,
            expiry_ms: expiry,
            exercise_cash: 12.0 * amount as f64,
            hedge_notional: 6.0 * amount as f64,
            ttl_ms: 30_000,
        }
    }

    #[test]
    fn keyed_reservations_transition_and_queue_for_persistence() {
        let mut b = Book::new(10_000);
        b.reserve_quote(quote_res("r1", 11, 1_000, false, 100), 0).unwrap();
        b.reserve_quote(quote_res("r2", 12, 2_000, true, 200), 0).unwrap();
        // One live reservation per key.
        assert_eq!(
            b.reserve_quote(quote_res("r1", 13, 1, false, 100), 0),
            Err(ReserveError::DuplicateKey)
        );
        assert_eq!(b.reserved_total(), 3_000);
        // The split counts each once: total, side, expiry.
        let s = b.reserved_split();
        assert_eq!((s.total, s.calls, s.puts), (3_000.0, 1_000.0, 2_000.0));
        assert_eq!(s.by_expiry[&100], 1_000.0);
        assert_eq!(s.by_expiry[&200], 2_000.0);
        assert_eq!(s.call_strike_cash, 12_000.0);
        assert_eq!(s.put_underlying_value, 24_000.0);
        assert_eq!(s.hedge_notional, 18_000.0);
        // quoted → accepted keeps capacity; → filled / reverted /
        // expired free it, each queued exactly once.
        assert!(b.accept_reservation("r1", 1));
        assert_eq!(b.reserved_total(), 3_000);
        assert_eq!(b.fill_reservation_by_nonce(11, 2).as_deref(), Some("r1"));
        assert_eq!(b.fill_reservation_by_nonce(11, 3), None, "already closed");
        assert!(b.revert_reservation("r2", 4));
        assert!(!b.revert_reservation("r2", 5));
        assert_eq!(b.reserved_total(), 0);
        let states: Vec<(String, ReservationState)> = b
            .drain_reservation_transitions()
            .into_iter()
            .map(|r| (r.key, r.state))
            .collect();
        assert_eq!(
            states,
            vec![
                ("r1".into(), ReservationState::Quoted),
                ("r2".into(), ReservationState::Quoted),
                ("r1".into(), ReservationState::Accepted),
                ("r1".into(), ReservationState::Filled),
                ("r2".into(), ReservationState::Reverted),
            ]
        );
        assert!(b.drain_reservation_transitions().is_empty());
        // TTL: expired lands in the outbox too.
        b.reserve_quote(quote_res("r3", 14, 500, false, 100), 10).unwrap();
        b.expire_reservations(40_010);
        assert_eq!(b.reserved_total(), 0);
        let out = b.drain_reservation_transitions();
        assert_eq!(out.last().map(|r| r.state), Some(ReservationState::Expired));
    }

    /// Doc 08 §4.6 gate: restarting during live quotes preserves the same
    /// available capacity. The durable rows are serialized (the DB row
    /// shape round-trips through serde) and reloaded into a fresh book.
    #[test]
    fn restart_during_live_quotes_preserves_capacity() {
        let mut before = Book::new(10_000);
        before.deployed = 1_000;
        before.reserve_quote(quote_res("ws-a", 1, 2_000, false, 100), 0).unwrap();
        // ws-b carries a longer TTL so it alone outlives the second restart.
        let mut b_res = quote_res("ws-b", 2, 1_500, true, 200);
        b_res.ttl_ms = 60_000;
        before.reserve_quote(b_res, 0).unwrap();
        before.reserve_quote(quote_res("ws-c", 3, 700, false, 200), 0).unwrap();
        before.accept_reservation("ws-b", 5);
        let split_before = before.reserved_split();
        let free_before = before.nav - before.deployed - before.reserved_total();

        // "Durable state": every transition the DB would hold, latest
        // row per key.
        let fixture = serde_json::to_string(&before.drain_reservation_transitions()).unwrap();
        let rows: Vec<Reservation> = serde_json::from_str(&fixture).unwrap();
        let mut latest: HashMap<String, Reservation> = HashMap::new();
        for r in rows {
            latest.insert(r.key.clone(), r);
        }

        // Restart: nothing filled on chain, nothing expired.
        let (live, transitions) =
            reconcile_reservations(latest.values().cloned().collect(), &Default::default(), 10);
        assert!(transitions.is_empty());
        let mut after = Book::new(10_000);
        after.deployed = 1_000;
        for r in live {
            after.restore_reservation(r);
        }
        assert_eq!(after.reserved_total(), before.reserved_total());
        assert_eq!(after.reserved_split(), split_before);
        assert_eq!(after.nav - after.deployed - after.reserved_total(), free_before);
        // The accepted one came back accepted.
        assert_eq!(
            after.reservations_snapshot().iter().find(|r| r.key == "ws-b").map(|r| r.state),
            Some(ReservationState::Accepted)
        );

        // Restart AFTER ws-a filled on chain while we were down and ws-c
        // aged out: both close, ws-b alone still holds capacity.
        let filled = std::collections::HashSet::from([1u64]);
        let (live, transitions) =
            reconcile_reservations(latest.values().cloned().collect(), &filled, 30_000);
        assert_eq!(live.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(), vec!["ws-b"]);
        let mut closed: Vec<(String, ReservationState)> =
            transitions.into_iter().map(|r| (r.key, r.state)).collect();
        closed.sort();
        assert_eq!(
            closed,
            vec![
                ("ws-a".into(), ReservationState::Filled),
                ("ws-c".into(), ReservationState::Expired)
            ]
        );
    }

    #[test]
    fn net_greeks_aggregates_by_expiry_with_signs() {
        let mut b = Book::new(0);
        b.holdings.push(holding(1, 100, 10));
        b.holdings.push(holding(2, 200, 5));
        b.written.push(written(3, 100, 4));
        let g = Greeks { delta: 0.5, gamma: 0.01, vega: 20.0, theta: -5.0, rho: 0.0 };
        let mut per_unit = HashMap::new();
        per_unit.insert(oid(1), g);
        per_unit.insert(oid(2), g);
        per_unit.insert(oid(3), g);
        let (by_expiry, total) = b.net_greeks(&per_unit);
        // Expiry 100: +10 long, −4 written → net 6 units of each greek.
        let e100 = by_expiry.get(&100).unwrap();
        assert!((e100.delta_units - 3.0).abs() < 1e-9); // 0.5 × 6
        assert!((e100.vega - 120.0).abs() < 1e-9); // 20 × 6
        let e200 = by_expiry.get(&200).unwrap();
        assert!((e200.delta_units - 2.5).abs() < 1e-9);
        assert!((total.delta_units - 5.5).abs() < 1e-9);
        assert!((total.theta_per_day - (-55.0)).abs() < 1e-9);
    }

    #[test]
    fn listed_units_replace_and_clear() {
        let mut b = Book::new(0);
        assert_eq!(b.listed_units(&oid(1)), 0);
        b.set_listed_units(oid(1), 500);
        assert_eq!(b.listed_units(&oid(1)), 500);
        // One ask per holding: a new value replaces, never accumulates.
        b.set_listed_units(oid(1), 200);
        assert_eq!(b.listed_units(&oid(1)), 200);
        b.set_listed_units(oid(1), 0);
        assert_eq!(b.listed_units(&oid(1)), 0);
    }

    #[test]
    fn naked_written_units_sums_uncovered() {
        let mut b = Book::new(0);
        b.written.push(Written { covered: 7, ..written(1, 1, 10) });
        b.written.push(Written { is_put: true, covered: 5, ..written(2, 1, 5) });
        assert_eq!(b.naked_written_units(), 3);
    }

    #[test]
    fn covered_netting_offsets_same_bucket_and_computes_naked() {
        let mut b = Book::new(0);
        // Bucket 1: 10 held, 6 written → fully covered write, 4 held spare.
        // Bucket 2: nothing held, 4 written → fully naked.
        b.holdings.push(holding(1, 100, 10));
        b.written.push(written(1, 100, 6));
        b.written.push(written(2, 100, 4));
        b.recompute_covered();
        assert_eq!(b.written[0].covered, 6);
        assert_eq!(b.written[1].covered, 0);
        assert_eq!(b.naked_written_units(), 4);

        // Held shrinks to 3 (partial cover); second line in the SAME
        // bucket gets nothing once the first drained the held amount.
        b.holdings[0].amount_vault = 3;
        b.written.push(written(1, 100, 5));
        b.recompute_covered();
        assert_eq!(b.written[0].covered, 3);
        assert_eq!(b.written[2].covered, 0);
        assert_eq!(b.naked_written_units(), 3 + 4 + 5);

        // Net greeks see the true net: bucket 1 expiry-100 holds 3 long
        // vs 6 + 5 written, bucket 2 adds 4 written → net −12 units.
        let g = Greeks { delta: 1.0, gamma: 0.0, vega: 1.0, theta: 0.0, rho: 0.0 };
        let mut per_unit = HashMap::new();
        per_unit.insert(oid(1), g);
        per_unit.insert(oid(2), g);
        let (_, total) = b.net_greeks(&per_unit);
        assert!((total.delta_units - (3.0 - 6.0 - 5.0 - 4.0)).abs() < 1e-9);
    }
}
