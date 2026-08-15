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

use crate::store::{
    AccountState, BucketState, DeepBookPoolState, PositionState, ReceiptKey, ReceiptState,
    RfqState, TradingVaultPositionState, TradingVaultState, VaultRoundState, VaultState,
};

use super::models::{
    account_row_into_state, event_type_tag, AccountRow,
    BucketRow, DeepBookPoolRow, EventParticipantRow, IndexedEventRow, NewIndexedEventRow,
    PositionRow, ProgressRow, RfqBidRow, RfqRow, TradingVaultPositionRow, TradingVaultRow,
    VaultReceiptRow, VaultRoundRow, VaultRow,
};
use super::schema::{
    accounts, bucket_deepbook_pools, buckets, event_participants,
    indexed_events, indexer_progress, positions, rfq_bids, rfqs, trading_vault_positions,
    trading_vaults, vault_rounds, vault_user_receipts, vaults,
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
    pub buckets: Vec<BucketRow>,
    /// Bucket → DeepBook venue rows (SO-152). Insert-only, first pool wins.
    pub deepbook_pools: Vec<DeepBookPoolRow>,
    pub position_upserts: Vec<PositionRow>,
    /// Positions to drop (`Redeemed` removes them). Keyed `(bucket_id_hex, range_start)`.
    pub position_deletes: Vec<(String, BigDecimal)>,
    /// Per-event (address, role) edges for the `participant` query filter.
    pub event_participants: Vec<EventParticipantRow>,
    /// RFQ auction snapshots (C3). Upsert per touching event.
    pub rfqs: Vec<RfqRow>,
    /// Append-only bid history (C3).
    pub rfq_bids: Vec<RfqBidRow>,
    /// Vault headline snapshots (D2).
    pub vaults: Vec<VaultRow>,
    /// Vault round track-record snapshots (D2).
    pub vault_rounds: Vec<VaultRoundRow>,
    /// Per-(vault, owner, round, kind) receipt aggregates (D2).
    pub vault_receipts: Vec<VaultReceiptRow>,
    /// Curated trading vault headline snapshots (SO-282).
    pub trading_vaults: Vec<TradingVaultRow>,
    /// Adapter-position snapshots per trading vault (SO-282).
    pub trading_vault_positions: Vec<TradingVaultPositionRow>,
}

impl CheckpointBatch {
    pub fn empty(checkpoint: i64, last_sequence: i64) -> Self {
        Self {
            checkpoint,
            last_sequence,
            events: Vec::new(),
            accounts: Vec::new(),
            buckets: Vec::new(),
            deepbook_pools: Vec::new(),
            position_upserts: Vec::new(),
            position_deletes: Vec::new(),
            event_participants: Vec::new(),
            rfqs: Vec::new(),
            rfq_bids: Vec::new(),
            vaults: Vec::new(),
            vault_rounds: Vec::new(),
            vault_receipts: Vec::new(),
            trading_vaults: Vec::new(),
            trading_vault_positions: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
            && self.accounts.is_empty()
            && self.buckets.is_empty()
            && self.deepbook_pools.is_empty()
            && self.position_upserts.is_empty()
            && self.position_deletes.is_empty()
            && self.event_participants.is_empty()
            && self.rfqs.is_empty()
            && self.rfq_bids.is_empty()
            && self.vaults.is_empty()
            && self.vault_rounds.is_empty()
            && self.vault_receipts.is_empty()
            && self.trading_vaults.is_empty()
            && self.trading_vault_positions.is_empty()
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
    pub buckets: BTreeMap<ObjectId, BucketState>,
    pub positions: BTreeMap<(ObjectId, u128), PositionState>,
    pub deepbook_pools: BTreeMap<ObjectId, DeepBookPoolState>,
    pub rfqs: BTreeMap<ObjectId, RfqState>,
    pub vaults: BTreeMap<ObjectId, VaultState>,
    pub vault_rounds: BTreeMap<(ObjectId, u64), VaultRoundState>,
    pub vault_receipts: BTreeMap<ReceiptKey, ReceiptState>,
    pub trading_vaults: BTreeMap<ObjectId, TradingVaultState>,
    pub trading_vault_positions: BTreeMap<(ObjectId, ObjectId), TradingVaultPositionState>,
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

        // Child spans per statement group so a checkpoint's Tempo trace shows
        // where the transaction spends its time (SO-180). The worker enters a
        // `checkpoint` span on this thread before calling us, so these parent
        // correctly.
        let _apply = tracing::info_span!("apply_checkpoint", checkpoint = batch.checkpoint)
            .entered();
        let mut conn = self.conn()?;
        conn.transaction::<_, anyhow::Error, _>(|conn| {
            if !batch.events.is_empty() {
                let _s = tracing::info_span!("db_query", query = "insert_indexed_events")
                    .entered();
                diesel::insert_into(indexed_events::table)
                    .values(&batch.events)
                    .on_conflict_do_nothing()
                    .execute(conn)
                    .context("inserting indexed_events")?;
            }

            if !batch.event_participants.is_empty() {
                // Inserted after indexed_events (FK). on_conflict_do_nothing
                // keeps checkpoint reprocessing idempotent.
                let _s = tracing::info_span!("db_query", query = "insert_event_participants")
                    .entered();
                diesel::insert_into(event_participants::table)
                    .values(&batch.event_participants)
                    .on_conflict_do_nothing()
                    .execute(conn)
                    .context("inserting event_participants")?;
            }

            if !batch.accounts.is_empty() {
                let _s = tracing::info_span!("db_query", query = "upsert_accounts").entered();
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
            }

            if !batch.buckets.is_empty() {
                let _s = tracing::info_span!("db_query", query = "upsert_buckets").entered();
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
            }

            if !batch.deepbook_pools.is_empty() {
                // First pool wins: conflicts on bucket_id (duplicate venue)
                // or pool_id (replay) are silently skipped.
                let _s = tracing::info_span!("db_query", query = "insert_deepbook_pools")
                    .entered();
                diesel::insert_into(bucket_deepbook_pools::table)
                    .values(&batch.deepbook_pools)
                    .on_conflict_do_nothing()
                    .execute(conn)
                    .context("inserting bucket_deepbook_pools")?;
            }

            if !batch.position_upserts.is_empty() {
                let _s = tracing::info_span!("db_query", query = "upsert_positions").entered();
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
            }

            if !batch.position_deletes.is_empty() {
                let _s = tracing::info_span!("db_query", query = "delete_positions").entered();
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
            }

            for rfq in &batch.rfqs {
                // Rows are born at AuctionCreated and *enriched* by later
                // adapter/vault events (bucket, meta id, kind, settle
                // economics), so the conflict-update mirrors the full
                // snapshot rather than just the bid/settle fields.
                diesel::insert_into(rfqs::table)
                    .values(rfq)
                    .on_conflict(rfqs::rfq_id)
                    .do_update()
                    .set((
                        rfqs::bucket_id.eq(&rfq.bucket_id),
                        rfqs::meta_id.eq(&rfq.meta_id),
                        rfqs::origin.eq(&rfq.origin),
                        rfqs::amount.eq(&rfq.amount),
                        rfqs::reserve_premium.eq(&rfq.reserve_premium),
                        rfqs::deadline_ms.eq(rfq.deadline_ms),
                        rfqs::best_premium.eq(&rfq.best_premium),
                        rfqs::best_bidder.eq(&rfq.best_bidder),
                        rfqs::status.eq(&rfq.status),
                        rfqs::winner.eq(&rfq.winner),
                        rfqs::net_premium.eq(&rfq.net_premium),
                        rfqs::position_id.eq(&rfq.position_id),
                        rfqs::gross_premium.eq(&rfq.gross_premium),
                        rfqs::fee.eq(&rfq.fee),
                        rfqs::auction_kind.eq(&rfq.auction_kind),
                        rfqs::updated_at_seq.eq(rfq.updated_at_seq),
                    ))
                    .execute(conn)
                    .context("upserting rfqs")?;
            }

            if !batch.rfq_bids.is_empty() {
                // Append-only; replays dedup on (rfq_id, sequence).
                diesel::insert_into(rfq_bids::table)
                    .values(&batch.rfq_bids)
                    .on_conflict_do_nothing()
                    .execute(conn)
                    .context("inserting rfq_bids")?;
            }

            for vault in &batch.vaults {
                diesel::insert_into(vaults::table)
                    .values(vault)
                    .on_conflict(vaults::vault_id)
                    .do_update()
                    .set((
                        vaults::round.eq(vault.round),
                        vaults::current_bucket.eq(&vault.current_bucket),
                        vaults::latest_pps.eq(&vault.latest_pps),
                        vaults::total_shares.eq(&vault.total_shares),
                        vaults::pending_deposits.eq(&vault.pending_deposits),
                        vaults::deposits_paused.eq(vault.deposits_paused),
                        vaults::updated_at_seq.eq(vault.updated_at_seq),
                    ))
                    .execute(conn)
                    .context("upserting vaults")?;
            }

            for round in &batch.vault_rounds {
                diesel::insert_into(vault_rounds::table)
                    .values(round)
                    .on_conflict((vault_rounds::vault_id, vault_rounds::round))
                    .do_update()
                    .set((
                        vault_rounds::bucket_id.eq(&round.bucket_id),
                        vault_rounds::strike.eq(&round.strike),
                        vault_rounds::strike_scale.eq(round.strike_scale),
                        vault_rounds::expiry_ms.eq(round.expiry_ms),
                        vault_rounds::pps.eq(&round.pps),
                        vault_rounds::aum.eq(&round.aum),
                        vault_rounds::shares.eq(&round.shares),
                        vault_rounds::premium_collected.eq(&round.premium_collected),
                        vault_rounds::mgmt_fee.eq(&round.mgmt_fee),
                        vault_rounds::perf_fee.eq(&round.perf_fee),
                        vault_rounds::finalized_at_ms.eq(round.finalized_at_ms),
                        vault_rounds::updated_at_seq.eq(round.updated_at_seq),
                    ))
                    .execute(conn)
                    .context("upserting vault_rounds")?;
            }

            for receipt in &batch.vault_receipts {
                diesel::insert_into(vault_user_receipts::table)
                    .values(receipt)
                    .on_conflict((
                        vault_user_receipts::vault_id,
                        vault_user_receipts::owner,
                        vault_user_receipts::round,
                        vault_user_receipts::kind,
                    ))
                    .do_update()
                    .set((
                        vault_user_receipts::amount.eq(&receipt.amount),
                        vault_user_receipts::settled.eq(&receipt.settled),
                        vault_user_receipts::updated_at_seq.eq(receipt.updated_at_seq),
                    ))
                    .execute(conn)
                    .context("upserting vault_user_receipts")?;
            }

            for tv in &batch.trading_vaults {
                diesel::insert_into(trading_vaults::table)
                    .values(tv)
                    .on_conflict(trading_vaults::vault_id)
                    .do_update()
                    .set((
                        trading_vaults::curator.eq(&tv.curator),
                        trading_vaults::curator_cap_id.eq(&tv.curator_cap_id),
                        trading_vaults::state.eq(&tv.state),
                        trading_vaults::deposits_paused.eq(tv.deposits_paused),
                        trading_vaults::mm_release_enabled.eq(tv.mm_release_enabled),
                        trading_vaults::total_shares.eq(&tv.total_shares),
                        trading_vaults::position_count.eq(tv.position_count),
                        trading_vaults::pending_withdrawals.eq(tv.pending_withdrawals),
                        trading_vaults::latest_pps_e12.eq(&tv.latest_pps_e12),
                        trading_vaults::updated_at_seq.eq(tv.updated_at_seq),
                        trading_vaults::updated_at_ms.eq(tv.updated_at_ms),
                        trading_vaults::external_account.eq(&tv.external_account),
                        trading_vaults::external_exposure.eq(tv.external_exposure),
                        trading_vaults::latest_external_equity.eq(tv.latest_external_equity),
                        trading_vaults::external_equity_updated_at_ms
                            .eq(tv.external_equity_updated_at_ms),
                        trading_vaults::latest_nav.eq(&tv.latest_nav),
                        trading_vaults::nav_updated_at_ms.eq(tv.nav_updated_at_ms),
                    ))
                    .execute(conn)
                    .context("upserting trading_vaults")?;
            }

            for pos in &batch.trading_vault_positions {
                diesel::insert_into(trading_vault_positions::table)
                    .values(pos)
                    .on_conflict((
                        trading_vault_positions::vault_id,
                        trading_vault_positions::position_id,
                    ))
                    .do_update()
                    .set((
                        trading_vault_positions::active.eq(pos.active),
                        trading_vault_positions::removed_at_ms.eq(pos.removed_at_ms),
                        trading_vault_positions::updated_at_seq.eq(pos.updated_at_seq),
                        trading_vault_positions::last_value.eq(pos.last_value),
                        trading_vault_positions::last_appraised_at_ms
                            .eq(pos.last_appraised_at_ms),
                    ))
                    .execute(conn)
                    .context("upserting trading_vault_positions")?;
            }

            // Singleton progress row. The first checkpoint creates it; later
            // ones just update.
            let _s = tracing::info_span!("db_query", query = "upsert_indexer_progress").entered();
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

        let mut rfq_map: BTreeMap<ObjectId, RfqState> = BTreeMap::new();
        for row in rfqs::table.load::<RfqRow>(&mut conn).context("loading rfqs")? {
            let (id, state) = row.into_state()?;
            rfq_map.insert(id, state);
        }

        let mut vault_map: BTreeMap<ObjectId, VaultState> = BTreeMap::new();
        for row in vaults::table.load::<VaultRow>(&mut conn).context("loading vaults")? {
            let (id, state) = row.into_state()?;
            vault_map.insert(id, state);
        }

        let mut round_map: BTreeMap<(ObjectId, u64), VaultRoundState> = BTreeMap::new();
        for row in vault_rounds::table
            .load::<VaultRoundRow>(&mut conn)
            .context("loading vault_rounds")?
        {
            let (key, state) = row.into_state()?;
            round_map.insert(key, state);
        }

        let mut receipt_map: BTreeMap<ReceiptKey, ReceiptState> = BTreeMap::new();
        for row in vault_user_receipts::table
            .load::<VaultReceiptRow>(&mut conn)
            .context("loading vault_user_receipts")?
        {
            let (key, state) = row.into_state()?;
            receipt_map.insert(key, state);
        }

        let mut trading_vault_map: BTreeMap<ObjectId, TradingVaultState> = BTreeMap::new();
        for row in trading_vaults::table
            .load::<TradingVaultRow>(&mut conn)
            .context("loading trading_vaults")?
        {
            let (id, state) = row.into_state()?;
            trading_vault_map.insert(id, state);
        }

        let mut trading_vault_position_map: BTreeMap<
            (ObjectId, ObjectId),
            TradingVaultPositionState,
        > = BTreeMap::new();
        for row in trading_vault_positions::table
            .load::<TradingVaultPositionRow>(&mut conn)
            .context("loading trading_vault_positions")?
        {
            let (key, state) = row.into_state()?;
            trading_vault_position_map.insert(key, state);
        }

        debug!(
            accounts = acct_map.len(),
            buckets = bucket_map.len(),
            positions = position_map.len(),
            deepbook_pools = deepbook_map.len(),
            rfqs = rfq_map.len(),
            vaults = vault_map.len(),
            trading_vaults = trading_vault_map.len(),
            "hydration complete"
        );
        Ok(HydratedViews {
            accounts: acct_map,
            buckets: bucket_map,
            positions: position_map,
            deepbook_pools: deepbook_map,
            rfqs: rfq_map,
            vaults: vault_map,
            vault_rounds: round_map,
            vault_receipts: receipt_map,
            trading_vaults: trading_vault_map,
            trading_vault_positions: trading_vault_position_map,
        })
    }

    /// JIT list of RFQ auctions, optionally filtered by status and/or
    /// origin (vault id). Backs the GraphQL `rfqs(...)` query (mm-bot
    /// discovery fallback, dashboards).
    pub fn rfqs_query(&self, status: Option<&str>, origin: Option<&str>) -> Result<Vec<RfqRow>> {
        let mut conn = self.conn()?;
        let mut q = rfqs::table.into_boxed();
        if let Some(s) = status {
            q = q.filter(rfqs::status.eq(s.to_string()));
        }
        if let Some(o) = origin {
            q = q.filter(rfqs::origin.eq(o.to_string()));
        }
        q.order(rfqs::deadline_ms.asc())
            .load::<RfqRow>(&mut conn)
            .context("loading rfqs")
    }

    /// Bid history for one auction, ascending.
    pub fn rfq_bids_for(&self, rfq_id: &str) -> Result<Vec<RfqBidRow>> {
        let mut conn = self.conn()?;
        rfq_bids::table
            .filter(rfq_bids::rfq_id.eq(rfq_id))
            .order(rfq_bids::sequence.asc())
            .load::<RfqBidRow>(&mut conn)
            .context("loading rfq_bids")
    }

    /// All vaults (the protocol runs a handful).
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

    /// All curated trading vaults (SO-282).
    pub fn trading_vaults_query(&self) -> Result<Vec<TradingVaultRow>> {
        let mut conn = self.conn()?;
        trading_vaults::table
            .order(trading_vaults::vault_id.asc())
            .load::<TradingVaultRow>(&mut conn)
            .context("loading trading_vaults")
    }

    pub fn trading_vault_by_id(&self, vault_id: &str) -> Result<Option<TradingVaultRow>> {
        let mut conn = self.conn()?;
        trading_vaults::table
            .find(vault_id)
            .first::<TradingVaultRow>(&mut conn)
            .optional()
            .context("loading trading_vault")
    }

    /// Adapter positions for one trading vault (past ones included,
    /// active=false), ascending by stored time.
    pub fn trading_vault_positions_query(
        &self,
        vault_id: &str,
    ) -> Result<Vec<TradingVaultPositionRow>> {
        let mut conn = self.conn()?;
        trading_vault_positions::table
            .filter(trading_vault_positions::vault_id.eq(vault_id))
            .order(trading_vault_positions::stored_at_ms.asc())
            .load::<TradingVaultPositionRow>(&mut conn)
            .context("loading trading_vault_positions")
    }

    /// Round history for one vault, ascending — the track record.
    pub fn vault_rounds_for(&self, vault_id: &str) -> Result<Vec<VaultRoundRow>> {
        let mut conn = self.conn()?;
        vault_rounds::table
            .filter(vault_rounds::vault_id.eq(vault_id))
            .order(vault_rounds::round.asc())
            .load::<VaultRoundRow>(&mut conn)
            .context("loading vault_rounds")
    }

    /// Receipt aggregates for one vault, optionally scoped to an owner.
    pub fn vault_receipts_for(
        &self,
        vault_id: &str,
        owner: Option<&str>,
    ) -> Result<Vec<VaultReceiptRow>> {
        let mut conn = self.conn()?;
        let mut q = vault_user_receipts::table
            .filter(vault_user_receipts::vault_id.eq(vault_id.to_string()))
            .into_boxed();
        if let Some(o) = owner {
            q = q.filter(vault_user_receipts::owner.eq(o.to_string()));
        }
        q.order((vault_user_receipts::round.asc(), vault_user_receipts::owner.asc()))
            .load::<VaultReceiptRow>(&mut conn)
            .context("loading vault_user_receipts")
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

    /// JIT point-lookup: one QuoteSigner registration (signing key + owner).
    /// Returns `None` when the signer isn't known. Backs the GraphQL
    /// `account(id)` query the quoting-service authenticates MMs against.
    pub fn account_by_id(&self, account_id: &str) -> Result<Option<AccountRow>> {
        let mut conn = self.conn()?;
        accounts::table
            .find(account_id)
            .first::<AccountRow>(&mut conn)
            .optional()
            .context("loading account")
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

    /// JIT list query over the bucket view. All filters are ANDed; `None`
    /// means "don't constrain". `active_only` drops cleaned buckets. Backs
    /// the GraphQL `buckets(...)` query behind api-service's bucket catalog.
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
        if let Some(a) = &f.asset_type {
            q = q.filter(buckets::asset_type.eq(a.clone()));
        }
        if let Some(s) = &f.settlement_type {
            q = q.filter(buckets::settlement_type.eq(s.clone()));
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
    pub asset_type: Option<String>,
    pub settlement_type: Option<String>,
    pub expiry_ms: Option<i64>,
    /// "call" or "put"; `None` returns both.
    pub option_kind: Option<String>,
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

