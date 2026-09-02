//! The desk's book — single source of truth (00-plan Phase 2).
//!
//! Tracks held option inventory (vault custody + wallet float), written
//! positions, NAV, the reservation ledger (`reservations + deployed ≤ NAV`
//! before every quote), and realized P&L attribution counters
//! (spread / scalp / theta / funding) exported as metrics and appended to
//! a JSONL file.
//!
//! On boot the inventory is reconstructed from VAULT custody:
//!   - The budget base from the indexer's `trading_vaults` view via
//!     [`budget_base`] (SO-418): the latest appraised NAV on untranched
//!     vaults, the junior/risk-bearing measure on tranched ones — falling
//!     back to the vault's settlement free balance
//!     (`vault::free_balance_of<Settlement>` dev-inspect) when nothing
//!     has priced the vault yet. **Documented choice**: the appraised
//!     figure includes positions; the free-balance fallback under-counts
//!     by design and only covers a freshly-created vault.
//!   - Held option coins per live bucket via
//!     `vault::free_balance_of<OptionCoin>` dev-inspect (the
//!     `custody_balance` pattern from the old vault_deepbook quoter),
//!     plus the bot wallet's own float of the same coin types.
//!   - Written positions: vault-custodied `Position` objects. Ids come
//!     from the indexer's `trading_vault_positions` view (the same
//!     indexer source the NAV path uses), amount + bucket from on-chain
//!     object reads, strike/expiry/kind from the api-service
//!     bucket-metadata path the holdings reconstruction already uses.
//!     This mirrors `sui_tx::tx::appraisal::discover_holdings`'
//!     classification (`::position::Position` type suffix; RfqTickets,
//!     DeepBook custody and coin objects are not written inventory)
//!     without pulling in the appraisal composer's full
//!     dynamic-field walk. Same-bucket held coins mark written lines
//!     `covered` ([`Book::recompute_covered`]); the uncovered remainder
//!     is the V2 naked-short budget.
//!
//! Fill detection (P&L attribution): [`classify_fill`] + [`apply_fills`]
//! turn indexer events into spread-line records, resumed from a
//! persisted sequence cursor ([`FillCursor`], write-after-apply).
//!
//! Reservations (SO-444, doc 08 §4.6) are keyed by quote/request id with
//! explicit `quoted → accepted | reverted | expired | filled`
//! transitions. The map here holds the LIVE ones; every transition is
//! queued in an outbox the desk persists to the history DB, and boot
//! re-installs the still-live rows after reconciling them against chain
//! fills ([`reconcile_reservations`]).

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use protocol_types::events::{ChainEvent, IndexedEvent};
use protocol_types::ids::{ObjectId, SuiAddress};
use serde::{Deserialize, Serialize};
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::TransactionKind;

use super::model::Greeks;

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

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PnlLine {
    Spread,
    Scalp,
    Theta,
    Funding,
}

#[derive(Serialize)]
struct PnlRecord<'a> {
    ts_ms: u64,
    line: PnlLine,
    amount: f64,
    note: &'a str,
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
    /// JSONL sink for P&L attribution records (append-only).
    pnl_path: Option<PathBuf>,
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
    pub fn new(nav: u64, pnl_path: Option<PathBuf>) -> Self {
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
            pnl_path,
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
    pub fn release_reservation(&mut self, id: u64) {
        let now = super::auctions::now_ms();
        self.transition(&format!("legacy-{id}"), ReservationState::Reverted, now);
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

    /// Record a realized P&L line: bumps the counter, exports the metric,
    /// appends a JSONL record.
    pub fn record_pnl(&mut self, line: PnlLine, amount: f64, note: &str, now_ms: u64) {
        match line {
            PnlLine::Spread => self.pnl.spread += amount,
            PnlLine::Scalp => self.pnl.scalp += amount,
            PnlLine::Theta => self.pnl.theta += amount,
            PnlLine::Funding => self.pnl.funding += amount,
        }
        let label = match line {
            PnlLine::Spread => "spread",
            PnlLine::Scalp => "scalp",
            PnlLine::Theta => "theta",
            PnlLine::Funding => "funding",
        };
        metrics::counter!("mm_desk_pnl_total", "line" => label)
            .increment(amount.abs().round() as u64);
        metrics::gauge!("mm_desk_pnl", "line" => label).set(match line {
            PnlLine::Spread => self.pnl.spread,
            PnlLine::Scalp => self.pnl.scalp,
            PnlLine::Theta => self.pnl.theta,
            PnlLine::Funding => self.pnl.funding,
        });
        if let Some(path) = &self.pnl_path {
            let rec = PnlRecord { ts_ms: now_ms, line, amount, note };
            if let Err(e) = append_jsonl(path, &rec) {
                tracing::warn!(error = %format!("{e:#}"), "pnl jsonl append failed");
            }
        }
    }
}

fn append_jsonl<T: Serialize>(path: &PathBuf, rec: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    serde_json::to_writer(&mut f, rec)?;
    f.write_all(b"\n")?;
    Ok(())
}

// ── v2 budget base (SO-418) ────────────────────────────────────────────

/// Mirror of the Move `SHARE_OFFSET` (shares are offset-scaled vs value;
/// observed pps_e12 = value × 1e12 × OFFSET / shares).
const SHARE_OFFSET: u128 = 1_000_000;

/// The desk's premium-budget base for one vault view (SO-418).
///
/// v1 used `latest_pps_e12 × total_shares` — total NAV. v2 bounds
/// reservations by the RISK-BEARING measure instead:
///
/// - untranched (`structure_code == 0`): the whole book is risk capital —
///   `latest_nav` from the last consumed appraisal/capital sync, falling
///   back to the observed pps over the junior book (which carries the
///   untranched supply).
/// - tranched: the junior side only — `junior_nav` from the last capital
///   sync, falling back to the junior observed pps × junior shares. The
///   senior claim is not the desk's to deploy against.
///
/// `None` = no usable measure yet (fresh vault); callers fall back to the
/// free settlement balance, which under-counts by design.
pub fn budget_base(v: &indexer_graphql::TradingVault) -> Option<u64> {
    let from_pps = |pps: Option<u128>, shares: u128| {
        pps.map(|pps| {
            u64::try_from(
                pps.saturating_mul(shares) / 1_000_000_000_000u128 / SHARE_OFFSET,
            )
            .unwrap_or(u64::MAX)
        })
    };
    let nav = if v.structure_code == 0 {
        v.latest_nav
            .or_else(|| from_pps(v.latest_pps_e12, v.junior_shares).map(u128::from))
    } else {
        v.junior_nav
            .or_else(|| from_pps(v.latest_junior_pps_e12, v.junior_shares).map(u128::from))
    };
    nav.map(|n| u64::try_from(n).unwrap_or(u64::MAX))
}

/// Is this vault "risk-off" for the desk (SO-418)? Mirrors the §8.4b gate
/// set the quote sessions and `vault_mm` releases abort on (code 124),
/// plus the terminal states: capital risk state not Healthy, curator
/// commitment breached, lifecycle not open, or settled.
pub fn vault_risk_off(v: &indexer_graphql::TradingVault) -> bool {
    v.risk_state != 0 || v.curator_commitment_breached || v.state != "open" || v.settled
}

// ── boot reconstruction ────────────────────────────────────────────────

/// Everything reconstruction needs (kept together so `spawn_desk` stays
/// readable).
pub struct ReconstructParams<'a> {
    pub wrap: &'a sui_tx::sui_client::SuiClientWrapper,
    pub indexer: &'a indexer_graphql::IndexerClient,
    pub api: &'a api_service_client::ApiServiceClient,
    pub trading_vault_package: ObjectID,
    pub vault_id: ObjectID,
    pub settlement_coin_type: String,
    pub pnl_path: Option<PathBuf>,
    /// Options package id — lets a holding be reconstructed from its
    /// option-coin type when the bucket catalog does not list it.
    pub options_package: Option<String>,
}

/// Reconstruct the book from vault custody (module docs describe the
/// sources; SO-418 switched the budget base from total pps × shares to
/// the risk-state-aware measure — see [`budget_base`]).
pub async fn reconstruct(p: ReconstructParams<'_>) -> Result<Book> {
    // Budget base: latest NAV (untranched) / junior NAV (tranched) from
    // the indexer view, else settlement free balance.
    let vault_hex = p.vault_id.to_hex_literal();
    let vaults = p.indexer.trading_vaults().await.context("indexer trading_vaults")?;
    let ours = vaults
        .iter()
        .find(|v| v.vault_id.to_hex() == vault_hex || format!("0x{}", v.vault_id.to_hex()) == vault_hex);
    let nav = match ours.and_then(budget_base) {
        Some(nav) => nav,
        None => {
            if ours.is_none() {
                tracing::warn!(vault = %vault_hex, "vault not in indexer view yet; NAV from free balance");
            }
            free_balance_of(p.wrap, p.trading_vault_package, p.vault_id, &p.settlement_coin_type)
                .await
                .unwrap_or(0)
        }
    };

    let mut book = Book::new(nav, p.pnl_path);
    book.holdings =
        fetch_holdings(
            p.wrap,
            p.indexer,
            p.api,
            p.trading_vault_package,
            p.vault_id,
            p.options_package.as_deref(),
        )
        .await?;
    book.written = fetch_written(p.wrap, p.indexer, p.api, p.vault_id).await?;
    book.recompute_covered();
    tracing::info!(
        nav = book.nav,
        holdings = book.holdings.len(),
        written = book.written.len(),
        naked = book.naked_written_units(),
        "book reconstructed from vault custody"
    );
    Ok(book)
}

/// Held option coins: every live bucket's option-coin balance in the
/// vault's free balances + VaultMm coin-custody positions + the bot
/// wallet float. Used at boot AND by the refresher's periodic custody
/// re-sync (auction wins / sweeps change balances out-of-band).
pub async fn fetch_holdings(
    wrap: &sui_tx::sui_client::SuiClientWrapper,
    indexer: &indexer_graphql::IndexerClient,
    api: &api_service_client::ApiServiceClient,
    trading_vault_package: ObjectID,
    vault_id: ObjectID,
    // Options package id, for decoding option-coin types the catalog omits.
    options_package: Option<&str>,
) -> Result<Vec<Holding>> {
    let mut holdings = Vec::new();
    // Pool-less buckets count. `tradeable_buckets` requires a DeepBook pool
    // and the default board drops off-ladder series, so scanning it left the
    // desk blind to any-strike inventory — understating NAV, net vega and the
    // cover available to a V2 write, silently.
    let buckets = api.writable_buckets().await.context("writable buckets")?;
    // VaultMm coin-custody positions (writer-flow sweeps store option
    // coins AS positions), keyed by the canonical option-coin type.
    let mut coin_positions = fetch_coin_positions(wrap, indexer, vault_id).await?;
    for b in &buckets {
        if b.call_coin_type.is_empty() {
            continue;
        }
        let vault_held = free_balance_of(wrap, trading_vault_package, vault_id, &b.call_coin_type)
            .await
            .unwrap_or(0);
        let wallet_held = match sui_types::parse_sui_struct_tag(&b.call_coin_type) {
            Ok(tag) => wrap
                .client
                .balance(wrap.signer.address, &tag)
                .await
                .map(|bal| u64::try_from(bal).unwrap_or(u64::MAX))
                .unwrap_or(0),
            Err(_) => 0,
        };
        let positions = coin_positions
            .remove(&protocol_types::asset::canonicalize_move_type(&b.call_coin_type))
            .unwrap_or_default();
        if vault_held == 0 && wallet_held == 0 && positions.is_empty() {
            continue;
        }
        // is_put isn't on TradeableBucket; resolve it from the cached
        // bucket-pricing lookup.
        let is_put = api
            .bucket_pricing(b.bucket_id.clone())
            .await
            .ok()
            .flatten()
            .map(|bp| bp.is_put)
            .unwrap_or(false);
        holdings.push(Holding {
            bucket_id: b.bucket_id.clone(),
            option_coin_type: b.call_coin_type.clone(),
            asset_coin_type: b.asset_coin_type.clone(),
            settlement_coin_type: b.settlement_coin_type.clone(),
            is_put,
            strike: b.strike_raw,
            strike_scale: b.strike_scale,
            expiry_ms: b.expiry_ms,
            amount_vault: vault_held,
            amount_wallet: wallet_held,
            coin_positions: positions,
        });
    }

    // Anything still in `coin_positions` is custody the catalog does not know
    // about — a bucket at an expiry the board has since dropped, say. The
    // option-coin type encodes its own spec, so the line is reconstructable
    // without any catalog at all; losing it would understate the book.
    if !coin_positions.is_empty() {
        for (coin_type, positions) in std::mem::take(&mut coin_positions) {
            let Some(spec) = options_package
                .and_then(|pkg| protocol_types::bucket_spec::decode_option_coin_type(pkg, &coin_type))
            else {
                tracing::warn!(
                    %coin_type,
                    "vault holds a coin that is not a decodable option coin; excluded from the book"
                );
                continue;
            };
            let amount: u64 = positions.iter().map(|p| p.amount).sum();
            tracing::info!(
                %coin_type,
                amount,
                "recovered a holding the bucket catalog omitted (decoded from the coin type)"
            );
            holdings.push(Holding {
                bucket_id: ObjectId::ZERO,
                option_coin_type: coin_type.clone(),
                asset_coin_type: protocol_types::asset::canonicalize_move_type(&spec.asset),
                settlement_coin_type: protocol_types::asset::canonicalize_move_type(
                    &spec.settlement,
                ),
                is_put: spec.is_put,
                strike: spec.sig as u128,
                strike_scale: spec.exp,
                expiry_ms: spec.expiry_ms,
                amount_vault: 0,
                amount_wallet: 0,
                coin_positions: positions,
            });
        }
    }
    Ok(holdings)
}

/// VaultMm coin-custody positions: active vault positions whose object
/// type is `0x2::coin::Coin<T>`, grouped by the canonical `T`. Ids come
/// from the indexer's `trading_vault_positions` view (like
/// [`fetch_written`]); amounts from on-chain object reads.
async fn fetch_coin_positions(
    wrap: &sui_tx::sui_client::SuiClientWrapper,
    indexer: &indexer_graphql::IndexerClient,
    vault_id: ObjectID,
) -> Result<HashMap<String, Vec<CoinPosition>>> {
    let vault_pt = ObjectId::new(vault_id.into_bytes());
    let positions = indexer
        .trading_vault_positions(vault_pt)
        .await
        .context("indexer trading_vault_positions")?;
    let mut out: HashMap<String, Vec<CoinPosition>> = HashMap::new();
    for pos in positions.iter().filter(|p| p.active) {
        let pos_id = ObjectID::new(*pos.position_id.as_bytes());
        let Some((object, _)) = wrap
            .client
            .try_get_object_json(pos_id)
            .await
            .with_context(|| format!("reading vault position {pos_id}"))?
        else {
            continue; // removed since the indexer view was written
        };
        // `0x2::coin::Coin<T>` custody positions only; everything else
        // (written Positions, custody objects, tickets) is not a coin.
        let Some(coin) = object.as_coin_maybe() else {
            continue;
        };
        let Some(inner) = object
            .struct_tag()
            .and_then(|t| t.type_params.first().cloned())
        else {
            continue;
        };
        let amount = coin.value();
        if amount == 0 {
            continue;
        }
        out.entry(protocol_types::asset::canonicalize_move_type(
            &inner.to_canonical_string(/* with_prefix */ true),
        ))
            .or_default()
            .push(CoinPosition { position_id: pos.position_id, amount });
    }
    Ok(out)
}

/// Written (short) positions: vault-custodied `Position` objects. Ids
/// from the indexer's `trading_vault_positions` view (active only),
/// amount + bucket from on-chain object reads, series metadata from the
/// bucket-pricing lookup. Used at boot AND by the refresher's periodic
/// custody re-sync (new writes/offset closes land out-of-band). Callers
/// run [`Book::recompute_covered`] after installing the result.
pub async fn fetch_written(
    wrap: &sui_tx::sui_client::SuiClientWrapper,
    indexer: &indexer_graphql::IndexerClient,
    api: &api_service_client::ApiServiceClient,
    vault_id: ObjectID,
) -> Result<Vec<Written>> {
    let vault_pt = ObjectId::new(vault_id.into_bytes());
    let positions = indexer
        .trading_vault_positions(vault_pt)
        .await
        .context("indexer trading_vault_positions")?;
    let mut written = Vec::new();
    for pos in positions.iter().filter(|p| p.active) {
        let pos_id = ObjectID::new(*pos.position_id.as_bytes());
        let Some((object, json)) = wrap
            .client
            .try_get_object_json(pos_id)
            .await
            .with_context(|| format!("reading vault position {pos_id}"))?
        else {
            continue; // removed since the indexer view was written
        };
        let ty = object
            .struct_tag()
            .map(|t| t.to_canonical_string(/* with_prefix */ true))
            .unwrap_or_default();
        // discover_holdings' classification: only `::position::Position`
        // objects are written option inventory (RfqTickets, DeepBook
        // custody and held coins are not).
        if !ty.ends_with("::position::Position") {
            continue;
        }
        let fields =
            json.ok_or_else(|| anyhow!("position {pos_id} has no readable Move content"))?;
        let bucket_id = fields
            .get("bucket_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| ObjectId::from_hex(s).ok())
            .ok_or_else(|| anyhow!("position {pos_id} missing bucket_id"))?;
        let range_start = json_u128(&fields, "range_start")?;
        let range_end = json_u128(&fields, "range_end")?;
        let amount = u64::try_from(range_end.saturating_sub(range_start)).unwrap_or(u64::MAX);
        if amount == 0 {
            continue; // fully offset-closed, awaiting destroy_empty
        }
        let Some(bucket) = api.bucket_pricing(bucket_id).await? else {
            tracing::warn!(
                position = %pos_id,
                bucket = %bucket_id,
                "written position's bucket unknown to api-service; skipping line"
            );
            continue;
        };
        written.push(Written {
            bucket_id,
            position_id: pos.position_id,
            asset_coin_type: bucket.asset_coin_type.clone(),
            is_put: bucket.is_put,
            strike: bucket.strike,
            strike_scale: bucket.strike_scale,
            expiry_ms: bucket.expiry_ms,
            amount,
            covered: 0,
        });
    }
    Ok(written)
}

/// A `u128` position field that may arrive as a JSON number or string.
fn json_u128(fields: &serde_json::Value, name: &str) -> Result<u128> {
    let v = fields
        .get(name)
        .ok_or_else(|| anyhow!("position missing field {name}"))?;
    match v {
        serde_json::Value::Number(n) => n
            .as_u64()
            .map(u128::from)
            .ok_or_else(|| anyhow!("non-u64 {name}: {n}")),
        serde_json::Value::String(s) => s.parse().with_context(|| format!("parsing {name} {s:?}")),
        other => Err(anyhow!("unexpected {name}: {other}")),
    }
}

/// `vault::free_balance_of<T>(vault)` via dev-inspect (the old
/// vault_deepbook `custody_balance` pattern).
pub async fn free_balance_of(
    wrap: &sui_tx::sui_client::SuiClientWrapper,
    trading_vault_package: ObjectID,
    vault_id: ObjectID,
    coin_type: &str,
) -> Result<u64> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(sui_tx::tx::shared_object_arg(&wrap.client, vault_id, false).await?)?;
    let tag = TypeTag::from_str(coin_type).with_context(|| format!("parsing {coin_type}"))?;
    pt.programmable_move_call(
        trading_vault_package,
        Identifier::new("vault").unwrap(),
        Identifier::new("free_balance_of").unwrap(),
        vec![tag],
        vec![vault],
    );
    let res = wrap
        .client
        .dev_inspect_ptb(wrap.signer.address, pt)
        .await
        .context("dev-inspecting free_balance_of")?;
    sui_tx::chain::decode_return_value::<u64>(&res, 0).context("decoding free balance")
}

/// The vault's on-chain external-account limits via dev-inspect of
/// `vault::external_limits`: `(budget_bps, daily_release_bps,
/// released_in_window, window_start_ms)`. Aborts (→ `Err`) when no
/// external account is registered — callers check the indexer view's
/// `external_account` first.
pub async fn external_limits(
    wrap: &sui_tx::sui_client::SuiClientWrapper,
    trading_vault_package: ObjectID,
    vault_id: ObjectID,
) -> Result<(u64, u64, u64, u64)> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(sui_tx::tx::shared_object_arg(&wrap.client, vault_id, false).await?)?;
    pt.programmable_move_call(
        trading_vault_package,
        Identifier::new("vault").unwrap(),
        Identifier::new("external_limits").unwrap(),
        vec![],
        vec![vault],
    );
    let res = wrap
        .client
        .dev_inspect_ptb(wrap.signer.address, pt)
        .await
        .context("dev-inspecting external_limits")?;
    let decode = |n: usize| sui_tx::chain::decode_return_value::<u64>(&res, n);
    Ok((
        decode(0).context("decoding budget_bps")?,
        decode(1).context("decoding daily_release_bps")?,
        decode(2).context("decoding released_in_window")?,
        decode(3).context("decoding window_start_ms")?,
    ))
}

// ── fill detection → spread-line attribution ───────────────────────────
//
// A poller (spawned in `mod.rs`) scans the indexer events feed for fills
// that touch OUR vault and books the spread line:
//
//   spread += (model fair at the current surface − premium paid)   [buys]
//   spread += (premium received − model fair at the current surface) [writes]
//
// **V1 attribution approximation (documented)**: fair is evaluated at
// DETECTION time, not at the on-chain fill time — the surface may have
// moved between the fill and the poll that observes it, so the
// spread-vs-scalp/theta split is approximate while the P&L total stays
// exact. The scalp line comes from `HedgeVenue::realized_pnl` deltas in
// the rebalancer; theta/funding accrue in the refresher.
//
// Identity: the desk is a vault-only maker, so "our" fills are exactly
// the events whose collateral released from the vault
// (`WriteExecuted`/`PutWriteExecuted` with `collateral_source == vault`
// — for our quotes the vault IS the QuoteSigner's collateral source and
// `signer_token_recipient` is the vault address, see `VaultRouting`) and
// the auction-channel WINS. Vault-funded bids route every auction output
// to the BidTicket's address (never the vault), so `RfqSettled`
// recipients can't identify us; instead a win is detected when the
// keeper redeems the ticket into the vault (`TvBidRedeemed` with our
// vault_id), joined to its `TvBidPlaced` for the ticket cost + bucket
// ([`classify_ticket_win`]).

/// Persisted events-feed cursor (sequence high-water mark). Written
/// AFTER fills are applied, so a crash between apply and persist
/// re-applies at most one batch; a clean restart never double-counts.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct FillCursor {
    #[serde(default)]
    pub last_sequence: u64,
}

impl FillCursor {
    /// `None` when no cursor file exists yet (first boot — the poller
    /// seeds from the indexer head so history isn't replayed as fills).
    pub fn load(path: &Path) -> Option<Self> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    pub fn persist(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        if let Err(e) = serde_json::to_string(self).map_err(anyhow::Error::from).and_then(|s| {
            std::fs::write(path, s).map_err(anyhow::Error::from)
        }) {
            tracing::warn!(error = %format!("{e:#}"), path = %path.display(), "fill cursor persist failed");
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

/// Classify one indexed event as a desk fill, or `None` when it isn't
/// ours / isn't a fill. `vault` is the trading vault's object id (its
/// address is the same 32 bytes).
pub fn classify_fill(ev: &IndexedEvent, vault: ObjectId) -> Option<DetectedFill> {
    let vault_addr = SuiAddress::new(*vault.as_bytes());
    match &ev.event {
        ChainEvent::WriteExecuted(w) if w.collateral_source == vault => {
            let (side, premium) = if w.call_token_recipient == vault_addr {
                (FillSide::Bought, w.gross_premium)
            } else {
                (FillSide::Wrote, w.net_premium)
            };
            Some(DetectedFill {
                sequence: ev.sequence,
                bucket_id: w.bucket_id,
                side,
                amount: w.write_amount,
                premium,
                link: FillLink::WsQuote { nonce: w.nonce },
            })
        }
        ChainEvent::PutWriteExecuted(w) if w.collateral_source == vault => {
            let (side, premium) = if w.put_token_recipient == vault_addr {
                (FillSide::Bought, w.gross_premium)
            } else {
                (FillSide::Wrote, w.net_premium)
            };
            Some(DetectedFill {
                sequence: ev.sequence,
                bucket_id: w.bucket_id,
                side,
                amount: w.write_amount,
                premium,
                link: FillLink::WsQuote { nonce: w.nonce },
            })
        }
        _ => None,
    }
}

/// Auction-channel win detection under vault-funded bids (SO-299): the
/// settle routes winnings to the TICKET address, never the vault, so a
/// win becomes observable when the keeper's crank redeems the ticket
/// into the vault (`TvBidRedeemed`). The ticket's `TvBidPlaced` (joined
/// by ticket id) supplies the cost (escrow) and the bucket.
pub fn classify_ticket_win(
    ev: &IndexedEvent,
    vault: ObjectId,
    placed_by_ticket: &HashMap<ObjectId, protocol_types::events::TvBidPlaced>,
) -> Option<DetectedFill> {
    let ChainEvent::TvBidRedeemed(r) = &ev.event else {
        return None;
    };
    if r.vault_id != vault {
        return None;
    }
    let Some(placed) = placed_by_ticket.get(&r.ticket_id) else {
        tracing::warn!(
            ticket = %r.ticket_id.to_hex(),
            "won ticket redeemed but its BidPlaced left the event window; fill not attributed"
        );
        return None;
    };
    Some(DetectedFill {
        sequence: ev.sequence,
        bucket_id: placed.bucket_id,
        side: FillSide::Bought,
        amount: placed.win_amount,
        premium: placed.escrow_amount,
        link: FillLink::AuctionTicket { ticket: r.ticket_id },
    })
}

/// Apply detected fills (paired with their model fair TOTAL premium at
/// detection) to the spread line, advance the cursor, then persist it
/// (write-after-apply). Fills at or below the cursor are skipped, so a
/// replay of an already-applied batch is a no-op. Returns how many fills
/// were applied.
pub fn apply_fills(
    book: &mut Book,
    cursor: &mut FillCursor,
    cursor_path: &Path,
    fills: &[(DetectedFill, f64)],
    now_ms: u64,
) -> usize {
    let mut applied = 0;
    for (f, fair_total) in fills {
        if f.sequence <= cursor.last_sequence {
            continue;
        }
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
        book.record_pnl(PnlLine::Spread, spread, &note, now_ms);
        metrics::counter!("mm_desk_fills_total", "side" => label).increment(1);
        cursor.last_sequence = f.sequence;
        applied += 1;
    }
    if applied > 0 {
        cursor.persist(cursor_path);
    }
    applied
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

    // ── v2 budget base (SO-418) ────────────────────────────────────────

    #[test]
    fn budget_base_untranched_prefers_latest_nav_then_pps_over_junior_book() {
        let mut v = crate::desk::provision::test_vault_view(1, 1, "open");
        // Fresh vault: nothing to price from.
        assert_eq!(budget_base(&v), None);
        // Observed pps over the junior book (untranched supply lives
        // there): pps_e12 = value×1e12×OFFSET/shares.
        // pps_e12 = value×1e12×OFFSET/shares → a par vault reads 1e12.
        v.junior_shares = 500 * 1_000_000; // 500 value at OFFSET scale
        v.latest_pps_e12 = Some(1_000_000_000_000);
        assert_eq!(budget_base(&v), Some(500));
        // The appraised NAV wins once present.
        v.latest_nav = Some(1_234);
        assert_eq!(budget_base(&v), Some(1_234));
    }

    #[test]
    fn budget_base_tranched_bounds_by_the_junior_measure() {
        let mut v = crate::desk::provision::test_vault_view(1, 1, "open");
        v.structure_code = 1;
        v.latest_nav = Some(10_000); // total NAV must NOT be the budget
        assert_eq!(budget_base(&v), None, "senior claim is not deployable");
        v.junior_nav = Some(3_000);
        assert_eq!(budget_base(&v), Some(3_000));
        // Without a sync, the junior observed pps stands in.
        v.junior_nav = None;
        v.junior_shares = 2_000 * 1_000_000;
        v.latest_junior_pps_e12 = Some(1_000_000_000_000); // par

        assert_eq!(budget_base(&v), Some(2_000));
    }

    #[test]
    fn risk_off_covers_state_breach_lifecycle_and_settlement() {
        let healthy = crate::desk::provision::test_vault_view(1, 1, "open");
        assert!(!vault_risk_off(&healthy));
        let mut v = healthy.clone();
        v.risk_state = 1; // CoverageBreach
        assert!(vault_risk_off(&v));
        let mut v = healthy.clone();
        v.curator_commitment_breached = true;
        assert!(vault_risk_off(&v));
        let mut v = healthy.clone();
        v.state = "closing".into();
        assert!(vault_risk_off(&v));
        let mut v = healthy.clone();
        v.settled = true;
        assert!(vault_risk_off(&v));
    }

    #[test]
    fn reservations_enforce_nav_bound() {
        let mut b = Book::new(1_000, None);
        b.deployed = 300;
        let r1 = b.reserve(400, 30_000, 0).unwrap();
        // 300 deployed + 400 reserved + 400 more > 1000 → refused.
        assert_eq!(b.reserve(400, 30_000, 0), Err(ReserveError::ExceedsNav));
        // Exactly filling the gap is fine.
        assert!(b.reserve(300, 30_000, 0).is_ok());
        b.release_reservation(r1);
        assert_eq!(b.reserved_total(), 300);
    }

    #[test]
    fn reservations_ttl_expire() {
        let mut b = Book::new(1_000, None);
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
        let mut b = Book::new(10_000, None);
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
        let mut before = Book::new(10_000, None);
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
        let mut after = Book::new(10_000, None);
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
        let mut b = Book::new(0, None);
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
        let mut b = Book::new(0, None);
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
        let mut b = Book::new(0, None);
        b.written.push(Written { covered: 7, ..written(1, 1, 10) });
        b.written.push(Written { is_put: true, covered: 5, ..written(2, 1, 5) });
        assert_eq!(b.naked_written_units(), 3);
    }

    #[test]
    fn covered_netting_offsets_same_bucket_and_computes_naked() {
        let mut b = Book::new(0, None);
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

    // ── fill detection ─────────────────────────────────────────────────

    fn hexid(b: u8) -> String {
        oid(b).to_hex()
    }

    /// Decode a canned IndexedEvent from the exact wire JSON the indexer
    /// GraphQL client produces (tagged ChainEvent envelope).
    fn canned_event(seq: u64, event: serde_json::Value) -> IndexedEvent {
        serde_json::from_value(serde_json::json!({
            "sequence": seq.to_string(),
            "timestamp_ms": "1000",
            "event": event,
        }))
        .unwrap()
    }

    fn canned_write_executed(collateral_source: u8, call_recipient: u8) -> serde_json::Value {
        serde_json::json!({
            "type": "WriteExecuted",
            "payload": {
                "bucket_id": hexid(1),
                "signer_id": hexid(7),
                "collateral_source": hexid(collateral_source),
                "signer_token_recipient": hexid(9),
                "executor": hexid(8),
                "position_id": hexid(6),
                "position_recipient": hexid(8),
                "call_token_recipient": hexid(call_recipient),
                "write_amount": "1000",
                "gross_premium": "500",
                "fee": "50",
                "net_premium": "450",
                "range_start": "0",
                "range_end": "1000",
                "nonce": "1",
            }
        })
    }

    #[test]
    fn classify_fill_scopes_to_our_vault_and_sides() {
        let vault = oid(9);
        // Our V1 buy: collateral from the vault, tokens to the vault.
        let ev = canned_event(10, canned_write_executed(9, 9));
        let f = classify_fill(&ev, vault).unwrap();
        assert_eq!(f.side, FillSide::Bought);
        assert_eq!((f.amount, f.premium), (1000, 500));
        // Our V2 write: collateral from the vault, tokens to retail →
        // premium is the NET the vault receives.
        let ev = canned_event(11, canned_write_executed(9, 3));
        let f = classify_fill(&ev, vault).unwrap();
        assert_eq!(f.side, FillSide::Wrote);
        assert_eq!(f.premium, 450);
        // Someone else's fill: not ours.
        let ev = canned_event(12, canned_write_executed(4, 4));
        assert_eq!(classify_fill(&ev, vault), None);
    }

    #[test]
    fn fill_replay_attributes_spread_and_cursor_survives_rerun() {
        let vault = oid(9);
        let dir = std::env::temp_dir();
        let cursor_path = dir.join(format!("mm-desk-fill-cursor-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&cursor_path);

        // Two canned fills: a WS-RFQ buy and an auction win observed as
        // a ticket redemption (TvBidRedeemed ⋈ TvBidPlaced).
        let ev1 = canned_event(100, canned_write_executed(9, 9));
        let placed = canned_event(90, serde_json::json!({
            "type": "TvBidPlaced",
            "payload": {
                "vault_id": hexid(9),
                "ticket_id": hexid(2),
                "auction_id": hexid(3),
                "bucket_id": hexid(1),
                "escrow_amount": "900",
                "win_type": "0x1::c::C",
                "win_amount": "2000",
                "is_put": false,
            }
        }));
        let ev2 = canned_event(101, serde_json::json!({
            "type": "TvBidRedeemed",
            "payload": {
                "vault_id": hexid(9),
                "ticket_id": hexid(2),
                "auction_id": hexid(3),
                "win_type": "0x1::c::C",
                "win_amount": "2000",
            }
        }));
        let placed_by_ticket: HashMap<_, _> = match &placed.event {
            ChainEvent::TvBidPlaced(b) => HashMap::from([(b.ticket_id, b.clone())]),
            other => panic!("unexpected {other:?}"),
        };
        let win = classify_ticket_win(&ev2, vault, &placed_by_ticket).unwrap();
        assert_eq!((win.amount, win.premium, win.side), (2000, 900, FillSide::Bought));
        assert_eq!(win.bucket_id, oid(1));
        // A redemption for someone else's vault is not ours; a missing
        // BidPlaced join can't be attributed.
        assert_eq!(classify_ticket_win(&ev2, oid(4), &placed_by_ticket), None);
        assert_eq!(classify_ticket_win(&ev2, vault, &HashMap::new()), None);
        let fills: Vec<(DetectedFill, f64)> = vec![
            // Model fair 600 vs 500 paid → spread +100.
            (classify_fill(&ev1, vault).unwrap(), 600.0),
            // Model fair 850 vs 900 paid → spread −50.
            (win, 850.0),
        ];

        let mut book = Book::new(0, None);
        let mut cursor = FillCursor::default();
        let applied = apply_fills(&mut book, &mut cursor, &cursor_path, &fills, 1);
        assert_eq!(applied, 2);
        assert!((book.pnl.spread - 50.0).abs() < 1e-9);
        assert_eq!(cursor.last_sequence, 101);

        // Restart: reload the persisted cursor, replay the same batch —
        // nothing double-counts.
        let mut cursor2 = FillCursor::load(&cursor_path).expect("cursor persisted");
        assert_eq!(cursor2.last_sequence, 101);
        let applied = apply_fills(&mut book, &mut cursor2, &cursor_path, &fills, 2);
        assert_eq!(applied, 0);
        assert!((book.pnl.spread - 50.0).abs() < 1e-9);
        let _ = std::fs::remove_file(&cursor_path);
    }

    #[test]
    fn pnl_lines_accumulate_and_append_jsonl() {
        let path = std::env::temp_dir().join(format!("mm-desk-pnl-test-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut b = Book::new(0, Some(path.clone()));
        b.record_pnl(PnlLine::Spread, 10.0, "fill", 1);
        b.record_pnl(PnlLine::Theta, -3.0, "decay", 2);
        assert_eq!(b.pnl.spread, 10.0);
        assert_eq!(b.pnl.theta, -3.0);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("\"line\":\"spread\""));
        let _ = std::fs::remove_file(&path);
    }
}
