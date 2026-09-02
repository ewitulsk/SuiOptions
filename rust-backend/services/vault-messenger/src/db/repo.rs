//! Repository over the vault-messenger DB.

use anyhow::{Context, Result};
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::r2d2::{ConnectionManager, PooledConnection};

use super::models::{status, LaneStatsRow, MessageRow, NewMessage, PayableRow};
use super::schema::lane_stats::dsl as ls;
use super::schema::spoke_payables::dsl as p;
use super::schema::vault_messages::dsl as m;
use super::schema::watch_cursors::dsl as c;
use super::DbPool;

#[derive(Clone)]
pub struct Repo {
    pool: std::sync::Arc<DbPool>,
}

/// Run one blocking repo call off the async runtime (diesel is sync).
pub async fn blocking<T, F>(repo: &Repo, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(Repo) -> Result<T> + Send + 'static,
{
    let repo = repo.clone();
    tokio::task::spawn_blocking(move || f(repo))
        .await
        .context("joining blocking DB call")?
}

impl Repo {
    pub fn new(pool: std::sync::Arc<DbPool>) -> Self {
        Self { pool }
    }

    fn conn(&self) -> Result<PooledConnection<ConnectionManager<PgConnection>>> {
        self.pool.get().context("checking out DB connection")
    }

    // ── messages ───────────────────────────────────────────────────────

    /// Insert a newly observed message; idempotent on
    /// (direction, spoke_id, seq). Returns true when the row is new —
    /// duplicates (re-scanned blocks, replayed cursors) are suppressed by
    /// the unique constraint.
    pub fn insert_message(&self, new: NewMessage) -> Result<bool> {
        let mut conn = self.conn()?;
        let inserted = diesel::insert_into(m::vault_messages)
            .values(&new)
            .on_conflict((m::direction, m::spoke_id, m::seq))
            .do_nothing()
            .execute(&mut conn)
            .context("inserting message")?;
        Ok(inserted > 0)
    }

    /// Rows in `status_` for one direction, lane order (spoke_id, seq).
    pub fn messages_with_status(&self, direction: &str, status_: &str) -> Result<Vec<MessageRow>> {
        let mut conn = self.conn()?;
        m::vault_messages
            .filter(m::direction.eq(direction))
            .filter(m::status.eq(status_))
            .order((m::spoke_id.asc(), m::seq.asc()))
            .load(&mut conn)
            .context("listing messages by status")
    }

    /// Highest confirmed seq on a lane (0 = nothing confirmed yet).
    pub fn last_confirmed_seq(&self, direction: &str, spoke_id: i64) -> Result<i64> {
        let mut conn = self.conn()?;
        let max: Option<i64> = m::vault_messages
            .filter(m::direction.eq(direction))
            .filter(m::spoke_id.eq(spoke_id))
            .filter(m::status.eq(status::CONFIRMED))
            .select(diesel::dsl::max(m::seq))
            .first(&mut conn)
            .context("reading last confirmed seq")?;
        Ok(max.unwrap_or(0))
    }

    pub fn mark_submitted(&self, id: i64, tx_hash: &str) -> Result<()> {
        let mut conn = self.conn()?;
        diesel::update(m::vault_messages.find(id))
            .set((
                m::status.eq(status::SUBMITTED),
                m::tx_hash.eq(tx_hash),
                m::attempts.eq(m::attempts + 1),
                m::updated_at.eq(diesel::dsl::now),
            ))
            .execute(&mut conn)
            .context("marking message submitted")?;
        Ok(())
    }

    pub fn mark_confirmed(&self, id: i64, tx_hash: Option<&str>, note: Option<&str>) -> Result<()> {
        let mut conn = self.conn()?;
        diesel::update(m::vault_messages.find(id))
            .set((
                m::status.eq(status::CONFIRMED),
                m::error.eq(note),
                m::updated_at.eq(diesel::dsl::now),
            ))
            .execute(&mut conn)
            .context("marking message confirmed")?;
        if let Some(tx) = tx_hash {
            diesel::update(m::vault_messages.find(id))
                .set(m::tx_hash.eq(tx))
                .execute(&mut conn)
                .context("recording confirm tx")?;
        }
        Ok(())
    }

    /// Attempt failed: bump attempts, back to `pending` for retry.
    pub fn record_failure(&self, id: i64, error: &str) -> Result<()> {
        let mut conn = self.conn()?;
        diesel::update(m::vault_messages.find(id))
            .set((
                m::status.eq(status::PENDING),
                m::attempts.eq(m::attempts + 1),
                m::error.eq(error),
                m::updated_at.eq(diesel::dsl::now),
            ))
            .execute(&mut conn)
            .context("recording delivery failure")?;
        Ok(())
    }

    /// Terminal failure (attempt budget exhausted).
    pub fn mark_failed(&self, id: i64, error: &str) -> Result<()> {
        let mut conn = self.conn()?;
        diesel::update(m::vault_messages.find(id))
            .set((
                m::status.eq(status::FAILED),
                m::error.eq(error),
                m::updated_at.eq(diesel::dsl::now),
            ))
            .execute(&mut conn)
            .context("marking message failed")?;
        Ok(())
    }

    /// The HTTP list: optional spoke/status filters, lane order, paged.
    pub fn list_messages(
        &self,
        spoke_id: Option<i64>,
        status_: Option<String>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<MessageRow>> {
        let mut conn = self.conn()?;
        let mut q = m::vault_messages
            .order((m::spoke_id.asc(), m::direction.asc(), m::seq.desc()))
            .limit(limit)
            .offset(offset)
            .into_boxed();
        if let Some(s) = spoke_id {
            q = q.filter(m::spoke_id.eq(s));
        }
        if let Some(st) = status_ {
            q = q.filter(m::status.eq(st));
        }
        q.load(&mut conn).context("listing messages")
    }

    /// Distinct (direction, spoke_id) lanes seen so far.
    pub fn lanes(&self) -> Result<Vec<(String, i64)>> {
        let mut conn = self.conn()?;
        m::vault_messages
            .select((m::direction, m::spoke_id))
            .distinct()
            .order((m::spoke_id.asc(), m::direction.asc()))
            .load(&mut conn)
            .context("listing lanes")
    }

    pub fn count_with_status(&self, direction: &str, spoke_id: i64, status_: &str) -> Result<i64> {
        let mut conn = self.conn()?;
        m::vault_messages
            .filter(m::direction.eq(direction))
            .filter(m::spoke_id.eq(spoke_id))
            .filter(m::status.eq(status_))
            .count()
            .get_result(&mut conn)
            .context("counting messages")
    }

    /// Oldest pending/submitted message across all lanes (queue-stall alert).
    pub fn oldest_undelivered_created(&self) -> Result<Option<DateTime<Utc>>> {
        let mut conn = self.conn()?;
        m::vault_messages
            .filter(m::status.eq_any([status::PENDING, status::SUBMITTED]))
            .select(diesel::dsl::min(m::created_at))
            .first(&mut conn)
            .context("reading oldest undelivered")
    }

    // ── cursors ────────────────────────────────────────────────────────

    pub fn cursor(&self, name: &str) -> Result<Option<String>> {
        let mut conn = self.conn()?;
        c::watch_cursors
            .find(name)
            .select(c::cursor)
            .first(&mut conn)
            .optional()
            .context("reading cursor")
    }

    pub fn set_cursor(&self, name: &str, value: &str) -> Result<()> {
        let mut conn = self.conn()?;
        diesel::insert_into(c::watch_cursors)
            .values((c::name.eq(name), c::cursor.eq(value)))
            .on_conflict(c::name)
            .do_update()
            .set((c::cursor.eq(value), c::updated_at.eq(diesel::dsl::now)))
            .execute(&mut conn)
            .context("writing cursor")?;
        Ok(())
    }

    // ── payables + lane stats ──────────────────────────────────────────

    pub fn upsert_payable(&self, spoke_id: i64, request_seq: i64, pay_units: BigDecimal) -> Result<()> {
        let mut conn = self.conn()?;
        diesel::insert_into(p::spoke_payables)
            .values((
                p::spoke_id.eq(spoke_id),
                p::request_seq.eq(request_seq),
                p::pay_units.eq(pay_units),
            ))
            .on_conflict((p::spoke_id, p::request_seq))
            .do_nothing()
            .execute(&mut conn)
            .context("upserting payable")?;
        Ok(())
    }

    pub fn settle_payable(&self, spoke_id: i64, request_seq: i64) -> Result<()> {
        let mut conn = self.conn()?;
        diesel::update(
            p::spoke_payables
                .filter(p::spoke_id.eq(spoke_id))
                .filter(p::request_seq.eq(request_seq)),
        )
        .set(p::settled_at.eq(diesel::dsl::now))
        .execute(&mut conn)
        .context("settling payable")?;
        Ok(())
    }

    /// Oldest unsettled payable (payout-queue-aged alert).
    pub fn oldest_unsettled_payable(&self) -> Result<Option<PayableRow>> {
        let mut conn = self.conn()?;
        p::spoke_payables
            .filter(p::settled_at.is_null())
            .order(p::created_at.asc())
            .first(&mut conn)
            .optional()
            .context("reading oldest unsettled payable")
    }

    pub fn upsert_lane_stats(
        &self,
        spoke_id: i64,
        fee_pot: BigDecimal,
        last_state_sync_ms: i64,
    ) -> Result<()> {
        let mut conn = self.conn()?;
        diesel::insert_into(ls::lane_stats)
            .values((
                ls::spoke_id.eq(spoke_id),
                ls::fee_pot.eq(&fee_pot),
                ls::last_state_sync_ms.eq(last_state_sync_ms),
            ))
            .on_conflict(ls::spoke_id)
            .do_update()
            .set((
                ls::fee_pot.eq(&fee_pot),
                ls::last_state_sync_ms.eq(last_state_sync_ms),
                ls::updated_at.eq(diesel::dsl::now),
            ))
            .execute(&mut conn)
            .context("upserting lane stats")?;
        Ok(())
    }

    pub fn lane_stats(&self) -> Result<Vec<LaneStatsRow>> {
        let mut conn = self.conn()?;
        ls::lane_stats
            .order(ls::spoke_id.asc())
            .load(&mut conn)
            .context("listing lane stats")
    }
}
