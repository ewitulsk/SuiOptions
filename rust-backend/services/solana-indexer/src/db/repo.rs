//! The Postgres-facing repository.
//!
//! Write path:
//!   - [`Repo::apply_slot`] — one transaction per confirmed slot. Each
//!     event inserts into `indexed_events` (`ON CONFLICT DO NOTHING
//!     RETURNING sequence`); ONLY events that actually landed fold into
//!     the materialised views, so additive folds (balances, receipts,
//!     pending deposits) can never double-apply on replay.
//!   - [`Repo::set_finalized_slot`] — advances the reorg watermark.
//!   - [`Repo::evict_forked_slot`] — deletes a forked-away slot's events
//!     and rebuilds every view by replaying the surviving log through the
//!     same fold. Expected to never run at `confirmed` commitment; exists
//!     as the correctness backstop.
//!
//! Read path: JIT point/list queries backing the GraphQL surface, plus the
//! generalized `query_events` filter compiler (ported from the Sui
//! indexer, with slot ranges and a `finalized_only` tier flag).

use std::sync::Arc;

use anyhow::{Context, Result};
use bigdecimal::BigDecimal;
use chrono::Utc;
use diesel::pg::{Pg, PgConnection};
use diesel::prelude::*;
use diesel::r2d2::{ConnectionManager, PooledConnection};
use diesel::sql_types::{Bool, Jsonb};
use diesel::IntoSql;
use tracing::{info, trace, warn};

use crate::events::{DecodedEvent, Pubkey};

use super::models::{
    u128_bd, u64_bd, AccountBalanceRow, AccountRow, AuctionBidRow, AuctionRow, BucketRow,
    EventParticipantRow, IndexedEventRow, NewIndexedEventRow, PositionRow, ProgressRow,
    VaultReceiptRow, VaultRoundRow, VaultRow,
};
use super::schema::{
    account_balances, accounts, auction_bids, auctions, buckets, event_participants,
    indexed_events, indexer_progress, positions, vault_receipts, vault_rounds, vaults,
};
use super::DbPool;

/// One decoded event awaiting persistence, with its transaction coordinates.
#[derive(Debug, Clone)]
pub struct PendingEvent {
    pub event: DecodedEvent,
    pub signature: String,
    pub tx_index: i64,
    /// Global enumeration index of the inner instruction within its
    /// transaction — with `signature` this is the idempotency key.
    pub inner_ix_index: i32,
}

/// Everything the worker accumulated for one confirmed slot.
#[derive(Debug, Clone)]
pub struct SlotBatch {
    pub slot: i64,
    pub timestamp_ms: i64,
    pub events: Vec<PendingEvent>,
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

    /// One transaction: insert events, fold views for the fresh ones,
    /// advance `last_slot`. Returns how many events were newly inserted.
    pub fn apply_slot(&self, batch: &SlotBatch) -> Result<usize> {
        let _apply = tracing::info_span!("apply_slot", slot = batch.slot).entered();
        let mut conn = self.conn()?;
        conn.transaction::<usize, anyhow::Error, _>(|conn| {
            let mut inserted = 0usize;
            for ev in &batch.events {
                let payload = ev.event.payload()?;
                let row = NewIndexedEventRow {
                    slot: batch.slot,
                    signature: ev.signature.clone(),
                    tx_index: ev.tx_index,
                    inner_ix_index: ev.inner_ix_index,
                    program: ev.event.program().as_str().to_string(),
                    timestamp_ms: batch.timestamp_ms,
                    event_type: ev.event.tag().to_string(),
                    payload: payload.clone(),
                };
                // The idempotency gate: a replayed event conflicts on
                // (signature, inner_ix_index) and returns no sequence —
                // its folds are skipped entirely.
                let seq: Option<i64> = diesel::insert_into(indexed_events::table)
                    .values(&row)
                    .on_conflict_do_nothing()
                    .returning(indexed_events::sequence)
                    .get_result(conn)
                    .optional()
                    .context("inserting indexed_events")?;
                let Some(seq) = seq else {
                    trace!(sig = %ev.signature, ix = ev.inner_ix_index, "replayed event, skipping folds");
                    continue;
                };
                inserted += 1;

                let participants = participants_from_payload(seq, &payload);
                if !participants.is_empty() {
                    diesel::insert_into(event_participants::table)
                        .values(&participants)
                        .on_conflict_do_nothing()
                        .execute(conn)
                        .context("inserting event_participants")?;
                }

                fold_event(conn, seq, batch.slot, batch.timestamp_ms, &ev.signature, &ev.event)
                    .with_context(|| format!("folding {}", ev.event.tag()))?;
            }

            upsert_progress(conn, |p| p.last_slot = batch.slot)?;
            Ok(inserted)
        })
    }

    /// Advance `last_slot` for a confirmed slot with no protocol events, so
    /// a restart resumes past it.
    pub fn advance_slot(&self, slot: i64) -> Result<()> {
        let mut conn = self.conn()?;
        upsert_progress(&mut conn, |p| p.last_slot = slot)
    }

    /// Advance the finalized watermark. Rows at `slot <=` this are
    /// immutable truth; consumers wanting reorg-proof reads filter on it.
    pub fn set_finalized_slot(&self, slot: i64) -> Result<()> {
        let mut conn = self.conn()?;
        upsert_progress(&mut conn, |p| p.finalized_slot = slot)
    }

    pub fn load_progress(&self) -> Result<Option<ProgressRow>> {
        let mut conn = self.conn()?;
        indexer_progress::table
            .find(1i16)
            .first::<ProgressRow>(&mut conn)
            .optional()
            .context("loading indexer_progress")
    }

    /// Fork backstop: drop a forked-away slot's events (participants go
    /// via CASCADE) and rebuild every materialised view by replaying the
    /// surviving log through the same fold. One transaction — readers
    /// never observe a half-rebuilt state.
    pub fn evict_forked_slot(&self, slot: i64) -> Result<usize> {
        let mut conn = self.conn()?;
        conn.transaction::<usize, anyhow::Error, _>(|conn| {
            let deleted =
                diesel::delete(indexed_events::table.filter(indexed_events::slot.eq(slot)))
                    .execute(conn)
                    .context("deleting forked slot events")?;
            if deleted == 0 {
                return Ok(0);
            }
            warn!(slot, deleted, "evicting forked slot and rebuilding views");
            rebuild_views(conn)?;
            Ok(deleted)
        })
    }

    /// Slots above the finalized watermark that still hold events — the
    /// provisional set the reconciler validates.
    pub fn provisional_slots(&self, above: i64) -> Result<Vec<i64>> {
        let mut conn = self.conn()?;
        indexed_events::table
            .filter(indexed_events::slot.gt(above))
            .select(indexed_events::slot)
            .distinct()
            .order(indexed_events::slot.asc())
            .load::<i64>(&mut conn)
            .context("loading provisional slots")
    }

    // ── JIT read queries (GraphQL) ─────────────────────────────────────

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
        let Some(acct) = acct else { return Ok(None) };
        let bals = account_balances::table
            .filter(account_balances::account_id.eq(account_id))
            .load::<AccountBalanceRow>(&mut conn)
            .context("loading account_balances")?;
        Ok(Some((acct, bals)))
    }

    pub fn bucket_by_id(&self, bucket_id: &str) -> Result<Option<BucketRow>> {
        let mut conn = self.conn()?;
        buckets::table
            .find(bucket_id)
            .first::<BucketRow>(&mut conn)
            .optional()
            .context("loading bucket")
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
        if let Some(m) = &f.underlying_mint {
            q = q.filter(buckets::underlying_mint.eq(m.clone()));
        }
        if let Some(m) = &f.settlement_mint {
            q = q.filter(buckets::settlement_mint.eq(m.clone()));
        }
        if let Some(e) = f.expiry_ms {
            q = q.filter(buckets::expiry_ms.eq(e));
        }
        if let Some(k) = &f.option_kind {
            q = q.filter(buckets::option_kind.eq(k.clone()));
        }
        q.order(buckets::expiry_ms.asc())
            .load::<BucketRow>(&mut conn)
            .context("loading buckets")
    }

    /// Enriched positions for a set of position account pubkeys, each
    /// joined to its bucket. Unknown ids are simply absent.
    pub fn positions_by_ids(&self, ids: &[String]) -> Result<Vec<(PositionRow, BucketRow)>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let mut conn = self.conn()?;
        positions::table
            .inner_join(buckets::table.on(positions::bucket_id.eq(buckets::bucket_id)))
            .filter(positions::position_id.eq_any(ids))
            .select((positions::all_columns, buckets::all_columns))
            .load::<(PositionRow, BucketRow)>(&mut conn)
            .context("loading positions by ids")
    }

    pub fn positions_by_recipient(&self, recipient: &str) -> Result<Vec<(PositionRow, BucketRow)>> {
        let mut conn = self.conn()?;
        positions::table
            .inner_join(buckets::table.on(positions::bucket_id.eq(buckets::bucket_id)))
            .filter(positions::recipient.eq(recipient))
            .select((positions::all_columns, buckets::all_columns))
            .load::<(PositionRow, BucketRow)>(&mut conn)
            .context("loading positions by recipient")
    }

    pub fn auctions_query(&self, f: AuctionQuery) -> Result<Vec<AuctionRow>> {
        let mut conn = self.conn()?;
        let mut q = auctions::table.into_boxed();
        if let Some(s) = &f.status {
            q = q.filter(auctions::status.eq(s.clone()));
        }
        if let Some(m) = &f.mode {
            q = q.filter(auctions::mode.eq(m.clone()));
        }
        if let Some(b) = &f.bucket_id {
            q = q.filter(auctions::bucket_id.eq(b.clone()));
        }
        if let Some(c) = &f.creator {
            q = q.filter(auctions::creator.eq(c.clone()));
        }
        q.order(auctions::deadline_ms.asc())
            .load::<AuctionRow>(&mut conn)
            .context("loading auctions")
    }

    pub fn auction_bids_for(&self, auction_id: &str) -> Result<Vec<AuctionBidRow>> {
        let mut conn = self.conn()?;
        auction_bids::table
            .filter(auction_bids::auction_id.eq(auction_id))
            .order(auction_bids::sequence.asc())
            .load::<AuctionBidRow>(&mut conn)
            .context("loading auction_bids")
    }

    pub fn vaults_query(&self) -> Result<Vec<VaultRow>> {
        let mut conn = self.conn()?;
        vaults::table
            .order(vaults::vault_id.asc())
            .load::<VaultRow>(&mut conn)
            .context("loading vaults")
    }

    pub fn vault_by_id(&self, vault_id: &str) -> Result<Option<VaultRow>> {
        let mut conn = self.conn()?;
        vaults::table
            .find(vault_id)
            .first::<VaultRow>(&mut conn)
            .optional()
            .context("loading vault")
    }

    pub fn vault_rounds_for(&self, vault_id: &str) -> Result<Vec<VaultRoundRow>> {
        let mut conn = self.conn()?;
        vault_rounds::table
            .filter(vault_rounds::vault_id.eq(vault_id))
            .order(vault_rounds::round.asc())
            .load::<VaultRoundRow>(&mut conn)
            .context("loading vault_rounds")
    }

    pub fn vault_receipts_for(
        &self,
        vault_id: &str,
        owner: Option<&str>,
    ) -> Result<Vec<VaultReceiptRow>> {
        let mut conn = self.conn()?;
        let mut q = vault_receipts::table
            .filter(vault_receipts::vault_id.eq(vault_id.to_string()))
            .into_boxed();
        if let Some(o) = owner {
            q = q.filter(vault_receipts::owner.eq(o.to_string()));
        }
        q.order((vault_receipts::round.asc(), vault_receipts::owner.asc()))
            .load::<VaultReceiptRow>(&mut conn)
            .context("loading vault_receipts")
    }

    /// Generalized event query: compile the filter AST to a parameterized
    /// WHERE over `indexed_events`, ordered by sequence with cursor
    /// pagination. `finalized_only` constrains to the reorg-proof tier.
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
            if q.finalized_only {
                query = query.filter(diesel::dsl::sql::<Bool>(
                    "slot <= (SELECT finalized_slot FROM indexer_progress WHERE id = 1)",
                ));
            }
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

/// Singleton progress row read-modify-write. The closure mutates only the
/// field it owns so `last_slot` and `finalized_slot` writers don't clobber
/// each other.
fn upsert_progress(conn: &mut PgConnection, mutate: impl FnOnce(&mut ProgressRow)) -> Result<()> {
    let existing = indexer_progress::table
        .find(1i16)
        .first::<ProgressRow>(conn)
        .optional()
        .context("loading indexer_progress for upsert")?;
    let mut row = existing.unwrap_or(ProgressRow {
        id: 1,
        last_slot: 0,
        finalized_slot: 0,
        updated_at: Utc::now(),
    });
    mutate(&mut row);
    row.updated_at = Utc::now();
    diesel::insert_into(indexer_progress::table)
        .values(&row)
        .on_conflict(indexer_progress::id)
        .do_update()
        .set((
            indexer_progress::last_slot.eq(row.last_slot),
            indexer_progress::finalized_slot.eq(row.finalized_slot),
            indexer_progress::updated_at.eq(row.updated_at),
        ))
        .execute(conn)
        .context("upserting indexer_progress")?;
    Ok(())
}

/// Extract (address, role) edges from a payload: every top-level string
/// field that parses as a 32-byte base58 pubkey, role = field name. Zero
/// (default) pubkeys — the venue's "no bucket" — are skipped.
fn participants_from_payload(
    sequence: i64,
    payload: &serde_json::Value,
) -> Vec<EventParticipantRow> {
    let Some(obj) = payload.as_object() else {
        return vec![];
    };
    obj.iter()
        .filter_map(|(role, value)| {
            let s = value.as_str()?;
            let pk = Pubkey::from_base58(s).ok()?;
            if pk.is_zero() {
                return None;
            }
            Some(EventParticipantRow {
                sequence,
                address: s.to_string(),
                role: role.clone(),
            })
        })
        .collect()
}

/// Rebuild every materialised view by replaying the surviving event log in
/// sequence order through [`fold_event`]. Only runs on the (never-expected)
/// fork-eviction path, so simplicity beats speed.
fn rebuild_views(conn: &mut PgConnection) -> Result<()> {
    diesel::delete(account_balances::table).execute(conn)?;
    diesel::delete(accounts::table).execute(conn)?;
    diesel::delete(positions::table).execute(conn)?;
    diesel::delete(buckets::table).execute(conn)?;
    diesel::delete(auction_bids::table).execute(conn)?;
    diesel::delete(auctions::table).execute(conn)?;
    diesel::delete(vault_receipts::table).execute(conn)?;
    diesel::delete(vault_rounds::table).execute(conn)?;
    diesel::delete(vaults::table).execute(conn)?;

    let rows = indexed_events::table
        .order(indexed_events::sequence.asc())
        .load::<IndexedEventRow>(conn)
        .context("loading log for rebuild")?;
    let total = rows.len();
    for row in rows {
        let event = DecodedEvent::from_payload(&row.event_type, &row.payload)
            .with_context(|| format!("rebuilding seq {}", row.sequence))?;
        fold_event(
            conn,
            row.sequence,
            row.slot,
            row.timestamp_ms,
            &row.signature,
            &event,
        )
        .with_context(|| format!("re-folding seq {}", row.sequence))?;
    }
    info!(events = total, "materialised views rebuilt from log");
    Ok(())
}

/// Apply one event's effect on the materialised views. MUST be
/// deterministic on its arguments — the fork-eviction rebuild replays the
/// log through this same function with the stored row's coordinates.
///
/// Callers guarantee each chain event reaches here exactly once and in
/// chain order, so absolute assignments (cursors, totals) and additive
/// updates (balances, receipts) are both safe.
fn fold_event(
    conn: &mut PgConnection,
    sequence: i64,
    slot: i64,
    timestamp_ms: i64,
    signature: &str,
    event: &DecodedEvent,
) -> Result<()> {
    use crate::events::DecodedEvent as E;
    match event {
        // ── core: buckets ──
        E::BucketCreated(e) => upsert_bucket(
            conn,
            BucketRow {
                bucket_id: e.bucket.to_base58(),
                underlying_mint: e.underlying_mint.to_base58(),
                settlement_mint: e.settlement_mint.to_base58(),
                option_mint: e.call_mint.to_base58(),
                option_kind: "call".into(),
                strike: u128_bd(e.strike.0),
                strike_scale: e.strike_scale as i16,
                expiry_ms: e.expiry_ms.0 as i64,
                total_written: 0.into(),
                exercise_cursor: 0.into(),
                cleaned: false,
                invalidated: false,
                updated_at_slot: slot,
            },
        ),
        E::PutBucketCreated(e) => upsert_bucket(
            conn,
            BucketRow {
                bucket_id: e.bucket.to_base58(),
                underlying_mint: e.underlying_mint.to_base58(),
                settlement_mint: e.settlement_mint.to_base58(),
                option_mint: e.put_mint.to_base58(),
                option_kind: "put".into(),
                strike: u128_bd(e.strike.0),
                strike_scale: e.strike_scale as i16,
                expiry_ms: e.expiry_ms.0 as i64,
                total_written: 0.into(),
                exercise_cursor: 0.into(),
                cleaned: false,
                invalidated: false,
                updated_at_slot: slot,
            },
        ),
        E::WriteExecuted(e) => {
            set_bucket_total_written(conn, &e.bucket.to_base58(), u128_bd(e.range_end.0), slot)?;
            upsert_position(
                conn,
                PositionRow {
                    position_id: e.position.to_base58(),
                    bucket_id: e.bucket.to_base58(),
                    range_start: u128_bd(e.range_start.0),
                    range_end: u128_bd(e.range_end.0),
                    recipient: e.position_recipient.to_base58(),
                    option_kind: "call".into(),
                    premium_received: u64_bd(e.net_premium.0),
                    mm_account_id: Some(e.signer_account.to_base58()),
                    signature: signature.to_string(),
                    minted_at_ms: timestamp_ms,
                    updated_at_slot: slot,
                },
            )
        }
        E::PutWriteExecuted(e) => {
            set_bucket_total_written(conn, &e.bucket.to_base58(), u128_bd(e.range_end.0), slot)?;
            upsert_position(
                conn,
                PositionRow {
                    position_id: e.position.to_base58(),
                    bucket_id: e.bucket.to_base58(),
                    range_start: u128_bd(e.range_start.0),
                    range_end: u128_bd(e.range_end.0),
                    recipient: e.position_recipient.to_base58(),
                    option_kind: "put".into(),
                    premium_received: u64_bd(e.net_premium.0),
                    mm_account_id: Some(e.signer_account.to_base58()),
                    signature: signature.to_string(),
                    minted_at_ms: timestamp_ms,
                    updated_at_slot: slot,
                },
            )
        }
        E::CollateralizedWrite(e) => {
            set_bucket_total_written(conn, &e.bucket.to_base58(), u128_bd(e.range_end.0), slot)?;
            upsert_position(
                conn,
                PositionRow {
                    position_id: e.position.to_base58(),
                    bucket_id: e.bucket.to_base58(),
                    range_start: u128_bd(e.range_start.0),
                    range_end: u128_bd(e.range_end.0),
                    recipient: e.writer.to_base58(),
                    option_kind: "call".into(),
                    premium_received: 0.into(),
                    mm_account_id: None,
                    signature: signature.to_string(),
                    minted_at_ms: timestamp_ms,
                    updated_at_slot: slot,
                },
            )
        }
        E::PutCollateralizedWrite(e) => {
            set_bucket_total_written(conn, &e.bucket.to_base58(), u128_bd(e.range_end.0), slot)?;
            upsert_position(
                conn,
                PositionRow {
                    position_id: e.position.to_base58(),
                    bucket_id: e.bucket.to_base58(),
                    range_start: u128_bd(e.range_start.0),
                    range_end: u128_bd(e.range_end.0),
                    recipient: e.writer.to_base58(),
                    option_kind: "put".into(),
                    premium_received: 0.into(),
                    mm_account_id: None,
                    signature: signature.to_string(),
                    minted_at_ms: timestamp_ms,
                    updated_at_slot: slot,
                },
            )
        }
        E::Exercised(e) => {
            set_bucket_cursor(conn, &e.bucket.to_base58(), u128_bd(e.cursor_after.0), slot)
        }
        E::PutExercised(e) => {
            set_bucket_cursor(conn, &e.bucket.to_base58(), u128_bd(e.cursor_after.0), slot)
        }
        E::Redeemed(e) => delete_position(conn, &e.position.to_base58()),
        E::PutRedeemed(e) => delete_position(conn, &e.position.to_base58()),
        E::BucketCleaned(e) => {
            set_bucket_flag(conn, &e.bucket.to_base58(), BucketFlag::Cleaned, true, slot)
        }
        E::PutBucketCleaned(e) => {
            set_bucket_flag(conn, &e.bucket.to_base58(), BucketFlag::Cleaned, true, slot)
        }
        E::BucketInvalidated(e) => set_bucket_flag(
            conn,
            &e.bucket.to_base58(),
            BucketFlag::Invalidated,
            true,
            slot,
        ),
        E::PutBucketInvalidated(e) => set_bucket_flag(
            conn,
            &e.bucket.to_base58(),
            BucketFlag::Invalidated,
            true,
            slot,
        ),
        E::BucketRevalidated(e) => set_bucket_flag(
            conn,
            &e.bucket.to_base58(),
            BucketFlag::Invalidated,
            false,
            slot,
        ),
        E::PutBucketRevalidated(e) => set_bucket_flag(
            conn,
            &e.bucket.to_base58(),
            BucketFlag::Invalidated,
            false,
            slot,
        ),
        E::PositionTransferred(e) => {
            diesel::update(positions::table.find(e.position.to_base58()))
                .set((
                    positions::recipient.eq(e.new_owner.to_base58()),
                    positions::updated_at_slot.eq(slot),
                ))
                .execute(conn)
                .context("updating position recipient")?;
            Ok(())
        }
        E::ExpiredOptionBurned(_) | E::PutExpiredOptionBurned(_) => Ok(()),

        // ── core: accounts ──
        E::AccountCreated(e) => {
            let row = AccountRow {
                account_id: e.account.to_base58(),
                owner: e.owner.to_base58(),
                signing_scheme: e.signing_scheme as i16,
                signing_pubkey: e.signing_pubkey.0.clone(),
                updated_at_slot: slot,
            };
            diesel::insert_into(accounts::table)
                .values(&row)
                .on_conflict(accounts::account_id)
                .do_update()
                .set((
                    accounts::owner.eq(&row.owner),
                    accounts::signing_scheme.eq(row.signing_scheme),
                    accounts::signing_pubkey.eq(&row.signing_pubkey),
                    accounts::updated_at_slot.eq(slot),
                ))
                .execute(conn)
                .context("upserting account")?;
            Ok(())
        }
        E::SigningKeyRotated(e) => {
            diesel::update(accounts::table.find(e.account.to_base58()))
                .set((
                    accounts::signing_scheme.eq(e.new_scheme as i16),
                    accounts::signing_pubkey.eq(&e.new_pubkey.0),
                    accounts::updated_at_slot.eq(slot),
                ))
                .execute(conn)
                .context("rotating signing key")?;
            Ok(())
        }
        E::AccountDeposit(e) => add_balance(
            conn,
            &e.account.to_base58(),
            &e.mint.to_base58(),
            u64_bd(e.amount.0),
            slot,
        ),
        E::AccountWithdraw(e) => add_balance(
            conn,
            &e.account.to_base58(),
            &e.mint.to_base58(),
            -u64_bd(e.amount.0),
            slot,
        ),

        // ── core: admin / treasury (log-only) ──
        E::FeeUpdated(_)
        | E::AdminChanged(_)
        | E::TreasuryWithdrawn(_)
        | E::ProtocolFeeDeposited(_) => Ok(()),

        // ── venue ──
        E::AuctionCreated(e) => {
            let row = AuctionRow {
                auction_id: e.auction.to_base58(),
                mode: e.mode.as_str().into(),
                bucket_id: (!e.bucket.is_zero()).then(|| e.bucket.to_base58()),
                creator: e.creator.to_base58(),
                escrow_mint: e.escrow_mint.to_base58(),
                bid_mint: e.bid_mint.to_base58(),
                amount: u64_bd(e.amount.0),
                notional: u64_bd(e.notional.0),
                reserve_bid: u64_bd(e.reserve_bid.0),
                deadline_ms: e.deadline_ms.0 as i64,
                max_deadline_ms: e.max_deadline_ms.0 as i64,
                min_increment_bps: e.min_increment_bps.0 as i64,
                settle_authority: e.settle_authority.map(|p| p.to_base58()),
                best_bid: None,
                best_bidder: None,
                status: "open".into(),
                winner: None,
                token_recipient: None,
                position_id: None,
                gross_bid: None,
                fee: None,
                net_proceeds: None,
                bid_refunded: None,
                updated_at_slot: slot,
            };
            diesel::insert_into(auctions::table)
                .values(&row)
                .on_conflict(auctions::auction_id)
                .do_nothing()
                .execute(conn)
                .context("inserting auction")?;
            Ok(())
        }
        E::AuctionBid(e) => {
            diesel::update(auctions::table.find(e.auction.to_base58()))
                .set((
                    auctions::best_bid.eq(Some(u64_bd(e.bid.0))),
                    auctions::best_bidder.eq(Some(e.bidder.to_base58())),
                    auctions::token_recipient.eq(Some(e.token_recipient.to_base58())),
                    // Anti-snipe: bids can push the deadline out.
                    auctions::deadline_ms.eq(e.deadline_ms.0 as i64),
                    auctions::updated_at_slot.eq(slot),
                ))
                .execute(conn)
                .context("updating auction best bid")?;
            // Bid history keys on the bid event's log sequence (same
            // convention as the Sui indexer's rfq_bids).
            let bid_row = AuctionBidRow {
                auction_id: e.auction.to_base58(),
                sequence,
                bidder: e.bidder.to_base58(),
                token_recipient: e.token_recipient.to_base58(),
                bid: u64_bd(e.bid.0),
                previous_bid: u64_bd(e.previous_bid.0),
                deadline_ms: e.deadline_ms.0 as i64,
            };
            diesel::insert_into(auction_bids::table)
                .values(&bid_row)
                .on_conflict_do_nothing()
                .execute(conn)
                .context("inserting auction bid")?;
            Ok(())
        }
        E::AuctionSettled(e) => {
            diesel::update(auctions::table.find(e.auction.to_base58()))
                .set((
                    auctions::status.eq("settled"),
                    auctions::winner.eq(Some(e.winner.to_base58())),
                    auctions::token_recipient.eq(Some(e.token_recipient.to_base58())),
                    auctions::position_id
                        .eq((!e.position.is_zero()).then(|| e.position.to_base58())),
                    auctions::gross_bid.eq(Some(u64_bd(e.gross_bid.0))),
                    auctions::fee.eq(Some(u64_bd(e.fee.0))),
                    auctions::net_proceeds.eq(Some(u64_bd(e.net_proceeds.0))),
                    auctions::updated_at_slot.eq(slot),
                ))
                .execute(conn)
                .context("settling auction")?;
            Ok(())
        }
        E::AuctionUnsold(e) => {
            diesel::update(auctions::table.find(e.auction.to_base58()))
                .set((
                    auctions::status.eq("unsold"),
                    auctions::bid_refunded.eq(Some(e.bid_refunded)),
                    auctions::updated_at_slot.eq(slot),
                ))
                .execute(conn)
                .context("marking auction unsold")?;
            Ok(())
        }

        // ── vault ──
        E::VaultCreated(e) => {
            let row = VaultRow {
                vault_id: e.vault.to_base58(),
                underlying_mint: e.underlying_mint.to_base58(),
                settlement_mint: e.settlement_mint.to_base58(),
                share_mint: e.share_mint.to_base58(),
                round: 0,
                current_bucket: None,
                latest_pps: None,
                total_shares: 0.into(),
                pending_deposits: 0.into(),
                deposits_paused: false,
                mgmt_fee_bps_annual: Some(e.mgmt_fee_bps_annual.0 as i64),
                perf_fee_bps: Some(e.perf_fee_bps.0 as i64),
                round_ms: Some(e.round_ms.0 as i64),
                selling_window_ms: Some(e.selling_window_ms.0 as i64),
                min_strike_bps_over_spot: Some(e.min_strike_bps_over_spot.0 as i64),
                max_strike_bps_over_spot: Some(e.max_strike_bps_over_spot.0 as i64),
                updated_at_slot: slot,
            };
            diesel::insert_into(vaults::table)
                .values(&row)
                .on_conflict(vaults::vault_id)
                .do_nothing()
                .execute(conn)
                .context("inserting vault")?;
            Ok(())
        }
        E::VaultConfigApplied(e) => {
            diesel::update(vaults::table.find(e.vault.to_base58()))
                .set((
                    vaults::mgmt_fee_bps_annual.eq(Some(e.mgmt_fee_bps_annual.0 as i64)),
                    vaults::perf_fee_bps.eq(Some(e.perf_fee_bps.0 as i64)),
                    vaults::round_ms.eq(Some(e.round_ms.0 as i64)),
                    vaults::selling_window_ms.eq(Some(e.selling_window_ms.0 as i64)),
                    vaults::min_strike_bps_over_spot.eq(Some(e.min_strike_bps_over_spot.0 as i64)),
                    vaults::max_strike_bps_over_spot.eq(Some(e.max_strike_bps_over_spot.0 as i64)),
                    vaults::updated_at_slot.eq(slot),
                ))
                .execute(conn)
                .context("applying vault config")?;
            Ok(())
        }
        E::VaultDepositsPaused(e) => {
            diesel::update(vaults::table.find(e.vault.to_base58()))
                .set((
                    vaults::deposits_paused.eq(e.paused),
                    vaults::updated_at_slot.eq(slot),
                ))
                .execute(conn)
                .context("pausing vault deposits")?;
            Ok(())
        }
        E::VaultDeposit(e) => {
            add_pending_deposits(conn, &e.vault.to_base58(), u64_bd(e.amount.0), slot)?;
            add_receipt(
                conn,
                &e.vault.to_base58(),
                &e.depositor.to_base58(),
                e.round.0 as i64,
                "deposit",
                u64_bd(e.amount.0),
                0.into(),
                slot,
            )
        }
        E::InstantWithdraw(e) => {
            add_pending_deposits(conn, &e.vault.to_base58(), -u64_bd(e.amount.0), slot)?;
            add_receipt(
                conn,
                &e.vault.to_base58(),
                &e.withdrawer.to_base58(),
                e.round.0 as i64,
                "deposit",
                -u64_bd(e.amount.0),
                0.into(),
                slot,
            )
        }
        E::SharesClaimed(e) => add_receipt(
            conn,
            &e.vault.to_base58(),
            &e.claimer.to_base58(),
            e.round.0 as i64,
            "deposit",
            0.into(),
            u64_bd(e.amount.0),
            slot,
        ),
        E::WithdrawInitiated(e) => add_receipt(
            conn,
            &e.vault.to_base58(),
            &e.withdrawer.to_base58(),
            e.round.0 as i64,
            "withdraw",
            u64_bd(e.shares.0),
            0.into(),
            slot,
        ),
        E::WithdrawCompleted(e) => add_receipt(
            conn,
            &e.vault.to_base58(),
            &e.withdrawer.to_base58(),
            e.round.0 as i64,
            "withdraw",
            0.into(),
            u64_bd(e.shares.0),
            slot,
        ),
        E::VaultBucketSelected(e) => {
            diesel::update(vaults::table.find(e.vault.to_base58()))
                .set((
                    vaults::current_bucket.eq(Some(e.bucket.to_base58())),
                    vaults::updated_at_slot.eq(slot),
                ))
                .execute(conn)
                .context("setting vault current bucket")?;
            upsert_vault_round(conn, e.vault.to_base58(), e.round.0 as i64, slot, |r| {
                r.bucket_id = Some(e.bucket.to_base58());
                r.strike = Some(u128_bd(e.strike.0));
                r.strike_scale = Some(e.strike_scale as i16);
                r.expiry_ms = Some(e.expiry_ms.0 as i64);
                r.selling_ends_ms = Some(e.selling_ends_ms.0 as i64);
                r.spot = Some(u128_bd(e.spot.0));
                r.spot_scale = Some(e.spot_scale as i16);
            })
        }
        E::VaultFeesCharged(e) => {
            upsert_vault_round(conn, e.vault.to_base58(), e.round.0 as i64, slot, |r| {
                r.mgmt_fee = Some(u64_bd(e.mgmt_fee.0));
                r.perf_fee = Some(u64_bd(e.perf_fee.0));
            })
        }
        E::VaultRoundFinalized(e) => {
            upsert_vault_round(conn, e.vault.to_base58(), e.round.0 as i64, slot, |r| {
                r.pps = Some(u128_bd(e.pps.0));
                r.aum = Some(u64_bd(e.aum.0));
                r.shares = Some(u64_bd(e.shares.0));
                r.premium_collected = Some(u64_bd(e.premium_s.0));
                r.finalized_at_ms = Some(timestamp_ms);
            })?;
            // Share supply after the finalize's burn (withdrawals) + mint
            // (queued deposits); the queue drains into deployable.
            let total_shares_after =
                u64_bd(e.shares.0) - u64_bd(e.shares_burned.0) + u64_bd(e.shares_minted.0);
            diesel::update(vaults::table.find(e.vault.to_base58()))
                .set((
                    vaults::round.eq(e.round.0 as i64 + 1),
                    vaults::latest_pps.eq(Some(u128_bd(e.pps.0))),
                    vaults::total_shares.eq(total_shares_after),
                    vaults::pending_deposits
                        .eq(vaults::pending_deposits - u64_bd(e.deposits_processed.0)),
                    vaults::current_bucket.eq(None::<String>),
                    vaults::updated_at_slot.eq(slot),
                ))
                .execute(conn)
                .context("finalizing vault round")?;
            Ok(())
        }
        // Venue AuctionCreated/Settled/Unsold rows already carry the
        // auction lifecycle; the vault-side echoes are log-only.
        E::VaultConfigUpdated(_)
        | E::VaultPositionRedeemed(_)
        | E::VaultRfqOpened(_)
        | E::VaultRfqSettled(_)
        | E::VaultRfqUnsold(_)
        | E::VaultSwapOpened(_)
        | E::VaultSwapSettled(_)
        | E::VaultSwapUnfilled(_) => Ok(()),
    }
}

fn upsert_bucket(conn: &mut PgConnection, row: BucketRow) -> Result<()> {
    diesel::insert_into(buckets::table)
        .values(&row)
        .on_conflict(buckets::bucket_id)
        .do_nothing()
        .execute(conn)
        .context("inserting bucket")?;
    Ok(())
}

fn set_bucket_total_written(
    conn: &mut PgConnection,
    bucket_id: &str,
    total: BigDecimal,
    slot: i64,
) -> Result<()> {
    diesel::update(buckets::table.find(bucket_id))
        .set((
            buckets::total_written.eq(total),
            buckets::updated_at_slot.eq(slot),
        ))
        .execute(conn)
        .context("updating bucket total_written")?;
    Ok(())
}

fn set_bucket_cursor(
    conn: &mut PgConnection,
    bucket_id: &str,
    cursor: BigDecimal,
    slot: i64,
) -> Result<()> {
    diesel::update(buckets::table.find(bucket_id))
        .set((
            buckets::exercise_cursor.eq(cursor),
            buckets::updated_at_slot.eq(slot),
        ))
        .execute(conn)
        .context("updating bucket cursor")?;
    Ok(())
}

enum BucketFlag {
    Cleaned,
    Invalidated,
}

fn set_bucket_flag(
    conn: &mut PgConnection,
    bucket_id: &str,
    flag: BucketFlag,
    value: bool,
    slot: i64,
) -> Result<()> {
    match flag {
        BucketFlag::Cleaned => diesel::update(buckets::table.find(bucket_id))
            .set((
                buckets::cleaned.eq(value),
                buckets::updated_at_slot.eq(slot),
            ))
            .execute(conn),
        BucketFlag::Invalidated => diesel::update(buckets::table.find(bucket_id))
            .set((
                buckets::invalidated.eq(value),
                buckets::updated_at_slot.eq(slot),
            ))
            .execute(conn),
    }
    .context("updating bucket flag")?;
    Ok(())
}

fn upsert_position(conn: &mut PgConnection, row: PositionRow) -> Result<()> {
    diesel::insert_into(positions::table)
        .values(&row)
        .on_conflict(positions::position_id)
        .do_nothing()
        .execute(conn)
        .context("inserting position")?;
    Ok(())
}

fn delete_position(conn: &mut PgConnection, position_id: &str) -> Result<()> {
    diesel::delete(positions::table.find(position_id))
        .execute(conn)
        .context("deleting position")?;
    Ok(())
}

/// Additive balance upsert — one statement, no read-modify-write race.
fn add_balance(
    conn: &mut PgConnection,
    account_id: &str,
    mint: &str,
    delta: BigDecimal,
    slot: i64,
) -> Result<()> {
    let row = AccountBalanceRow {
        account_id: account_id.to_string(),
        mint: mint.to_string(),
        balance: delta,
        updated_at_slot: slot,
    };
    diesel::insert_into(account_balances::table)
        .values(&row)
        .on_conflict((account_balances::account_id, account_balances::mint))
        .do_update()
        .set(
            (
                account_balances::balance
                    .eq(account_balances::balance
                        + diesel::upsert::excluded(account_balances::balance)),
                account_balances::updated_at_slot.eq(slot),
            ),
        )
        .execute(conn)
        .context("upserting account balance")?;
    Ok(())
}

fn add_pending_deposits(
    conn: &mut PgConnection,
    vault_id: &str,
    delta: BigDecimal,
    slot: i64,
) -> Result<()> {
    diesel::update(vaults::table.find(vault_id))
        .set((
            vaults::pending_deposits.eq(vaults::pending_deposits + delta),
            vaults::updated_at_slot.eq(slot),
        ))
        .execute(conn)
        .context("updating vault pending_deposits")?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn add_receipt(
    conn: &mut PgConnection,
    vault_id: &str,
    owner: &str,
    round: i64,
    kind: &str,
    amount_delta: BigDecimal,
    settled_delta: BigDecimal,
    slot: i64,
) -> Result<()> {
    let row = VaultReceiptRow {
        vault_id: vault_id.to_string(),
        owner: owner.to_string(),
        round,
        kind: kind.to_string(),
        amount: amount_delta,
        settled: settled_delta,
        updated_at_slot: slot,
    };
    diesel::insert_into(vault_receipts::table)
        .values(&row)
        .on_conflict((
            vault_receipts::vault_id,
            vault_receipts::owner,
            vault_receipts::round,
            vault_receipts::kind,
        ))
        .do_update()
        .set((
            vault_receipts::amount
                .eq(vault_receipts::amount + diesel::upsert::excluded(vault_receipts::amount)),
            vault_receipts::settled
                .eq(vault_receipts::settled + diesel::upsert::excluded(vault_receipts::settled)),
            vault_receipts::updated_at_slot.eq(slot),
        ))
        .execute(conn)
        .context("upserting vault receipt")?;
    Ok(())
}

/// Upsert a `(vault, round)` row, mutating only the fields the event
/// carries — later events must not null out earlier ones' fields.
fn upsert_vault_round(
    conn: &mut PgConnection,
    vault_id: String,
    round: i64,
    slot: i64,
    mutate: impl FnOnce(&mut VaultRoundRow),
) -> Result<()> {
    let existing = vault_rounds::table
        .find((&vault_id, round))
        .first::<VaultRoundRow>(conn)
        .optional()
        .context("loading vault_round for upsert")?;
    let mut row = existing.unwrap_or(VaultRoundRow {
        vault_id: vault_id.clone(),
        round,
        bucket_id: None,
        strike: None,
        strike_scale: None,
        expiry_ms: None,
        selling_ends_ms: None,
        spot: None,
        spot_scale: None,
        pps: None,
        aum: None,
        shares: None,
        premium_collected: None,
        mgmt_fee: None,
        perf_fee: None,
        finalized_at_ms: None,
        updated_at_slot: slot,
    });
    mutate(&mut row);
    row.updated_at_slot = slot;
    diesel::insert_into(vault_rounds::table)
        .values(&row)
        .on_conflict((vault_rounds::vault_id, vault_rounds::round))
        .do_update()
        .set(&row)
        .execute(conn)
        .context("upserting vault_round")?;
    Ok(())
}

// ── generalized event filter (ported from the Sui indexer) ────────────────

/// Plain (async-graphql-free) filter AST.
#[derive(Default, Clone, Debug)]
pub struct EventFilter {
    pub and: Vec<EventFilter>,
    pub or: Vec<EventFilter>,
    pub not: Option<Box<EventFilter>>,
    /// `event_type IN (...)`.
    pub event_type: Option<Vec<String>>,
    /// Address involved in any role (via `event_participants`).
    pub participant: Option<String>,
    /// Convenience → `payload @> {"account": …}`.
    pub account: Option<String>,
    /// Convenience → `payload @> {"bucket": …}`.
    pub bucket: Option<String>,
    /// Convenience → `payload @> {"vault": …}`.
    pub vault: Option<String>,
    /// Convenience → `payload @> {"auction": …}`.
    pub auction: Option<String>,
    /// General matcher → `payload @> $json`.
    pub payload_contains: Option<serde_json::Value>,
    pub timestamp_ms_gte: Option<i64>,
    pub timestamp_ms_lte: Option<i64>,
    pub sequence_gt: Option<i64>,
    pub sequence_lt: Option<i64>,
    pub slot_gte: Option<i64>,
    pub slot_lte: Option<i64>,
    pub signature: Option<String>,
}

pub struct EventQuery {
    pub filter: Option<EventFilter>,
    pub descending: bool,
    pub after_sequence: Option<i64>,
    pub limit: i64,
    /// Constrain to `slot <= finalized_slot` — the reorg-proof tier for
    /// money-shaped consumers.
    pub finalized_only: bool,
}

/// Filter for [`Repo::buckets_query`]. All set fields are ANDed.
#[derive(Default, Clone, Debug)]
pub struct BucketQuery {
    pub active_only: bool,
    pub ids: Option<Vec<String>>,
    pub underlying_mint: Option<String>,
    pub settlement_mint: Option<String>,
    pub expiry_ms: Option<i64>,
    /// "call" or "put"; `None` returns both.
    pub option_kind: Option<String>,
}

/// Filter for [`Repo::auctions_query`]. All set fields are ANDed.
#[derive(Default, Clone, Debug)]
pub struct AuctionQuery {
    /// open | settled | unsold.
    pub status: Option<String>,
    /// swap | covered_call | cash_secured_put.
    pub mode: Option<String>,
    pub bucket_id: Option<String>,
    pub creator: Option<String>,
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
    if let Some(v) = f.slot_gte {
        conds.push(Box::new(indexed_events::slot.ge(v)));
    }
    if let Some(v) = f.slot_lte {
        conds.push(Box::new(indexed_events::slot.le(v)));
    }
    if let Some(sig) = &f.signature {
        conds.push(Box::new(indexed_events::signature.eq(sig.clone())));
    }
    if let Some(addr) = &f.participant {
        let sub = event_participants::table
            .filter(event_participants::address.eq(addr.clone()))
            .select(event_participants::sequence);
        conds.push(Box::new(indexed_events::sequence.eq_any(sub)));
    }
    if let Some(a) = &f.account {
        conds.push(payload_contains_expr(serde_json::json!({ "account": a })));
    }
    if let Some(b) = &f.bucket {
        conds.push(payload_contains_expr(serde_json::json!({ "bucket": b })));
    }
    if let Some(v) = &f.vault {
        conds.push(payload_contains_expr(serde_json::json!({ "vault": v })));
    }
    if let Some(a) = &f.auction {
        conds.push(payload_contains_expr(serde_json::json!({ "auction": a })));
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
        let inner =
            f.or.iter()
                .map(|s| compile_event_filter(s, depth + 1))
                .collect::<Result<Vec<_>>>()?;
        conds.push(fold_bool(inner, false));
    }
    if let Some(n) = &f.not {
        conds.push(Box::new(diesel::dsl::not(compile_event_filter(
            n,
            depth + 1,
        )?)));
    }

    Ok(fold_bool(conds, true))
}
