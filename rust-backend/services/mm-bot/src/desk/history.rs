//! Desk time-series history on TimescaleDB (SO-349).
//!
//! A recorder task samples the same snapshot `/desk/state` serves
//! ([`super::state::snapshot`]) every `[desk.history].sample_secs` and
//! appends rows to the env's Tiger Data TimescaleDB — the instance
//! price-charting already uses; tables are `desk_`-prefixed and the
//! migration version is date-stamped so both services' embedded
//! migrations share `__diesel_schema_migrations` without collisions.
//! The recorder also mirrors the P&L-attribution JSONL ledger into
//! `desk_pnl_lines`, exactly-once via a byte offset advanced in the same
//! transaction as each batch (so the full ledger backfills on the first
//! boot with a DB configured).
//!
//! `GET /desk/history?series=…` (served by `crate::server`) reads the
//! tables back with `time_bucket` downsampling.
//!
//! The DB is never load-bearing for trading: the pool is built
//! unchecked (no eager connection), migrations run in the recorder with
//! retry, and insert/query failures log + count metrics instead of
//! propagating.

use std::io::{Read as _, Seek as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel::sql_types::{BigInt, Bool, Double, Nullable, Text, Timestamptz};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};
use serde::{Deserialize, Serialize};

use super::book::{Reservation, ReservationState};
use super::state::DeskStateDto;
use super::Desk;

pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("src/desk/history_migrations");

type DbPool = Pool<ConnectionManager<PgConnection>>;

// ── config ─────────────────────────────────────────────────────────────

/// `[desk.history]`. The URL is a secret: the deployed path is the
/// `DESK_DATABASE_URL` env var (compose maps it from the rendered
/// `CHART_DATABASE_URL` — same Tiger instance as price-charting); the
/// TOML field exists for local dev. Empty both ways ⇒ history disabled.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HistoryConfig {
    pub database_url: String,
    pub db_pool_size: u32,
    /// Recorder cadence, seconds.
    pub sample_secs: u64,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self { database_url: String::new(), db_pool_size: 2, sample_secs: 60 }
    }
}

impl HistoryConfig {
    /// Config value, else `DESK_DATABASE_URL` from the environment.
    pub fn effective_database_url(&self) -> Option<String> {
        if !self.database_url.trim().is_empty() {
            return Some(self.database_url.trim().to_string());
        }
        std::env::var("DESK_DATABASE_URL").ok().filter(|s| !s.trim().is_empty())
    }
}

// ── schema (hand-written, kept in sync with history_migrations/) ───────

diesel::table! {
    desk_snapshots (time) {
        time                  -> Timestamptz,
        nav                   -> Double,
        deployed              -> Double,
        reserved              -> Double,
        net_vega_per_volpt    -> Double,
        theta_cost_per_day    -> Double,
        premium_util          -> Double,
        vega_util             -> Double,
        theta_util            -> Double,
        premium_lt90          -> Double,
        premium_90_110        -> Double,
        premium_gt110         -> Double,
        naked_units           -> Int8,
        funding_rate_annual   -> Double,
        kill_switch           -> Bool,
        stress_blocked        -> Bool,
        worst_stress_drawdown -> Nullable<Double>,
    }
}

diesel::table! {
    desk_symbol_samples (time, symbol) {
        time              -> Timestamptz,
        symbol            -> Text,
        spot              -> Nullable<Double>,
        book_delta_units  -> Double,
        hedge_units       -> Double,
        net_delta_units   -> Double,
        band_units        -> Nullable<Double>,
    }
}

diesel::table! {
    desk_venue_samples (time, venue, symbol) {
        time            -> Timestamptz,
        venue           -> Text,
        symbol          -> Text,
        position_units  -> Double,
        funding_annual  -> Double,
        margin_headroom -> Double,
        notional        -> Double,
        realized_pnl    -> Double,
    }
}

diesel::table! {
    desk_expiry_samples (time, expiry_ms) {
        time          -> Timestamptz,
        expiry_ms     -> Int8,
        premium       -> Double,
        delta_units   -> Double,
        gamma_units   -> Double,
        vega          -> Double,
        theta_per_day -> Double,
    }
}

diesel::table! {
    desk_pnl_lines (time, ts_ms, line) {
        time   -> Timestamptz,
        ts_ms  -> Int8,
        line   -> Text,
        amount -> Double,
        note   -> Text,
    }
}

diesel::table! {
    desk_pnl_ingest (id) {
        id          -> Int2,
        byte_offset -> Int8,
    }
}

diesel::table! {
    desk_rfq_outcomes (time, request_id) {
        time                -> Timestamptz,
        request_id          -> Text,
        source              -> Text,
        auction_id          -> Nullable<Text>,
        symbol              -> Nullable<Text>,
        option_type         -> Text,
        side                -> Text,
        strike              -> Double,
        expiry_ms           -> Int8,
        size_units          -> Int8,
        spot_at_request     -> Nullable<Double>,
        model_fair          -> Nullable<Double>,
        surface_vol         -> Nullable<Double>,
        quoted_premium      -> Nullable<Int8>,
        valid_until_ms      -> Nullable<Int8>,
        nonce               -> Nullable<Int8>,
        response_latency_ms -> Nullable<Double>,
        outcome             -> Text,
        outcome_at          -> Nullable<Timestamptz>,
        reason              -> Nullable<Text>,
        fill_sequence       -> Nullable<Int8>,
    }
}

diesel::table! {
    desk_reservations (request_id) {
        request_id     -> Text,
        nonce          -> Nullable<Int8>,
        amount         -> Int8,
        is_put         -> Bool,
        expiry_ms      -> Int8,
        exercise_cash  -> Double,
        hedge_notional -> Double,
        quoted_at_ms   -> Int8,
        expires_ms     -> Int8,
        state          -> Text,
        state_at_ms    -> Int8,
        updated_at     -> Timestamptz,
    }
}

// ── row types ──────────────────────────────────────────────────────────

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = desk_snapshots)]
struct SnapshotRow {
    time: DateTime<Utc>,
    nav: f64,
    deployed: f64,
    reserved: f64,
    net_vega_per_volpt: f64,
    theta_cost_per_day: f64,
    premium_util: f64,
    vega_util: f64,
    theta_util: f64,
    premium_lt90: f64,
    premium_90_110: f64,
    premium_gt110: f64,
    naked_units: i64,
    funding_rate_annual: f64,
    kill_switch: bool,
    stress_blocked: bool,
    worst_stress_drawdown: Option<f64>,
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = desk_symbol_samples)]
struct SymbolRow {
    time: DateTime<Utc>,
    symbol: String,
    spot: Option<f64>,
    book_delta_units: f64,
    hedge_units: f64,
    net_delta_units: f64,
    band_units: Option<f64>,
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = desk_venue_samples)]
struct VenueRow {
    time: DateTime<Utc>,
    venue: String,
    symbol: String,
    position_units: f64,
    funding_annual: f64,
    margin_headroom: f64,
    notional: f64,
    realized_pnl: f64,
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = desk_expiry_samples)]
struct ExpiryRow {
    time: DateTime<Utc>,
    expiry_ms: i64,
    premium: f64,
    delta_units: f64,
    gamma_units: f64,
    vega: f64,
    theta_per_day: f64,
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = desk_pnl_lines)]
struct PnlLineRow {
    time: DateTime<Utc>,
    ts_ms: i64,
    line: String,
    amount: f64,
    note: String,
}

/// One JSONL record as `Book::record_pnl` appends it.
#[derive(Deserialize)]
struct PnlJsonlRecord {
    ts_ms: u64,
    line: String,
    amount: f64,
    #[serde(default)]
    note: String,
}

/// One RFQ-funnel row (SO-425, doc 08 §3.1): a WS RFQ decision or a
/// vault-funded auction bid, inserted as `declined` (terminal) or
/// `quoted` (pending). Pending rows are swept to `expired` by the
/// recorder after the quote TTL and upgraded to `filled` by the fill
/// poller — a detected fill is ground truth and always wins.
#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = desk_rfq_outcomes)]
pub struct RfqOutcomeRow {
    pub time: DateTime<Utc>,
    pub request_id: String,
    pub source: String,
    pub auction_id: Option<String>,
    pub symbol: Option<String>,
    pub option_type: String,
    pub side: String,
    pub strike: f64,
    pub expiry_ms: i64,
    pub size_units: i64,
    pub spot_at_request: Option<f64>,
    pub model_fair: Option<f64>,
    pub surface_vol: Option<f64>,
    pub quoted_premium: Option<i64>,
    pub valid_until_ms: Option<i64>,
    pub nonce: Option<i64>,
    pub response_latency_ms: Option<f64>,
    pub outcome: String,
    pub outcome_at: Option<DateTime<Utc>>,
    pub reason: Option<String>,
    pub fill_sequence: Option<i64>,
}

impl RfqOutcomeRow {
    /// A base row at request-received time; callers fill the rest via
    /// the builder methods below before recording.
    #[allow(clippy::too_many_arguments)]
    pub fn base(
        request_id: String,
        source: &str,
        is_put: bool,
        side: &str,
        strike: f64,
        expiry_ms: u64,
        size_units: u64,
        received_at_ms: u64,
    ) -> Self {
        Self {
            time: ms_to_dt(received_at_ms as i64),
            request_id,
            source: source.to_string(),
            auction_id: None,
            symbol: None,
            option_type: if is_put { "put" } else { "call" }.to_string(),
            side: side.to_string(),
            strike,
            expiry_ms: expiry_ms as i64,
            size_units: size_units as i64,
            spot_at_request: None,
            model_fair: None,
            surface_vol: None,
            quoted_premium: None,
            valid_until_ms: None,
            nonce: None,
            response_latency_ms: None,
            outcome: String::new(),
            outcome_at: None,
            reason: None,
            fill_sequence: None,
        }
    }

    /// Terminal decline at `at_ms`.
    pub fn declined(mut self, reason: String, at_ms: u64) -> Self {
        self.outcome = "declined".to_string();
        self.outcome_at = Some(ms_to_dt(at_ms as i64));
        self.reason = Some(reason);
        self
    }

    /// Pending signed quote; the sweep/fill poller supplies the terminal.
    pub fn quoted(
        mut self,
        premium: u64,
        model_fair: f64,
        surface_vol: f64,
        valid_until_ms: u64,
        nonce: Option<u64>,
    ) -> Self {
        self.outcome = "quoted".to_string();
        self.quoted_premium = Some(premium as i64);
        self.model_fair = Some(model_fair);
        self.surface_vol = Some(surface_vol);
        self.valid_until_ms = Some(valid_until_ms as i64);
        self.nonce = nonce.map(|n| n as i64);
        self
    }
}

/// One durable reservation row (SO-444) — the DB shape of
/// [`Reservation`]; the latest state per request id.
#[derive(Insertable, Queryable, Debug, Clone)]
#[diesel(table_name = desk_reservations)]
struct ReservationRow {
    request_id: String,
    nonce: Option<i64>,
    amount: i64,
    is_put: bool,
    expiry_ms: i64,
    exercise_cash: f64,
    hedge_notional: f64,
    quoted_at_ms: i64,
    expires_ms: i64,
    state: String,
    state_at_ms: i64,
    updated_at: DateTime<Utc>,
}

impl From<&Reservation> for ReservationRow {
    fn from(r: &Reservation) -> Self {
        Self {
            request_id: r.key.clone(),
            nonce: r.nonce.map(|n| n as i64),
            amount: r.amount as i64,
            is_put: r.is_put,
            expiry_ms: r.expiry_ms as i64,
            exercise_cash: r.exercise_cash,
            hedge_notional: r.hedge_notional,
            quoted_at_ms: r.quoted_at_ms as i64,
            expires_ms: r.expires_ms as i64,
            state: r.state.as_str().to_string(),
            state_at_ms: r.state_at_ms as i64,
            updated_at: Utc::now(),
        }
    }
}

impl ReservationRow {
    fn into_reservation(self) -> Option<Reservation> {
        Some(Reservation {
            key: self.request_id,
            nonce: self.nonce.map(|n| n as u64),
            amount: self.amount.max(0) as u64,
            is_put: self.is_put,
            expiry_ms: self.expiry_ms.max(0) as u64,
            exercise_cash: self.exercise_cash,
            hedge_notional: self.hedge_notional,
            quoted_at_ms: self.quoted_at_ms.max(0) as u64,
            expires_ms: self.expires_ms.max(0) as u64,
            state: ReservationState::parse(&self.state)?,
            state_at_ms: self.state_at_ms.max(0) as u64,
        })
    }
}

/// How a detected fill joins back to its funnel row.
#[derive(Debug, Clone)]
pub enum RfqFillKey {
    /// WS quote: the signed quote nonce (`WriteExecuted.nonce`).
    Nonce(u64),
    /// Auction bid: the `BidTicket` id hex used as the row's request_id.
    Request(String),
}

// ── the handle ─────────────────────────────────────────────────────────

pub struct History {
    pool: DbPool,
    cfg: HistoryConfig,
    /// Set once migrations have run; queries 503 before that.
    ready: AtomicBool,
}

impl History {
    /// Build the (lazy) pool. Never touches the network — the recorder
    /// runs migrations with retry so a down DB can't block bot startup.
    pub fn connect(cfg: &HistoryConfig, database_url: &str) -> Arc<Self> {
        let manager = ConnectionManager::<PgConnection>::new(database_url);
        let pool = Pool::builder()
            .max_size(cfg.db_pool_size.max(1))
            .connection_timeout(Duration::from_secs(5))
            .build_unchecked(manager);
        Arc::new(Self { pool, cfg: cfg.clone(), ready: AtomicBool::new(false) })
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    fn conn(&self) -> Result<diesel::r2d2::PooledConnection<ConnectionManager<PgConnection>>> {
        self.pool.get().context("checking out a history DB connection")
    }

    fn run_migrations(&self) -> Result<()> {
        let mut conn = self.conn()?;
        conn.run_pending_migrations(MIGRATIONS)
            .map_err(|e| anyhow::anyhow!("running desk history migrations: {e}"))?;
        Ok(())
    }

    // ── RFQ outcome funnel (SO-425) ───────────────────────────────────
    //
    // All funnel writes are fire-and-forget: spawned off the hot path,
    // failures counted + logged, never propagated — the DB is not
    // load-bearing for trading (same contract as the recorder).

    /// Insert one funnel row.
    pub fn record_rfq(self: &Arc<Self>, row: RfqOutcomeRow) {
        let h = Arc::clone(self);
        tokio::spawn(async move {
            let res = tokio::task::spawn_blocking(move || h.insert_rfq(row)).await;
            if let Ok(Err(e)) | Err(e) = res.map_err(anyhow::Error::from) {
                metrics::counter!("mm_desk_history_failures_total", "op" => "rfq")
                    .increment(1);
                tracing::warn!(error = %format!("{e:#}"), "rfq outcome insert failed");
            }
        });
    }

    /// Upgrade a `quoted` (or already swept `expired`) row to `filled`.
    pub fn record_rfq_filled(self: &Arc<Self>, key: RfqFillKey, fill_sequence: u64, at_ms: u64) {
        let h = Arc::clone(self);
        tokio::spawn(async move {
            let res = tokio::task::spawn_blocking(move || {
                h.mark_rfq_filled(&key, fill_sequence as i64, ms_to_dt(at_ms as i64))
            })
            .await;
            if let Ok(Err(e)) | Err(e) = res.map_err(anyhow::Error::from) {
                metrics::counter!("mm_desk_history_failures_total", "op" => "rfq")
                    .increment(1);
                tracing::warn!(error = %format!("{e:#}"), "rfq fill upgrade failed");
            }
        });
    }

    /// Move a still-`quoted` row to a terminal outcome (auction ticket
    /// burns). A row the fill poller already upgraded is left alone.
    pub fn record_rfq_terminal(
        self: &Arc<Self>,
        request_id: String,
        outcome: &'static str,
        reason: &'static str,
        at_ms: u64,
    ) {
        let h = Arc::clone(self);
        tokio::spawn(async move {
            let res = tokio::task::spawn_blocking(move || {
                h.mark_rfq_terminal(&request_id, outcome, reason, ms_to_dt(at_ms as i64))
            })
            .await;
            if let Ok(Err(e)) | Err(e) = res.map_err(anyhow::Error::from) {
                metrics::counter!("mm_desk_history_failures_total", "op" => "rfq")
                    .increment(1);
                tracing::warn!(error = %format!("{e:#}"), "rfq terminal update failed");
            }
        });
    }

    fn insert_rfq(&self, row: RfqOutcomeRow) -> Result<()> {
        let mut conn = self.conn()?;
        diesel::insert_into(desk_rfq_outcomes::table).values(&row).execute(&mut conn)?;
        Ok(())
    }

    // ── durable reservations (SO-444) ─────────────────────────────────

    /// Persist reservation transitions (oldest first), fire-and-forget
    /// like the funnel: the DB is not load-bearing for the quote path,
    /// a failed write is counted + logged (and `/desk/state` still shows
    /// the in-memory ledger).
    pub fn record_reservations(self: &Arc<Self>, transitions: Vec<Reservation>) {
        if transitions.is_empty() {
            return;
        }
        let h = Arc::clone(self);
        tokio::spawn(async move {
            let res = tokio::task::spawn_blocking(move || h.upsert_reservations(&transitions)).await;
            if let Ok(Err(e)) | Err(e) = res.map_err(anyhow::Error::from) {
                metrics::counter!("mm_desk_history_failures_total", "op" => "reservation")
                    .increment(1);
                tracing::warn!(error = %format!("{e:#}"), "reservation transition persist failed");
            }
        });
    }

    /// Upsert in order. A live row (`quoted`/`accepted`) may move
    /// anywhere; a terminal row only ever upgrades to `filled` — the
    /// chain fill is ground truth over an earlier expiry/revert.
    fn upsert_reservations(&self, transitions: &[Reservation]) -> Result<()> {
        use desk_reservations::dsl as t;
        use diesel::query_dsl::methods::FilterDsl;
        let mut conn = self.conn()?;
        conn.transaction::<_, anyhow::Error, _>(|conn| {
            for r in transitions {
                let row = ReservationRow::from(r);
                diesel::insert_into(t::desk_reservations)
                    .values(&row)
                    .on_conflict(t::request_id)
                    .do_update()
                    .set((
                        t::nonce.eq(&row.nonce),
                        t::state.eq(&row.state),
                        t::state_at_ms.eq(row.state_at_ms),
                        t::expires_ms.eq(row.expires_ms),
                        t::updated_at.eq(row.updated_at),
                    ))
                    .filter(
                        t::state
                            .eq_any(["quoted", "accepted"])
                            .or(diesel::dsl::sql::<Bool>("excluded.state = 'filled'")),
                    )
                    .execute(conn)?;
            }
            Ok(())
        })
        .context("upserting desk reservations")?;
        metrics::counter!("mm_desk_reservation_transitions_total").increment(transitions.len() as u64);
        Ok(())
    }

    /// Every still-live reservation, for boot reconstruction. Blocking
    /// (call from `spawn_blocking`). Runs the pending migrations first
    /// so a first boot with this schema does not fail the read.
    pub fn load_live_reservations(&self) -> Result<Vec<Reservation>> {
        use desk_reservations::dsl as t;
        self.run_migrations()?;
        let mut conn = self.conn()?;
        let rows = t::desk_reservations
            .filter(t::state.eq_any(["quoted", "accepted"]))
            .load::<ReservationRow>(&mut conn)
            .context("loading live desk reservations")?;
        Ok(rows.into_iter().filter_map(ReservationRow::into_reservation).collect())
    }

    fn mark_rfq_filled(&self, key: &RfqFillKey, fill_sequence: i64, at: DateTime<Utc>) -> Result<()> {
        use desk_rfq_outcomes::dsl as t;
        let mut conn = self.conn()?;
        let assignments = (
            t::outcome.eq("filled"),
            t::outcome_at.eq(Some(at)),
            t::fill_sequence.eq(Some(fill_sequence)),
        );
        match key {
            RfqFillKey::Nonce(n) => diesel::update(
                t::desk_rfq_outcomes
                    .filter(t::nonce.eq(Some(*n as i64)))
                    .filter(t::outcome.ne("declined")),
            )
            .set(assignments)
            .execute(&mut conn)?,
            RfqFillKey::Request(r) => diesel::update(
                t::desk_rfq_outcomes
                    .filter(t::request_id.eq(r))
                    .filter(t::outcome.ne("declined")),
            )
            .set(assignments)
            .execute(&mut conn)?,
        };
        Ok(())
    }

    fn mark_rfq_terminal(
        &self,
        request_id: &str,
        outcome: &str,
        reason: &str,
        at: DateTime<Utc>,
    ) -> Result<()> {
        use desk_rfq_outcomes::dsl as t;
        let mut conn = self.conn()?;
        diesel::update(
            t::desk_rfq_outcomes
                .filter(t::request_id.eq(request_id))
                .filter(t::outcome.eq("quoted")),
        )
        .set((
            t::outcome.eq(outcome),
            t::outcome_at.eq(Some(at)),
            t::reason.eq(Some(reason)),
        ))
        .execute(&mut conn)?;
        Ok(())
    }

    /// Sweep `quoted` rows whose TTL elapsed (plus a grace window for
    /// fill-detection lag) to `expired`. A late-detected fill still
    /// upgrades a swept row — see `mark_rfq_filled`.
    fn sweep_expired_rfqs(&self, now_ms: i64) -> Result<usize> {
        /// Fill-poller/indexer lag allowance before a live quote is
        /// declared expired.
        const GRACE_MS: i64 = 300_000;
        use desk_rfq_outcomes::dsl as t;
        let mut conn = self.conn()?;
        let n = diesel::update(
            t::desk_rfq_outcomes
                .filter(t::outcome.eq("quoted"))
                .filter(t::valid_until_ms.is_not_null())
                .filter(t::valid_until_ms.lt(Some(now_ms - GRACE_MS))),
        )
        .set((
            t::outcome.eq("expired"),
            t::outcome_at.eq(Some(ms_to_dt(now_ms))),
            t::reason.eq(Some("quote TTL elapsed with no detected fill")),
        ))
        .execute(&mut conn)?;
        Ok(n)
    }

    // ── writes (recorder) ─────────────────────────────────────────────

    fn insert_sample(
        &self,
        snapshot: SnapshotRow,
        symbols: Vec<SymbolRow>,
        venues: Vec<VenueRow>,
        expiries: Vec<ExpiryRow>,
    ) -> Result<()> {
        let mut conn = self.conn()?;
        conn.transaction::<_, anyhow::Error, _>(|conn| {
            diesel::insert_into(desk_snapshots::table).values(&snapshot).execute(conn)?;
            diesel::insert_into(desk_symbol_samples::table).values(&symbols).execute(conn)?;
            diesel::insert_into(desk_venue_samples::table).values(&venues).execute(conn)?;
            diesel::insert_into(desk_expiry_samples::table).values(&expiries).execute(conn)?;
            Ok(())
        })
        .context("inserting desk history sample")?;
        metrics::counter!("mm_desk_history_samples_total").increment(1);
        Ok(())
    }

    /// Insert a JSONL batch and advance the ingest offset atomically.
    fn insert_pnl_batch(&self, rows: Vec<PnlLineRow>, new_offset: i64) -> Result<()> {
        let mut conn = self.conn()?;
        conn.transaction::<_, anyhow::Error, _>(|conn| {
            diesel::insert_into(desk_pnl_lines::table).values(&rows).execute(conn)?;
            diesel::insert_into(desk_pnl_ingest::table)
                .values((desk_pnl_ingest::id.eq(1i16), desk_pnl_ingest::byte_offset.eq(new_offset)))
                .on_conflict(desk_pnl_ingest::id)
                .do_update()
                .set(desk_pnl_ingest::byte_offset.eq(new_offset))
                .execute(conn)?;
            Ok(())
        })
        .context("inserting desk pnl batch")?;
        Ok(())
    }

    fn pnl_ingest_offset(&self) -> Result<i64> {
        let mut conn = self.conn()?;
        Ok(desk_pnl_ingest::table
            .filter(desk_pnl_ingest::id.eq(1i16))
            .select(desk_pnl_ingest::byte_offset)
            .first::<i64>(&mut conn)
            .optional()?
            .unwrap_or(0))
    }

    // ── reads (GET /desk/history) ─────────────────────────────────────

    pub fn query(&self, q: &HistoryQuery) -> Result<HistoryResponse> {
        let (from_ms, to_ms) = q.range();
        let bucket_secs = q.bucket_secs(from_ms, to_ms);
        let points = match q.series {
            Series::Snapshots => self.query_snapshots(bucket_secs, from_ms, to_ms)?,
            Series::Symbols => self.query_symbols(bucket_secs, from_ms, to_ms, q.symbol.as_deref())?,
            Series::Venues => self.query_venues(bucket_secs, from_ms, to_ms, q.venue.as_deref())?,
            Series::Expiries => self.query_expiries(bucket_secs, from_ms, to_ms)?,
            Series::Pnl => self.query_pnl(from_ms, to_ms, q.line.as_deref())?,
        };
        Ok(HistoryResponse { series: q.series, from_ms, to_ms, bucket_secs, points })
    }

    fn query_snapshots(&self, bucket: i64, from_ms: i64, to_ms: i64) -> Result<Vec<serde_json::Value>> {
        #[derive(QueryableByName)]
        struct Row {
            #[diesel(sql_type = Timestamptz)]
            bucket: DateTime<Utc>,
            #[diesel(sql_type = Double)]
            nav: f64,
            #[diesel(sql_type = Double)]
            deployed: f64,
            #[diesel(sql_type = Double)]
            reserved: f64,
            #[diesel(sql_type = Double)]
            net_vega_per_volpt: f64,
            #[diesel(sql_type = Double)]
            theta_cost_per_day: f64,
            #[diesel(sql_type = Double)]
            premium_util: f64,
            #[diesel(sql_type = Double)]
            vega_util: f64,
            #[diesel(sql_type = Double)]
            theta_util: f64,
            #[diesel(sql_type = Double)]
            premium_lt90: f64,
            #[diesel(sql_type = Double)]
            premium_90_110: f64,
            #[diesel(sql_type = Double)]
            premium_gt110: f64,
            #[diesel(sql_type = BigInt)]
            naked_units: i64,
            #[diesel(sql_type = Double)]
            funding_rate_annual: f64,
            #[diesel(sql_type = Bool)]
            kill_switch: bool,
            #[diesel(sql_type = Bool)]
            stress_blocked: bool,
            #[diesel(sql_type = Nullable<Double>)]
            worst_stress_drawdown: Option<f64>,
        }
        let mut conn = self.conn()?;
        let rows = diesel::sql_query(
            "SELECT time_bucket(make_interval(secs => $1), time) AS bucket, \
                    avg(nav) AS nav, avg(deployed) AS deployed, avg(reserved) AS reserved, \
                    avg(net_vega_per_volpt) AS net_vega_per_volpt, \
                    avg(theta_cost_per_day) AS theta_cost_per_day, \
                    avg(premium_util) AS premium_util, avg(vega_util) AS vega_util, \
                    avg(theta_util) AS theta_util, \
                    avg(premium_lt90) AS premium_lt90, avg(premium_90_110) AS premium_90_110, \
                    avg(premium_gt110) AS premium_gt110, \
                    max(naked_units) AS naked_units, \
                    avg(funding_rate_annual) AS funding_rate_annual, \
                    bool_or(kill_switch) AS kill_switch, bool_or(stress_blocked) AS stress_blocked, \
                    max(worst_stress_drawdown) AS worst_stress_drawdown \
             FROM desk_snapshots WHERE time >= $2 AND time < $3 \
             GROUP BY bucket ORDER BY bucket",
        )
        .bind::<BigInt, _>(bucket)
        .bind::<Timestamptz, _>(ms_to_dt(from_ms))
        .bind::<Timestamptz, _>(ms_to_dt(to_ms))
        .load::<Row>(&mut conn)
        .context("querying desk_snapshots")?;
        Ok(rows
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "timeMs": r.bucket.timestamp_millis(),
                    "nav": r.nav,
                    "deployed": r.deployed,
                    "reserved": r.reserved,
                    "netVegaPerVolpt": r.net_vega_per_volpt,
                    "thetaCostPerDay": r.theta_cost_per_day,
                    "premiumUtil": r.premium_util,
                    "vegaUtil": r.vega_util,
                    "thetaUtil": r.theta_util,
                    "premiumByStrikeBucket": [r.premium_lt90, r.premium_90_110, r.premium_gt110],
                    "nakedUnits": r.naked_units,
                    "fundingRateAnnual": r.funding_rate_annual,
                    "killSwitch": r.kill_switch,
                    "stressBlocked": r.stress_blocked,
                    "worstStressDrawdown": r.worst_stress_drawdown,
                })
            })
            .collect())
    }

    fn query_symbols(
        &self,
        bucket: i64,
        from_ms: i64,
        to_ms: i64,
        symbol: Option<&str>,
    ) -> Result<Vec<serde_json::Value>> {
        #[derive(QueryableByName)]
        struct Row {
            #[diesel(sql_type = Timestamptz)]
            bucket: DateTime<Utc>,
            #[diesel(sql_type = Text)]
            symbol: String,
            #[diesel(sql_type = Nullable<Double>)]
            spot: Option<f64>,
            #[diesel(sql_type = Double)]
            book_delta_units: f64,
            #[diesel(sql_type = Double)]
            hedge_units: f64,
            #[diesel(sql_type = Double)]
            net_delta_units: f64,
            #[diesel(sql_type = Nullable<Double>)]
            band_units: Option<f64>,
        }
        let mut conn = self.conn()?;
        let rows = diesel::sql_query(
            "SELECT time_bucket(make_interval(secs => $1), time) AS bucket, symbol, \
                    avg(spot) AS spot, avg(book_delta_units) AS book_delta_units, \
                    avg(hedge_units) AS hedge_units, \
                    avg(net_delta_units) AS net_delta_units, avg(band_units) AS band_units \
             FROM desk_symbol_samples \
             WHERE time >= $2 AND time < $3 AND ($4 = '' OR symbol = $4) \
             GROUP BY bucket, symbol ORDER BY bucket",
        )
        .bind::<BigInt, _>(bucket)
        .bind::<Timestamptz, _>(ms_to_dt(from_ms))
        .bind::<Timestamptz, _>(ms_to_dt(to_ms))
        .bind::<Text, _>(symbol.unwrap_or(""))
        .load::<Row>(&mut conn)
        .context("querying desk_symbol_samples")?;
        Ok(rows
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "timeMs": r.bucket.timestamp_millis(),
                    "symbol": r.symbol,
                    "spot": r.spot,
                    "bookDeltaUnits": r.book_delta_units,
                    "hedgeUnits": r.hedge_units,
                    "netDeltaUnits": r.net_delta_units,
                    "bandUnits": r.band_units,
                })
            })
            .collect())
    }

    fn query_venues(
        &self,
        bucket: i64,
        from_ms: i64,
        to_ms: i64,
        venue: Option<&str>,
    ) -> Result<Vec<serde_json::Value>> {
        #[derive(QueryableByName)]
        struct Row {
            #[diesel(sql_type = Timestamptz)]
            bucket: DateTime<Utc>,
            #[diesel(sql_type = Text)]
            venue: String,
            #[diesel(sql_type = Text)]
            symbol: String,
            #[diesel(sql_type = Double)]
            position_units: f64,
            #[diesel(sql_type = Double)]
            funding_annual: f64,
            #[diesel(sql_type = Double)]
            margin_headroom: f64,
            #[diesel(sql_type = Double)]
            notional: f64,
            #[diesel(sql_type = Double)]
            realized_pnl: f64,
        }
        let mut conn = self.conn()?;
        let rows = diesel::sql_query(
            "SELECT time_bucket(make_interval(secs => $1), time) AS bucket, venue, symbol, \
                    avg(position_units) AS position_units, avg(funding_annual) AS funding_annual, \
                    min(margin_headroom) AS margin_headroom, avg(notional) AS notional, \
                    last(realized_pnl, time) AS realized_pnl \
             FROM desk_venue_samples \
             WHERE time >= $2 AND time < $3 AND ($4 = '' OR venue = $4) \
             GROUP BY bucket, venue, symbol ORDER BY bucket",
        )
        .bind::<BigInt, _>(bucket)
        .bind::<Timestamptz, _>(ms_to_dt(from_ms))
        .bind::<Timestamptz, _>(ms_to_dt(to_ms))
        .bind::<Text, _>(venue.unwrap_or(""))
        .load::<Row>(&mut conn)
        .context("querying desk_venue_samples")?;
        Ok(rows
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "timeMs": r.bucket.timestamp_millis(),
                    "venue": r.venue,
                    "symbol": r.symbol,
                    "positionUnits": r.position_units,
                    "fundingRateAnnual": r.funding_annual,
                    "marginHeadroom": r.margin_headroom,
                    "notional": r.notional,
                    "realizedPnl": r.realized_pnl,
                })
            })
            .collect())
    }

    fn query_expiries(&self, bucket: i64, from_ms: i64, to_ms: i64) -> Result<Vec<serde_json::Value>> {
        #[derive(QueryableByName)]
        struct Row {
            #[diesel(sql_type = Timestamptz)]
            bucket: DateTime<Utc>,
            #[diesel(sql_type = BigInt)]
            expiry_ms: i64,
            #[diesel(sql_type = Double)]
            premium: f64,
            #[diesel(sql_type = Double)]
            delta_units: f64,
            #[diesel(sql_type = Double)]
            gamma_units: f64,
            #[diesel(sql_type = Double)]
            vega: f64,
            #[diesel(sql_type = Double)]
            theta_per_day: f64,
        }
        let mut conn = self.conn()?;
        let rows = diesel::sql_query(
            "SELECT time_bucket(make_interval(secs => $1), time) AS bucket, expiry_ms, \
                    avg(premium) AS premium, avg(delta_units) AS delta_units, \
                    avg(gamma_units) AS gamma_units, avg(vega) AS vega, \
                    avg(theta_per_day) AS theta_per_day \
             FROM desk_expiry_samples WHERE time >= $2 AND time < $3 \
             GROUP BY bucket, expiry_ms ORDER BY bucket",
        )
        .bind::<BigInt, _>(bucket)
        .bind::<Timestamptz, _>(ms_to_dt(from_ms))
        .bind::<Timestamptz, _>(ms_to_dt(to_ms))
        .load::<Row>(&mut conn)
        .context("querying desk_expiry_samples")?;
        Ok(rows
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "timeMs": r.bucket.timestamp_millis(),
                    "expiryMs": r.expiry_ms,
                    "premium": r.premium,
                    "deltaUnits": r.delta_units,
                    "gammaUnits": r.gamma_units,
                    "vega": r.vega,
                    "thetaPerDay": r.theta_per_day,
                })
            })
            .collect())
    }

    fn query_pnl(&self, from_ms: i64, to_ms: i64, line: Option<&str>) -> Result<Vec<serde_json::Value>> {
        let mut conn = self.conn()?;
        let mut q = desk_pnl_lines::table
            .filter(desk_pnl_lines::time.ge(ms_to_dt(from_ms)))
            .filter(desk_pnl_lines::time.lt(ms_to_dt(to_ms)))
            .into_boxed();
        if let Some(line) = line {
            q = q.filter(desk_pnl_lines::line.eq(line.to_string()));
        }
        let rows = q
            .order(desk_pnl_lines::ts_ms.asc())
            .limit(5000)
            .select((
                desk_pnl_lines::ts_ms,
                desk_pnl_lines::line,
                desk_pnl_lines::amount,
                desk_pnl_lines::note,
            ))
            .load::<(i64, String, f64, String)>(&mut conn)
            .context("querying desk_pnl_lines")?;
        Ok(rows
            .into_iter()
            .map(|(ts_ms, line, amount, note)| {
                serde_json::json!({ "timeMs": ts_ms, "line": line, "amount": amount, "note": note })
            })
            .collect())
    }
}

// ── query params / response ────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Series {
    Snapshots,
    Symbols,
    Venues,
    Expiries,
    Pnl,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryQuery {
    pub series: Series,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    pub bucket_secs: Option<i64>,
    pub symbol: Option<String>,
    pub venue: Option<String>,
    /// `pnl` series only: filter to one attribution line.
    pub line: Option<String>,
}

impl HistoryQuery {
    /// `[from, to)` with a default of the last 24h.
    fn range(&self) -> (i64, i64) {
        let now = chrono::Utc::now().timestamp_millis();
        let to = self.to_ms.unwrap_or(now).min(now + 60_000);
        let from = self.from_ms.unwrap_or(to - 24 * 3_600_000).min(to);
        (from, to)
    }

    /// Requested bucket clamped to ≥15s and to at most ~1000 points.
    fn bucket_secs(&self, from_ms: i64, to_ms: i64) -> i64 {
        let span_secs = ((to_ms - from_ms) / 1000).max(1);
        let floor = (span_secs / 1000).max(15);
        self.bucket_secs.unwrap_or((span_secs / 720).max(60)).max(floor)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryResponse {
    pub series: Series,
    pub from_ms: i64,
    pub to_ms: i64,
    /// 0 for un-bucketed series (`pnl`).
    pub bucket_secs: i64,
    pub points: Vec<serde_json::Value>,
}

pub(crate) fn ms_to_dt(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().unwrap_or_else(Utc::now)
}

// ── recorder ───────────────────────────────────────────────────────────

/// Spawn the recorder: migrations (with retry), then a sample +
/// JSONL-mirror pass every `sample_secs`.
pub fn spawn_recorder(history: Arc<History>, desk: Arc<Desk>, network: String) {
    tokio::spawn(async move {
        // Migrations with retry — the DB must never gate bot startup.
        loop {
            let h = Arc::clone(&history);
            let res = tokio::task::spawn_blocking(move || h.run_migrations()).await;
            match res {
                Ok(Ok(())) => break,
                Ok(Err(e)) => {
                    tracing::warn!(error = %format!("{e:#}"), "desk history migrations failed; retrying in 30s");
                }
                Err(e) => {
                    tracing::warn!(error = %e.to_string(), "desk history migration task panicked; retrying in 30s");
                }
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        history.ready.store(true, Ordering::SeqCst);
        tracing::info!("desk history DB ready (migrations applied)");

        let pnl_path = std::path::PathBuf::from(&desk.cfg.pnl_jsonl_path);
        let mut ticker =
            tokio::time::interval(Duration::from_secs(history.cfg.sample_secs.max(15)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let dto = super::state::snapshot(&desk, &network).await;
            let (snapshot, symbols, venues, expiries) = rows_from_snapshot(&dto);
            {
                let h = Arc::clone(&history);
                let res = tokio::task::spawn_blocking(move || {
                    h.insert_sample(snapshot, symbols, venues, expiries)
                })
                .await;
                if let Ok(Err(e)) | Err(e) = res.map_err(anyhow::Error::from) {
                    metrics::counter!("mm_desk_history_failures_total", "op" => "sample")
                        .increment(1);
                    tracing::warn!(error = %format!("{e:#}"), "desk history sample insert failed");
                }
            }
            // Mirror new JSONL P&L records.
            if let Err(e) = ingest_pnl_jsonl(&history, &pnl_path).await {
                metrics::counter!("mm_desk_history_failures_total", "op" => "pnl").increment(1);
                tracing::warn!(error = %format!("{e:#}"), "desk pnl jsonl ingest failed");
            }
            // Sweep TTL-elapsed quotes in the RFQ funnel to `expired`.
            {
                let h = Arc::clone(&history);
                let now_ms = Utc::now().timestamp_millis();
                let res =
                    tokio::task::spawn_blocking(move || h.sweep_expired_rfqs(now_ms)).await;
                if let Ok(Err(e)) | Err(e) = res.map_err(anyhow::Error::from) {
                    metrics::counter!("mm_desk_history_failures_total", "op" => "rfq")
                        .increment(1);
                    tracing::warn!(error = %format!("{e:#}"), "rfq expiry sweep failed");
                }
            }
        }
    });
}

/// Map one `/desk/state` snapshot into the four sample-row shapes.
fn rows_from_snapshot(
    dto: &DeskStateDto,
) -> (SnapshotRow, Vec<SymbolRow>, Vec<VenueRow>, Vec<ExpiryRow>) {
    let time = ms_to_dt(dto.generated_at_ms as i64);
    let snapshot = SnapshotRow {
        time,
        nav: dto.exposure.nav,
        deployed: dto.exposure.premium_deployed,
        reserved: dto.exposure.reserved,
        net_vega_per_volpt: dto.exposure.net_vega_per_volpt,
        theta_cost_per_day: dto.exposure.theta_cost_per_day,
        premium_util: dto.utilization.premium,
        vega_util: dto.utilization.vega,
        theta_util: dto.utilization.theta,
        premium_lt90: dto.exposure.premium_by_strike_bucket[0],
        premium_90_110: dto.exposure.premium_by_strike_bucket[1],
        premium_gt110: dto.exposure.premium_by_strike_bucket[2],
        naked_units: dto.naked_written_units as i64,
        funding_rate_annual: dto.funding_rate_annual,
        kill_switch: dto.exposure.kill_switch,
        stress_blocked: dto.exposure.stress_blocked,
        worst_stress_drawdown: dto.stress.as_ref().map(|s| s.worst_drawdown),
    };
    let symbols = dto
        .hedge
        .by_symbol
        .iter()
        .map(|s| SymbolRow {
            time,
            symbol: s.symbol.clone(),
            spot: dto.markets.iter().find(|m| m.symbol == s.symbol).and_then(|m| m.spot),
            book_delta_units: s.book_delta_units,
            hedge_units: s.hedge_units,
            net_delta_units: s.net_units,
            band_units: s.band_units,
        })
        .collect();
    let venues = dto
        .hedge
        .venues
        .iter()
        .filter(|v| v.read_ok)
        .map(|v| VenueRow {
            time,
            venue: v.name.clone(),
            symbol: v.symbol.clone(),
            position_units: v.position_units,
            funding_annual: v.funding_rate_annual,
            margin_headroom: v.margin_headroom,
            notional: v.notional,
            realized_pnl: v.realized_pnl,
        })
        .collect();
    // Union of the greeks expiries and the premium-concentration expiries.
    let mut expiry_keys: Vec<u64> = dto
        .greeks
        .by_expiry
        .iter()
        .map(|e| e.expiry_ms)
        .chain(dto.exposure.premium_by_expiry.keys().copied())
        .collect();
    expiry_keys.sort_unstable();
    expiry_keys.dedup();
    let expiries = expiry_keys
        .into_iter()
        .map(|expiry_ms| {
            let g = dto.greeks.by_expiry.iter().find(|e| e.expiry_ms == expiry_ms);
            ExpiryRow {
                time,
                expiry_ms: expiry_ms as i64,
                premium: dto.exposure.premium_by_expiry.get(&expiry_ms).copied().unwrap_or(0.0),
                delta_units: g.map(|e| e.greeks.delta_units).unwrap_or(0.0),
                gamma_units: g.map(|e| e.greeks.gamma_units).unwrap_or(0.0),
                vega: g.map(|e| e.greeks.vega).unwrap_or(0.0),
                theta_per_day: g.map(|e| e.greeks.theta_per_day).unwrap_or(0.0),
            }
        })
        .collect();
    (snapshot, symbols, venues, expiries)
}

/// Read complete JSONL lines past the persisted offset and mirror them.
async fn ingest_pnl_jsonl(history: &Arc<History>, path: &std::path::Path) -> Result<()> {
    let h = Arc::clone(history);
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let Ok(mut file) = std::fs::File::open(&path) else {
            return Ok(()); // no ledger yet
        };
        let offset = h.pnl_ingest_offset()?;
        let len = file.metadata().map(|m| m.len() as i64).unwrap_or(0);
        if len <= offset {
            return Ok(());
        }
        file.seek(std::io::SeekFrom::Start(offset as u64))
            .context("seeking pnl jsonl")?;
        let mut buf = String::new();
        file.read_to_string(&mut buf).context("reading pnl jsonl")?;
        let (rows, consumed) = parse_pnl_lines(&buf);
        if consumed == 0 {
            return Ok(());
        }
        h.insert_pnl_batch(rows, offset + consumed as i64)
    })
    .await
    .map_err(anyhow::Error::from)?
}

/// Parse complete (newline-terminated) JSONL records; returns the rows
/// and the byte count consumed (partial trailing lines are left for the
/// next pass). Unparseable complete lines are skipped but still consume
/// their bytes.
fn parse_pnl_lines(buf: &str) -> (Vec<PnlLineRow>, usize) {
    let mut rows = Vec::new();
    let mut consumed = 0usize;
    for line in buf.split_inclusive('\n') {
        if !line.ends_with('\n') {
            break; // partial write in flight
        }
        consumed += line.len();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<PnlJsonlRecord>(trimmed) {
            Ok(rec) => rows.push(PnlLineRow {
                time: ms_to_dt(rec.ts_ms as i64),
                ts_ms: rec.ts_ms as i64,
                line: rec.line,
                amount: rec.amount,
                note: rec.note,
            }),
            Err(e) => {
                tracing::warn!(error = %e, "skipping unparseable pnl jsonl line");
            }
        }
    }
    (rows, consumed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pnl_lines_handles_partials_and_junk() {
        let buf = "{\"ts_ms\":1,\"line\":\"spread\",\"amount\":10.5,\"note\":\"fill\"}\n\
                   not-json\n\
                   {\"ts_ms\":2,\"line\":\"theta\",\"amount\":-3.0,\"note\":\"accrual\"}\n\
                   {\"ts_ms\":3,\"line\":\"scalp\",\"amou";
        let (rows, consumed) = parse_pnl_lines(buf);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].line, "spread");
        assert_eq!(rows[1].ts_ms, 2);
        // Everything except the partial trailing line is consumed.
        let partial = "{\"ts_ms\":3,\"line\":\"scalp\",\"amou".len();
        assert_eq!(consumed, buf.len() - partial);
    }

    #[test]
    fn history_query_defaults_and_bucket_clamps() {
        let q = HistoryQuery {
            series: Series::Snapshots,
            from_ms: None,
            to_ms: None,
            bucket_secs: None,
            symbol: None,
            venue: None,
            line: None,
        };
        let (from, to) = q.range();
        assert_eq!(to - from, 24 * 3_600_000);
        // Default bucket for 24h ≈ span/720 = 120s.
        assert_eq!(q.bucket_secs(from, to), 120);
        // An explicit tiny bucket over a huge span is clamped to keep
        // point counts bounded.
        let q = HistoryQuery { bucket_secs: Some(1), ..q };
        let month = 30 * 24 * 3_600_000i64;
        assert_eq!(q.bucket_secs(0, month), month / 1000 / 1000);
    }

    #[test]
    fn effective_database_url_prefers_config_then_env() {
        let mut cfg = HistoryConfig::default();
        assert_eq!(cfg.effective_database_url(), None);
        cfg.database_url = "postgres://local".into();
        assert_eq!(cfg.effective_database_url().as_deref(), Some("postgres://local"));
    }
}
