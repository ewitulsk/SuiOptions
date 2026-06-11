//! Repository over the charts DB. Timescale specifics (`time_bucket`,
//! `first`/`last`) go through `sql_query` — diesel's DSL doesn't know them.

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::r2d2::{ConnectionManager, PooledConnection};
use diesel::sql_types::{BigInt, Double, Text, Timestamptz};

use super::models::{CursorRow, MidRow, TradeRow};
use super::schema::{pool_mids, pool_trades, watch_cursor};
use super::DbPool;

#[derive(Clone)]
pub struct Repo {
    pool: std::sync::Arc<DbPool>,
}

/// One raw OHLC bucket as SQL returns it (only buckets containing trades —
/// gap filling happens in `bars::carry_forward`).
#[derive(QueryableByName, Debug, Clone)]
pub struct RawBar {
    #[diesel(sql_type = Timestamptz)]
    pub bucket: DateTime<Utc>,
    #[diesel(sql_type = Double)]
    pub open: f64,
    #[diesel(sql_type = Double)]
    pub high: f64,
    #[diesel(sql_type = Double)]
    pub low: f64,
    #[diesel(sql_type = Double)]
    pub close: f64,
    #[diesel(sql_type = Double)]
    pub volume_base: f64,
}

/// One downsampled midpoint bucket as SQL returns it (only buckets containing
/// samples — gap filling happens in `bars::carry_forward_mids`).
#[derive(QueryableByName, Debug, Clone)]
pub struct RawMid {
    #[diesel(sql_type = Timestamptz)]
    pub bucket: DateTime<Utc>,
    #[diesel(sql_type = Double)]
    pub mid: f64,
}

#[derive(QueryableByName, Debug, Clone)]
pub struct PoolSummary {
    #[diesel(sql_type = Text)]
    pub pool_id: String,
    #[diesel(sql_type = Text)]
    pub bucket_id: String,
    #[diesel(sql_type = Double)]
    pub last_price: f64,
    #[diesel(sql_type = BigInt)]
    pub last_trade_ms: i64,
    #[diesel(sql_type = Double)]
    pub volume_24h_base: f64,
}

impl Repo {
    pub fn new(pool: std::sync::Arc<DbPool>) -> Self {
        Self { pool }
    }

    fn conn(&self) -> Result<PooledConnection<ConnectionManager<PgConnection>>> {
        self.pool.get().context("checking out DB connection")
    }

    pub fn load_cursor(&self) -> Result<Option<(String, i64)>> {
        let mut conn = self.conn()?;
        let row = watch_cursor::table
            .find(1i16)
            .first::<CursorRow>(&mut conn)
            .optional()
            .context("loading watch_cursor")?;
        Ok(row.map(|r| (r.cursor_tx, r.cursor_ev)))
    }

    /// Insert a fill batch and advance the global cursor in one transaction.
    /// Conflicting rows (replays) are skipped; returns how many were new.
    pub fn insert_trades(&self, trades: &[TradeRow], cursor: (String, i64)) -> Result<usize> {
        let mut conn = self.conn()?;
        conn.transaction::<_, anyhow::Error, _>(|conn| {
            let inserted = if trades.is_empty() {
                0
            } else {
                diesel::insert_into(pool_trades::table)
                    .values(trades)
                    .on_conflict_do_nothing()
                    .execute(conn)
                    .context("inserting pool_trades")?
            };
            diesel::insert_into(watch_cursor::table)
                .values(CursorRow {
                    id: 1,
                    cursor_tx: cursor.0.clone(),
                    cursor_ev: cursor.1,
                    updated_at: Utc::now(),
                })
                .on_conflict(watch_cursor::id)
                .do_update()
                .set((
                    watch_cursor::cursor_tx.eq(&cursor.0),
                    watch_cursor::cursor_ev.eq(cursor.1),
                    watch_cursor::updated_at.eq(Utc::now()),
                ))
                .execute(conn)
                .context("advancing watch_cursor")?;
            Ok(inserted)
        })
    }

    /// Trade-bearing OHLC buckets for `[from, to)`, ascending.
    pub fn raw_bars(
        &self,
        pool_id: &str,
        interval_secs: i64,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<RawBar>> {
        let mut conn = self.conn()?;
        diesel::sql_query(
            "SELECT time_bucket(make_interval(secs => $1), time) AS bucket, \
                    first(price, time) AS open, max(price) AS high, \
                    min(price)  AS low, last(price, time) AS close, \
                    sum(base_qty / power(10, base_decimals))::float8 AS volume_base \
             FROM pool_trades \
             WHERE pool_id = $2 AND time >= $3 AND time < $4 \
             GROUP BY bucket ORDER BY bucket",
        )
        .bind::<BigInt, _>(interval_secs)
        .bind::<Text, _>(pool_id)
        .bind::<Timestamptz, _>(ms_to_dt(from_ms))
        .bind::<Timestamptz, _>(ms_to_dt(to_ms))
        .load::<RawBar>(&mut conn)
        .context("querying bars")
    }

    /// Insert one midpoint sample. Replays (same pool + timestamp) are skipped.
    pub fn insert_mid(&self, row: &MidRow) -> Result<()> {
        let mut conn = self.conn()?;
        diesel::insert_into(pool_mids::table)
            .values(row)
            .on_conflict_do_nothing()
            .execute(&mut conn)
            .context("inserting pool_mids")?;
        Ok(())
    }

    /// Sample-bearing midpoint buckets for `[from, to)`, ascending — the last
    /// sample in each bucket is the bucket's value.
    pub fn raw_mids(
        &self,
        pool_id: &str,
        interval_secs: i64,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<RawMid>> {
        let mut conn = self.conn()?;
        diesel::sql_query(
            "SELECT time_bucket(make_interval(secs => $1), time) AS bucket, \
                    last(mid, time) AS mid \
             FROM pool_mids \
             WHERE pool_id = $2 AND time >= $3 AND time < $4 \
             GROUP BY bucket ORDER BY bucket",
        )
        .bind::<BigInt, _>(interval_secs)
        .bind::<Text, _>(pool_id)
        .bind::<Timestamptz, _>(ms_to_dt(from_ms))
        .bind::<Timestamptz, _>(ms_to_dt(to_ms))
        .load::<RawMid>(&mut conn)
        .context("querying mids")
    }

    /// Last midpoint strictly before `before_ms` — seeds the carry-forward
    /// value for ranges that start inside a gap.
    pub fn last_mid_before(&self, pool_id: &str, before_ms: i64) -> Result<Option<f64>> {
        let mut conn = self.conn()?;
        let row = pool_mids::table
            .filter(pool_mids::pool_id.eq(pool_id))
            .filter(pool_mids::time.lt(ms_to_dt(before_ms)))
            .order(pool_mids::time.desc())
            .select(pool_mids::mid)
            .first::<f64>(&mut conn)
            .optional()
            .context("querying last mid")?;
        Ok(row)
    }

    /// Last trade price strictly before `before_ms` — seeds the carry-forward
    /// close for ranges that start inside a gap.
    pub fn last_close_before(&self, pool_id: &str, before_ms: i64) -> Result<Option<f64>> {
        let mut conn = self.conn()?;
        let row = pool_trades::table
            .filter(pool_trades::pool_id.eq(pool_id))
            .filter(pool_trades::time.lt(ms_to_dt(before_ms)))
            .order(pool_trades::time.desc())
            .select(pool_trades::price)
            .first::<f64>(&mut conn)
            .optional()
            .context("querying last close")?;
        Ok(row)
    }

    /// Per-pool last price + 24h volume across every pool with data.
    pub fn pool_summaries(&self) -> Result<Vec<PoolSummary>> {
        let mut conn = self.conn()?;
        diesel::sql_query(
            "SELECT pool_id, \
                    last(bucket_id, time) AS bucket_id, \
                    last(price, time) AS last_price, \
                    (extract(epoch FROM max(time)) * 1000)::int8 AS last_trade_ms, \
                    coalesce(sum(base_qty / power(10, base_decimals)) \
                        FILTER (WHERE time > now() - interval '24 hours'), 0)::float8 \
                        AS volume_24h_base \
             FROM pool_trades GROUP BY pool_id",
        )
        .load::<PoolSummary>(&mut conn)
        .context("querying pool summaries")
    }

    /// Drop trades + mids for pools outside `keep` whose newest row (in
    /// either table) is older than `ttl_hours`. Returns (pools dropped,
    /// rows deleted).
    pub fn evict_stale(&self, keep: &[String], ttl_hours: i64) -> Result<(usize, usize)> {
        #[derive(QueryableByName)]
        struct StalePool {
            #[diesel(sql_type = Text)]
            pool_id: String,
        }

        let mut conn = self.conn()?;
        let stale: Vec<String> = diesel::sql_query(
            "SELECT pool_id FROM ( \
                 SELECT pool_id, max(time) AS newest FROM pool_trades GROUP BY pool_id \
                 UNION ALL \
                 SELECT pool_id, max(time) FROM pool_mids GROUP BY pool_id \
             ) t GROUP BY pool_id \
             HAVING max(newest) < now() - make_interval(hours => $1)",
        )
        .bind::<BigInt, _>(ttl_hours)
        .load::<StalePool>(&mut conn)
        .context("finding stale pools")?
        .into_iter()
        .map(|p| p.pool_id)
        .filter(|p| !keep.contains(p))
        .collect();

        if stale.is_empty() {
            return Ok((0, 0));
        }
        let deleted = diesel::delete(pool_trades::table.filter(pool_trades::pool_id.eq_any(&stale)))
            .execute(&mut conn)
            .context("evicting stale pools")?;
        let deleted_mids =
            diesel::delete(pool_mids::table.filter(pool_mids::pool_id.eq_any(&stale)))
                .execute(&mut conn)
                .context("evicting stale pool mids")?;
        Ok((stale.len(), deleted + deleted_mids))
    }
}

fn ms_to_dt(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().unwrap_or_else(Utc::now)
}
