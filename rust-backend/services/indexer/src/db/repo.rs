//! The Postgres-facing repository.
//!
//! Three operations matter:
//!
//!   - [`Repo::apply_checkpoint`] — single transaction per Sui checkpoint.
//!     Inserts the events into the log, upserts the materialised views, and
//!     advances `indexer_progress`. Idempotent via the
//!     `UNIQUE (checkpoint, tx_digest, event_index)` constraint, so re-running
//!     a partially-processed checkpoint is safe.
//!
//!   - [`Repo::load_progress`] — at boot, returns the last fully processed
//!     `(checkpoint, sequence)` so the worker can resume.
//!
//!   - [`Repo::hydrate`] — at boot, reloads accounts/buckets/positions into
//!     in-memory state so `Store::bucket()` / `Store::account()` work without
//!     replaying the log.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use bigdecimal::BigDecimal;
use chrono::Utc;
use diesel::pg::{Pg, PgConnection};
use diesel::prelude::*;
use diesel::r2d2::{ConnectionManager, PooledConnection};
use diesel::sql_types::{Bool, Jsonb};
use diesel::IntoSql;
use tracing::{debug, info, trace};

use protocol_types::events::ChainEvent;
use protocol_types::ids::ObjectId;

use crate::store::{AccountState, BucketState, DeepBookPoolState, OrgState, PositionState};

use super::models::{
    account_row_into_state, bigdecimal_to_u128, event_type_tag, AccountBalanceRow, AccountRow,
    BucketRow, DeepBookPoolRow, EventParticipantRow, IndexedEventRow, NewIndexedEventRow, OrgRow,
    PositionRow, ProgressRow,
};
use super::schema::{
    account_balances, accounts, bucket_deepbook_pools, buckets, event_participants,
    indexed_events, indexer_progress, orgs, positions,
};
use super::DbPool;

/// What a worker accumulates for a single checkpoint before calling
/// [`Repo::apply_checkpoint`].
///
/// All sequence numbers in `events` must be contiguous; `last_sequence` is
/// the highest one. The worker is responsible for assigning these
/// monotonically (the Store helps via `Store::ingest_batch`).
#[derive(Debug, Clone)]
pub struct CheckpointBatch {
    pub checkpoint: i64,
    pub last_sequence: i64,
    pub events: Vec<NewIndexedEventRow>,
    pub accounts: Vec<AccountRow>,
    pub account_balances: Vec<AccountBalanceRow>,
    pub orgs: Vec<OrgRow>,
    pub buckets: Vec<BucketRow>,
    /// Bucket → DeepBook venue rows (SO-152). Insert-only, first pool wins.
    pub deepbook_pools: Vec<DeepBookPoolRow>,
    pub position_upserts: Vec<PositionRow>,
    /// Positions to drop (`Redeemed` removes them). Keyed `(bucket_id_hex, range_start)`.
    pub position_deletes: Vec<(String, BigDecimal)>,
    /// Per-event (address, role) edges for the `participant` query filter.
    pub event_participants: Vec<EventParticipantRow>,
}

impl CheckpointBatch {
    pub fn empty(checkpoint: i64, last_sequence: i64) -> Self {
        Self {
            checkpoint,
            last_sequence,
            events: Vec::new(),
            accounts: Vec::new(),
            account_balances: Vec::new(),
            orgs: Vec::new(),
            buckets: Vec::new(),
            deepbook_pools: Vec::new(),
            position_upserts: Vec::new(),
            position_deletes: Vec::new(),
            event_participants: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
            && self.accounts.is_empty()
            && self.account_balances.is_empty()
            && self.orgs.is_empty()
            && self.buckets.is_empty()
            && self.deepbook_pools.is_empty()
            && self.position_upserts.is_empty()
            && self.position_deletes.is_empty()
            && self.event_participants.is_empty()
    }
}

/// Build helper used by `Store::ingest_batch` to turn a single `ChainEvent`
/// into the rows it should produce. Kept here so all "ChainEvent → DB row"
/// logic lives next to the Repo that consumes it.
pub struct EventBuild;

impl EventBuild {
    pub fn new_event_row(
        sequence: i64,
        checkpoint: i64,
        tx_digest: String,
        event_index: i32,
        timestamp_ms: i64,
        event: &ChainEvent,
    ) -> Result<NewIndexedEventRow> {
        let payload = serde_json::to_value(event).context("encoding event payload")?;
        Ok(NewIndexedEventRow {
            sequence,
            checkpoint,
            tx_digest,
            event_index,
            timestamp_ms,
            event_type: event_type_tag(event).to_string(),
            payload,
        })
    }
}

/// In-memory result of `Repo::hydrate`. Same shape `Store` keeps internally,
/// so the boot path can move it straight in.
pub struct HydratedViews {
    pub accounts: BTreeMap<ObjectId, AccountState>,
    pub orgs: BTreeMap<ObjectId, OrgState>,
    pub buckets: BTreeMap<ObjectId, BucketState>,
    pub positions: BTreeMap<(ObjectId, u128), PositionState>,
    pub deepbook_pools: BTreeMap<ObjectId, DeepBookPoolState>,
}

#[derive(Clone)]
pub struct Repo {
    pool: Arc<DbPool>,
}

impl Repo {
    pub fn new(pool: Arc<DbPool>) -> Self {
        Self { pool }
    }

    fn conn(&self) -> Result<PooledConnection<ConnectionManager<PgConnection>>> {
        self.pool.get().context("checking out DB connection")
    }

    /// One transaction: insert events, upsert materialised views, advance
    /// progress. Empty batches are a no-op (the worker may see checkpoints
    /// containing zero indexable events).
    pub fn apply_checkpoint(&self, batch: &CheckpointBatch) -> Result<()> {
        if batch.is_empty() {
            trace!(checkpoint = batch.checkpoint, "empty checkpoint, advancing progress only");
            // Still advance progress so we don't re-scan empty checkpoints
            // forever after a restart.
            return self.advance_progress(batch.checkpoint, batch.last_sequence);
        }

        let mut conn = self.conn()?;
        conn.transaction::<_, anyhow::Error, _>(|conn| {
            if !batch.events.is_empty() {
                diesel::insert_into(indexed_events::table)
                    .values(&batch.events)
                    .on_conflict_do_nothing()
                    .execute(conn)
                    .context("inserting indexed_events")?;
            }

            if !batch.event_participants.is_empty() {
                // Inserted after indexed_events (FK). on_conflict_do_nothing
                // keeps checkpoint reprocessing idempotent.
                diesel::insert_into(event_participants::table)
                    .values(&batch.event_participants)
                    .on_conflict_do_nothing()
                    .execute(conn)
                    .context("inserting event_participants")?;
            }

            for acct in &batch.accounts {
                diesel::insert_into(accounts::table)
                    .values(acct)
                    .on_conflict(accounts::account_id)
                    .do_update()
                    .set((
                        accounts::owner.eq(&acct.owner),
                        accounts::signing_pubkey.eq(&acct.signing_pubkey),
                        accounts::signing_scheme.eq(acct.signing_scheme),
                        accounts::updated_at_seq.eq(acct.updated_at_seq),
                    ))
                    .execute(conn)
                    .context("upserting accounts")?;
            }

            for bal in &batch.account_balances {
                diesel::insert_into(account_balances::table)
                    .values(bal)
                    .on_conflict((
                        account_balances::account_id,
                        account_balances::asset_type,
                    ))
                    .do_update()
                    .set((
                        account_balances::balance.eq(&bal.balance),
                        account_balances::updated_at_seq.eq(bal.updated_at_seq),
                    ))
                    .execute(conn)
                    .context("upserting account_balances")?;
            }

            for org in &batch.orgs {
                diesel::insert_into(orgs::table)
                    .values(org)
                    .on_conflict(orgs::org_id)
                    .do_update()
                    .set((
                        orgs::fee_bps.eq(org.fee_bps),
                        orgs::updated_at_seq.eq(org.updated_at_seq),
                    ))
                    .execute(conn)
                    .context("upserting orgs")?;
            }

            for bkt in &batch.buckets {
                diesel::insert_into(buckets::table)
                    .values(bkt)
                    .on_conflict(buckets::bucket_id)
                    .do_update()
                    .set((
                        buckets::total_written.eq(&bkt.total_written),
                        buckets::exercise_cursor.eq(&bkt.exercise_cursor),
                        buckets::cleaned.eq(bkt.cleaned),
                        buckets::invalidated.eq(bkt.invalidated),
                        buckets::updated_at_seq.eq(bkt.updated_at_seq),
                    ))
                    .execute(conn)
                    .context("upserting buckets")?;
            }

            if !batch.deepbook_pools.is_empty() {
                // First pool wins: conflicts on bucket_id (duplicate venue)
                // or pool_id (replay) are silently skipped.
                diesel::insert_into(bucket_deepbook_pools::table)
                    .values(&batch.deepbook_pools)
                    .on_conflict_do_nothing()
                    .execute(conn)
                    .context("inserting bucket_deepbook_pools")?;
            }

            for pos in &batch.position_upserts {
                diesel::insert_into(positions::table)
                    .values(pos)
                    .on_conflict((positions::bucket_id, positions::range_start))
                    .do_update()
                    .set((
                        positions::range_end.eq(&pos.range_end),
                        positions::recipient.eq(&pos.recipient),
                        positions::updated_at_seq.eq(pos.updated_at_seq),
                    ))
                    .execute(conn)
                    .context("upserting positions")?;
            }

            for (bucket_hex, range_start) in &batch.position_deletes {
                diesel::delete(
                    positions::table.filter(
                        positions::bucket_id
                            .eq(bucket_hex)
                            .and(positions::range_start.eq(range_start)),
                    ),
                )
                .execute(conn)
                .context("deleting positions")?;
            }

            // Singleton progress row. The first checkpoint creates it; later
            // ones just update.
            diesel::insert_into(indexer_progress::table)
                .values(ProgressRow {
                    id: 1,
                    last_checkpoint: batch.checkpoint,
                    last_sequence: batch.last_sequence,
                    updated_at: Utc::now(),
                })
                .on_conflict(indexer_progress::id)
                .do_update()
                .set((
                    indexer_progress::last_checkpoint.eq(batch.checkpoint),
                    indexer_progress::last_sequence.eq(batch.last_sequence),
                    indexer_progress::updated_at.eq(Utc::now()),
                ))
                .execute(conn)
                .context("upserting indexer_progress")?;

            Ok(())
        })
    }

    fn advance_progress(&self, checkpoint: i64, last_sequence: i64) -> Result<()> {
        let mut conn = self.conn()?;
        diesel::insert_into(indexer_progress::table)
            .values(ProgressRow {
                id: 1,
                last_checkpoint: checkpoint,
                last_sequence,
                updated_at: Utc::now(),
            })
            .on_conflict(indexer_progress::id)
            .do_update()
            .set((
                indexer_progress::last_checkpoint.eq(checkpoint),
                indexer_progress::last_sequence.eq(last_sequence),
                indexer_progress::updated_at.eq(Utc::now()),
            ))
            .execute(&mut conn)
            .context("advancing indexer_progress on empty checkpoint")?;
        Ok(())
    }

    pub fn load_progress(&self) -> Result<Option<ProgressRow>> {
        debug!("loading indexer progress from postgres");
        let mut conn = self.conn()?;
        indexer_progress::table
            .find(1i16)
            .first::<ProgressRow>(&mut conn)
            .optional()
            .context("loading indexer_progress")
    }

    /// Reload the materialised views into memory. Called once at boot.
    pub fn hydrate(&self) -> Result<HydratedViews> {
        info!("hydrating materialized views from postgres");
        let mut conn = self.conn()?;

        let mut acct_map: BTreeMap<ObjectId, AccountState> = BTreeMap::new();
        for row in accounts::table
            .load::<AccountRow>(&mut conn)
            .context("loading accounts")?
        {
            let (id, state) = account_row_into_state(row)?;
            acct_map.insert(id, state);
        }

        for row in account_balances::table
            .load::<AccountBalanceRow>(&mut conn)
            .context("loading account_balances")?
        {
            let id = ObjectId::from_hex(&row.account_id)
                .map_err(|e| anyhow::anyhow!("balance account_id {}: {e}", row.account_id))?;
            if let Some(acct) = acct_map.get_mut(&id) {
                let bal = bigdecimal_to_u128(&row.balance)?;
                // Balances are stored as NUMERIC for headroom but the in-memory
                // shape uses u64 (mirroring Coin values). Truncate via u128 → u64
                // — overflow would mean an on-chain balance > u64::MAX which
                // can't happen.
                let bal_u64 = bal.try_into().map_err(|_| {
                    anyhow::anyhow!(
                        "balance {bal} for account {} asset {} exceeds u64",
                        row.account_id,
                        row.asset_type
                    )
                })?;
                acct.balances.insert(
                    protocol_types::asset::AssetType::new(row.asset_type),
                    bal_u64,
                );
            }
        }

        let mut org_map: BTreeMap<ObjectId, OrgState> = BTreeMap::new();
        for row in orgs::table.load::<OrgRow>(&mut conn).context("loading orgs")? {
            let (id, state) = row.into_state()?;
            org_map.insert(id, state);
        }

        let mut bucket_map: BTreeMap<ObjectId, BucketState> = BTreeMap::new();
        for row in buckets::table
            .load::<BucketRow>(&mut conn)
            .context("loading buckets")?
        {
            let (id, state) = row.into_state()?;
            bucket_map.insert(id, state);
        }

        let mut position_map: BTreeMap<(ObjectId, u128), PositionState> = BTreeMap::new();
        for row in positions::table
            .load::<PositionRow>(&mut conn)
            .context("loading positions")?
        {
            let (key, state) = row.into_state()?;
            position_map.insert(key, state);
        }

        let mut deepbook_map: BTreeMap<ObjectId, DeepBookPoolState> = BTreeMap::new();
        for row in bucket_deepbook_pools::table
            .load::<DeepBookPoolRow>(&mut conn)
            .context("loading bucket_deepbook_pools")?
        {
            let (bucket, state) = row.into_state()?;
            deepbook_map.insert(bucket, state);
        }

        debug!(
            accounts = acct_map.len(),
            orgs = org_map.len(),
            buckets = bucket_map.len(),
            positions = position_map.len(),
            deepbook_pools = deepbook_map.len(),
            "hydration complete"
        );
        Ok(HydratedViews {
            accounts: acct_map,
            orgs: org_map,
            buckets: bucket_map,
            positions: position_map,
            deepbook_pools: deepbook_map,
        })
    }

    /// SO-97: enriched positions for a set of on-chain object ids — the
    /// authoritative list comes from the caller's wallet; this joins each id
    /// to its bucket (strike/expiry/cursor) plus the denormalized provenance
    /// on the position row. Unknown ids are simply absent from the result.
    pub fn positions_by_object_ids(
        &self,
        object_ids: &[String],
    ) -> Result<Vec<(PositionRow, BucketRow)>> {
        if object_ids.is_empty() {
            return Ok(vec![]);
        }
        let mut conn = self.conn()?;
        positions::table
            .inner_join(buckets::table.on(positions::bucket_id.eq(buckets::bucket_id)))
            .filter(positions::object_id.eq_any(object_ids))
            .select((positions::all_columns, buckets::all_columns))
            .load::<(PositionRow, BucketRow)>(&mut conn)
            .context("loading positions by object_ids")
    }

    /// JIT point-lookup: one account with its per-asset balances. Returns
    /// `None` when the account isn't known. Backs the GraphQL `account(id)`
    /// query that replaces the quoting-service's in-memory account mirror.
    pub fn account_by_id(
        &self,
        account_id: &str,
    ) -> Result<Option<(AccountRow, Vec<AccountBalanceRow>)>> {
        let mut conn = self.conn()?;
        let acct = accounts::table
            .find(account_id)
            .first::<AccountRow>(&mut conn)
            .optional()
            .context("loading account")?;
        let Some(acct) = acct else {
            return Ok(None);
        };
        let bals = account_balances::table
            .filter(account_balances::account_id.eq(account_id))
            .load::<AccountBalanceRow>(&mut conn)
            .context("loading account_balances")?;
        Ok(Some((acct, bals)))
    }

    /// JIT point-lookup: one bucket by id. Backs the GraphQL `bucket(id)`
    /// query (quoting-service validity/invalidation checks).
    pub fn bucket_by_id(&self, bucket_id: &str) -> Result<Option<BucketRow>> {
        let mut conn = self.conn()?;
        buckets::table
            .find(bucket_id)
            .first::<BucketRow>(&mut conn)
            .optional()
            .context("loading bucket")
    }

    /// JIT point-lookup: one org by id. Backs the GraphQL `org(id)` query.
    pub fn org_by_id(&self, org_id: &str) -> Result<Option<OrgRow>> {
        let mut conn = self.conn()?;
        orgs::table
            .find(org_id)
            .first::<OrgRow>(&mut conn)
            .optional()
            .context("loading org")
    }

    /// JIT list of every org the indexer has seen. Backs the GraphQL `orgs`
    /// query (api-service joins this against the verified allowlist).
    pub fn orgs_query(&self) -> Result<Vec<OrgRow>> {
        let mut conn = self.conn()?;
        orgs::table
            .order(orgs::org_id.asc())
            .load::<OrgRow>(&mut conn)
            .context("loading orgs")
    }

    /// JIT list query over the bucket view. All filters are ANDed; `None`
    /// means "don't constrain". `active_only` drops cleaned buckets. Backs
    /// the GraphQL `buckets(...)` query (api-service catalog + option-scheduler
    /// roll-confirmation lookups).
    /// bucket_id → DeepBook pool_id for the given buckets (SO-152). Buckets
    /// without a venue are simply absent from the map.
    pub fn deepbook_pool_ids(
        &self,
        bucket_ids: &[String],
    ) -> Result<std::collections::HashMap<String, String>> {
        if bucket_ids.is_empty() {
            return Ok(Default::default());
        }
        let mut conn = self.conn()?;
        let rows: Vec<(String, String)> = bucket_deepbook_pools::table
            .filter(bucket_deepbook_pools::bucket_id.eq_any(bucket_ids))
            .select((
                bucket_deepbook_pools::bucket_id,
                bucket_deepbook_pools::pool_id,
            ))
            .load(&mut conn)
            .context("loading bucket_deepbook_pools ids")?;
        Ok(rows.into_iter().collect())
    }

    pub fn buckets_query(&self, f: BucketQuery) -> Result<Vec<BucketRow>> {
        let mut conn = self.conn()?;
        let mut q = buckets::table.into_boxed();
        if f.active_only {
            q = q.filter(buckets::cleaned.eq(false));
        }
        if let Some(ids) = &f.ids {
            q = q.filter(buckets::bucket_id.eq_any(ids.clone()));
        }
        if let Some(org_ids) = &f.org_ids {
            q = q.filter(buckets::org_id.eq_any(org_ids.clone()));
        }
        if let Some(a) = &f.asset_type {
            q = q.filter(buckets::asset_type.eq(a.clone()));
        }
        if let Some(s) = &f.settlement_type {
            q = q.filter(buckets::settlement_type.eq(s.clone()));
        }
        if let Some(e) = f.expiry_ms {
            q = q.filter(buckets::expiry_ms.eq(e));
        }
        q.order(buckets::expiry_ms.asc())
            .load::<BucketRow>(&mut conn)
            .context("loading buckets")
    }

    /// JIT: enriched positions held by `recipient` (mint-time owner-of-record),
    /// each joined to its bucket. Backs the GraphQL `positions(recipient:)`
    /// query (api-service writer-side positions list).
    pub fn positions_by_recipient(
        &self,
        recipient: &str,
    ) -> Result<Vec<(PositionRow, BucketRow)>> {
        let mut conn = self.conn()?;
        positions::table
            .inner_join(buckets::table.on(positions::bucket_id.eq(buckets::bucket_id)))
            .filter(positions::recipient.eq(recipient))
            .select((positions::all_columns, buckets::all_columns))
            .load::<(PositionRow, BucketRow)>(&mut conn)
            .context("loading positions by recipient")
    }

    /// Generalized event query (SO-97): compile the `EventFilter` AST to a
    /// parameterized WHERE over `indexed_events`, ordered by sequence with
    /// cursor pagination. Wrapped in a transaction with a `statement_timeout`
    /// so a pathological filter can't run away even though the API is internal.
    pub fn query_events(&self, q: EventQuery) -> Result<Vec<IndexedEventRow>> {
        let mut conn = self.conn()?;
        conn.transaction::<Vec<IndexedEventRow>, anyhow::Error, _>(|conn| {
            diesel::sql_query("SET LOCAL statement_timeout = 5000")
                .execute(conn)
                .context("setting statement_timeout")?;

            let cond: BoxedEventCond = match &q.filter {
                Some(f) => compile_event_filter(f, 0)?,
                None => Box::new(diesel::dsl::sql::<Bool>("TRUE")),
            };

            let mut query = indexed_events::table.into_boxed().filter(cond);
            if let Some(after) = q.after_sequence {
                query = if q.descending {
                    query.filter(indexed_events::sequence.lt(after))
                } else {
                    query.filter(indexed_events::sequence.gt(after))
                };
            }
            query = if q.descending {
                query.order(indexed_events::sequence.desc())
            } else {
                query.order(indexed_events::sequence.asc())
            };
            query
                .limit(q.limit)
                .load::<IndexedEventRow>(conn)
                .context("loading events")
        })
    }
}

// ── generalized event filter (SO-97) ──────────────────────────────────────

/// Plain (async-graphql-free) filter AST. `src/graphql.rs` maps its
/// `InputObject` onto this so the compiler stays a pure DB concern.
#[derive(Default, Clone, Debug)]
pub struct EventFilter {
    pub and: Vec<EventFilter>,
    pub or: Vec<EventFilter>,
    pub not: Option<Box<EventFilter>>,
    /// `event_type IN (...)`.
    pub event_type: Option<Vec<String>>,
    /// Address involved in any role (via `event_participants`).
    pub participant: Option<String>,
    /// Convenience → `payload @> {"account_id": …}`.
    pub account_id: Option<String>,
    /// Convenience → `payload @> {"bucket_id": …}`.
    pub bucket_id: Option<String>,
    /// General matcher → `payload @> $json`.
    pub payload_contains: Option<serde_json::Value>,
    pub timestamp_ms_gte: Option<i64>,
    pub timestamp_ms_lte: Option<i64>,
    pub sequence_gt: Option<i64>,
    pub sequence_lt: Option<i64>,
    pub checkpoint_gte: Option<i64>,
    pub checkpoint_lte: Option<i64>,
    pub tx_digest: Option<String>,
}

pub struct EventQuery {
    pub filter: Option<EventFilter>,
    pub descending: bool,
    pub after_sequence: Option<i64>,
    pub limit: i64,
}

/// Filter for [`Repo::buckets_query`]. All set fields are ANDed.
#[derive(Default, Clone, Debug)]
pub struct BucketQuery {
    /// Drop cleaned buckets when true.
    pub active_only: bool,
    pub ids: Option<Vec<String>>,
    /// Restrict to buckets belonging to these orgs (verified-only surfaces).
    pub org_ids: Option<Vec<String>>,
    pub asset_type: Option<String>,
    pub settlement_type: Option<String>,
    pub expiry_ms: Option<i64>,
}

const MAX_FILTER_DEPTH: u8 = 12;

// `payload @> $json` — JSONB containment, GIN-indexed.
diesel::infix_operator!(JsonbContains, " @> ", backend: Pg);

type BoxedEventCond = Box<dyn BoxableExpression<indexed_events::table, Pg, SqlType = Bool>>;

fn payload_contains_expr(value: serde_json::Value) -> BoxedEventCond {
    Box::new(JsonbContains::new(
        indexed_events::payload,
        value.into_sql::<Jsonb>(),
    ))
}

fn fold_bool(conds: Vec<BoxedEventCond>, all: bool) -> BoxedEventCond {
    let mut it = conds.into_iter();
    match it.next() {
        None => Box::new(diesel::dsl::sql::<Bool>(if all { "TRUE" } else { "FALSE" })),
        Some(first) => it.fold(first, |acc, c| {
            if all {
                Box::new(acc.and(c))
            } else {
                Box::new(acc.or(c))
            }
        }),
    }
}

fn compile_event_filter(f: &EventFilter, depth: u8) -> Result<BoxedEventCond> {
    if depth > MAX_FILTER_DEPTH {
        anyhow::bail!("event filter nested deeper than {MAX_FILTER_DEPTH}");
    }
    let mut conds: Vec<BoxedEventCond> = Vec::new();

    if let Some(types) = &f.event_type {
        if !types.is_empty() {
            conds.push(Box::new(indexed_events::event_type.eq_any(types.clone())));
        }
    }
    if let Some(v) = f.timestamp_ms_gte {
        conds.push(Box::new(indexed_events::timestamp_ms.ge(v)));
    }
    if let Some(v) = f.timestamp_ms_lte {
        conds.push(Box::new(indexed_events::timestamp_ms.le(v)));
    }
    if let Some(v) = f.sequence_gt {
        conds.push(Box::new(indexed_events::sequence.gt(v)));
    }
    if let Some(v) = f.sequence_lt {
        conds.push(Box::new(indexed_events::sequence.lt(v)));
    }
    if let Some(v) = f.checkpoint_gte {
        conds.push(Box::new(indexed_events::checkpoint.ge(v)));
    }
    if let Some(v) = f.checkpoint_lte {
        conds.push(Box::new(indexed_events::checkpoint.le(v)));
    }
    if let Some(tx) = &f.tx_digest {
        conds.push(Box::new(indexed_events::tx_digest.eq(tx.clone())));
    }
    if let Some(addr) = &f.participant {
        let sub = event_participants::table
            .filter(event_participants::address.eq(addr.clone()))
            .select(event_participants::sequence);
        conds.push(Box::new(indexed_events::sequence.eq_any(sub)));
    }
    if let Some(a) = &f.account_id {
        conds.push(payload_contains_expr(serde_json::json!({ "account_id": a })));
    }
    if let Some(b) = &f.bucket_id {
        conds.push(payload_contains_expr(serde_json::json!({ "bucket_id": b })));
    }
    if let Some(j) = &f.payload_contains {
        conds.push(payload_contains_expr(j.clone()));
    }
    if !f.and.is_empty() {
        let inner = f
            .and
            .iter()
            .map(|s| compile_event_filter(s, depth + 1))
            .collect::<Result<Vec<_>>>()?;
        conds.push(fold_bool(inner, true));
    }
    if !f.or.is_empty() {
        let inner = f
            .or
            .iter()
            .map(|s| compile_event_filter(s, depth + 1))
            .collect::<Result<Vec<_>>>()?;
        conds.push(fold_bool(inner, false));
    }
    if let Some(n) = &f.not {
        conds.push(Box::new(diesel::dsl::not(compile_event_filter(n, depth + 1)?)));
    }

    Ok(fold_bool(conds, true))
}

