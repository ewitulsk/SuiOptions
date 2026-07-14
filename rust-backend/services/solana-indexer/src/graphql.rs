//! GraphQL query API over the indexer's Postgres views.
//!
//! Same surface conventions as the Sui indexer (SO-97): JIT point/list
//! queries, decimal-string integers, a recursive `events` filter, cursor
//! pagination, GraphiQL gated behind config. Solana renames: checkpoint →
//! slot, tx digest → signature, RFQs → auctions; ids are base58 pubkeys.
//!
//! The two-tier reorg model surfaces here: `progress.finalizedSlot` is the
//! watermark, every event carries its slot, and `events(finalizedOnly:
//! true)` constrains to the reorg-proof tier. View tables (buckets,
//! vaults, …) serve the fast confirmed tier; the fork backstop rebuilds
//! them if a confirmed slot is ever evicted.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use async_graphql::http::GraphiQLSource;
use async_graphql::{
    Context, EmptyMutation, EmptySubscription, Enum, InputObject, Json, Object, Schema,
    SimpleObject,
};
use async_graphql_axum::GraphQL;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post_service};
use axum::{Extension, Router};
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use crate::db::models::{
    AccountBalanceRow, AccountRow, AuctionBidRow, AuctionRow, BucketRow, IndexedEventRow,
    PositionRow, VaultReceiptRow, VaultRoundRow, VaultRow,
};
use crate::db::{AuctionQuery, BucketQuery, EventFilter, EventQuery, Repo};
use crate::progress::{ProgressSnapshot, ProgressState};

/// An enriched written position: the position PDA joined to its bucket,
/// plus provenance denormalized at mint.
#[derive(SimpleObject)]
pub struct PositionGql {
    pub position_id: String,
    pub bucket_id: String,
    pub recipient: String,
    pub range_start_raw: String,
    pub range_end_raw: String,
    /// "call" or "put".
    pub option_kind: String,
    // bucket
    pub underlying_mint: String,
    pub settlement_mint: String,
    pub option_mint: String,
    pub strike_raw: String,
    pub strike_scale: i32,
    pub expiry_ms: String,
    pub total_written_raw: String,
    pub exercise_cursor_raw: String,
    // provenance
    pub premium_received_raw: String,
    pub mm_account_id: Option<String>,
    pub signature: String,
    pub minted_at_ms: String,
}

impl From<(PositionRow, BucketRow)> for PositionGql {
    fn from((p, b): (PositionRow, BucketRow)) -> Self {
        PositionGql {
            position_id: p.position_id,
            bucket_id: p.bucket_id,
            recipient: p.recipient,
            range_start_raw: p.range_start.to_string(),
            range_end_raw: p.range_end.to_string(),
            option_kind: p.option_kind,
            underlying_mint: b.underlying_mint,
            settlement_mint: b.settlement_mint,
            option_mint: b.option_mint,
            strike_raw: b.strike.to_string(),
            strike_scale: b.strike_scale as i32,
            expiry_ms: b.expiry_ms.to_string(),
            total_written_raw: b.total_written.to_string(),
            exercise_cursor_raw: b.exercise_cursor.to_string(),
            premium_received_raw: p.premium_received.to_string(),
            mm_account_id: p.mm_account_id,
            signature: p.signature,
            minted_at_ms: p.minted_at_ms.to_string(),
        }
    }
}

/// One indexed event. `payload` is the raw event JSON (base58 pubkeys,
/// decimal-string integers).
#[derive(SimpleObject)]
pub struct EventGql {
    pub sequence: String,
    pub slot: String,
    pub signature: String,
    pub tx_index: i32,
    pub inner_ix_index: i32,
    pub program: String,
    pub timestamp_ms: String,
    pub event_type: String,
    pub payload: Json<serde_json::Value>,
}

impl From<IndexedEventRow> for EventGql {
    fn from(r: IndexedEventRow) -> Self {
        EventGql {
            sequence: r.sequence.to_string(),
            slot: r.slot.to_string(),
            signature: r.signature,
            tx_index: r.tx_index as i32,
            inner_ix_index: r.inner_ix_index,
            program: r.program,
            timestamp_ms: r.timestamp_ms.to_string(),
            event_type: r.event_type,
            payload: Json(r.payload),
        }
    }
}

#[derive(SimpleObject)]
pub struct BucketGql {
    pub bucket_id: String,
    pub underlying_mint: String,
    pub settlement_mint: String,
    pub option_mint: String,
    /// "call" or "put".
    pub option_kind: String,
    pub strike_raw: String,
    pub strike_scale: i32,
    pub expiry_ms: String,
    pub total_written_raw: String,
    pub exercise_cursor_raw: String,
    pub cleaned: bool,
    pub invalidated: bool,
}

impl From<BucketRow> for BucketGql {
    fn from(b: BucketRow) -> Self {
        BucketGql {
            bucket_id: b.bucket_id,
            underlying_mint: b.underlying_mint,
            settlement_mint: b.settlement_mint,
            option_mint: b.option_mint,
            option_kind: b.option_kind,
            strike_raw: b.strike.to_string(),
            strike_scale: b.strike_scale as i32,
            expiry_ms: b.expiry_ms.to_string(),
            total_written_raw: b.total_written.to_string(),
            exercise_cursor_raw: b.exercise_cursor.to_string(),
            cleaned: b.cleaned,
            invalidated: b.invalidated,
        }
    }
}

#[derive(SimpleObject)]
pub struct AccountBalanceGql {
    pub mint: String,
    pub balance_raw: String,
}

/// One MM account: owner, quote-signing key, per-mint balances.
/// `signing_pubkey_hex` is lowercase hex; `signing_scheme` the on-chain u8
/// tag (0=Ed25519, 1=Secp256k1, 2=Secp256r1).
#[derive(SimpleObject)]
pub struct AccountGql {
    pub account_id: String,
    pub owner: String,
    pub signing_scheme: i32,
    pub signing_pubkey_hex: String,
    pub balances: Vec<AccountBalanceGql>,
}

impl AccountGql {
    fn build(acct: AccountRow, balances: Vec<AccountBalanceRow>) -> Self {
        use std::fmt::Write;
        let mut hex = String::with_capacity(acct.signing_pubkey.len() * 2);
        for b in &acct.signing_pubkey {
            let _ = write!(hex, "{b:02x}");
        }
        AccountGql {
            account_id: acct.account_id,
            owner: acct.owner,
            signing_scheme: acct.signing_scheme as i32,
            signing_pubkey_hex: hex,
            balances: balances
                .into_iter()
                .map(|r| AccountBalanceGql {
                    mint: r.mint,
                    balance_raw: r.balance.to_string(),
                })
                .collect(),
        }
    }
}

/// One venue auction. `mode` is swap | covered_call | cash_secured_put;
/// `status` open | settled | unsold.
#[derive(SimpleObject)]
pub struct AuctionGql {
    pub auction_id: String,
    pub mode: String,
    /// Null for pure swaps.
    pub bucket_id: Option<String>,
    pub creator: String,
    pub escrow_mint: String,
    pub bid_mint: String,
    pub amount_raw: String,
    pub notional_raw: String,
    pub reserve_bid_raw: String,
    pub deadline_ms: String,
    pub max_deadline_ms: String,
    pub min_increment_bps: String,
    pub settle_authority: Option<String>,
    pub best_bid_raw: Option<String>,
    pub best_bidder: Option<String>,
    pub status: String,
    pub winner: Option<String>,
    pub token_recipient: Option<String>,
    pub position_id: Option<String>,
    pub gross_bid_raw: Option<String>,
    pub fee_raw: Option<String>,
    pub net_proceeds_raw: Option<String>,
    pub bid_refunded: Option<bool>,
}

impl From<AuctionRow> for AuctionGql {
    fn from(a: AuctionRow) -> Self {
        AuctionGql {
            auction_id: a.auction_id,
            mode: a.mode,
            bucket_id: a.bucket_id,
            creator: a.creator,
            escrow_mint: a.escrow_mint,
            bid_mint: a.bid_mint,
            amount_raw: a.amount.to_string(),
            notional_raw: a.notional.to_string(),
            reserve_bid_raw: a.reserve_bid.to_string(),
            deadline_ms: a.deadline_ms.to_string(),
            max_deadline_ms: a.max_deadline_ms.to_string(),
            min_increment_bps: a.min_increment_bps.to_string(),
            settle_authority: a.settle_authority,
            best_bid_raw: a.best_bid.map(|v| v.to_string()),
            best_bidder: a.best_bidder,
            status: a.status,
            winner: a.winner,
            token_recipient: a.token_recipient,
            position_id: a.position_id,
            gross_bid_raw: a.gross_bid.map(|v| v.to_string()),
            fee_raw: a.fee.map(|v| v.to_string()),
            net_proceeds_raw: a.net_proceeds.map(|v| v.to_string()),
            bid_refunded: a.bid_refunded,
        }
    }
}

#[derive(SimpleObject)]
pub struct AuctionBidGql {
    pub auction_id: String,
    pub sequence: String,
    pub bidder: String,
    pub token_recipient: String,
    pub bid_raw: String,
    pub previous_bid_raw: String,
    pub deadline_ms: String,
}

impl From<AuctionBidRow> for AuctionBidGql {
    fn from(r: AuctionBidRow) -> Self {
        AuctionBidGql {
            auction_id: r.auction_id,
            sequence: r.sequence.to_string(),
            bidder: r.bidder,
            token_recipient: r.token_recipient,
            bid_raw: r.bid.to_string(),
            previous_bid_raw: r.previous_bid.to_string(),
            deadline_ms: r.deadline_ms.to_string(),
        }
    }
}

#[derive(SimpleObject)]
pub struct VaultGql {
    pub vault_id: String,
    pub underlying_mint: String,
    pub settlement_mint: String,
    pub share_mint: String,
    /// Current round (last finalized + 1; 0 = pre-genesis).
    pub round: String,
    pub current_bucket: Option<String>,
    pub latest_pps_raw: Option<String>,
    pub total_shares_raw: String,
    pub pending_deposits_raw: String,
    pub deposits_paused: bool,
    pub mgmt_fee_bps_annual: Option<String>,
    pub perf_fee_bps: Option<String>,
    pub round_ms: Option<String>,
    pub selling_window_ms: Option<String>,
    pub min_strike_bps_over_spot: Option<String>,
    pub max_strike_bps_over_spot: Option<String>,
}

impl From<VaultRow> for VaultGql {
    fn from(v: VaultRow) -> Self {
        VaultGql {
            vault_id: v.vault_id,
            underlying_mint: v.underlying_mint,
            settlement_mint: v.settlement_mint,
            share_mint: v.share_mint,
            round: v.round.to_string(),
            current_bucket: v.current_bucket,
            latest_pps_raw: v.latest_pps.map(|p| p.to_string()),
            total_shares_raw: v.total_shares.to_string(),
            pending_deposits_raw: v.pending_deposits.to_string(),
            deposits_paused: v.deposits_paused,
            mgmt_fee_bps_annual: v.mgmt_fee_bps_annual.map(|x| x.to_string()),
            perf_fee_bps: v.perf_fee_bps.map(|x| x.to_string()),
            round_ms: v.round_ms.map(|x| x.to_string()),
            selling_window_ms: v.selling_window_ms.map(|x| x.to_string()),
            min_strike_bps_over_spot: v.min_strike_bps_over_spot.map(|x| x.to_string()),
            max_strike_bps_over_spot: v.max_strike_bps_over_spot.map(|x| x.to_string()),
        }
    }
}

/// One round in a vault's track record. Selection fields land at
/// `select_bucket`; pps/aum/premium at finalize.
#[derive(SimpleObject)]
pub struct VaultRoundGql {
    pub vault_id: String,
    pub round: String,
    pub bucket_id: Option<String>,
    pub strike_raw: Option<String>,
    pub strike_scale: Option<i32>,
    pub expiry_ms: Option<String>,
    pub selling_ends_ms: Option<String>,
    pub spot_raw: Option<String>,
    pub spot_scale: Option<i32>,
    pub pps_raw: Option<String>,
    pub aum_raw: Option<String>,
    pub shares_raw: Option<String>,
    pub premium_collected_raw: Option<String>,
    pub mgmt_fee_raw: Option<String>,
    pub perf_fee_raw: Option<String>,
    pub finalized_at_ms: Option<String>,
}

impl From<VaultRoundRow> for VaultRoundGql {
    fn from(r: VaultRoundRow) -> Self {
        VaultRoundGql {
            vault_id: r.vault_id,
            round: r.round.to_string(),
            bucket_id: r.bucket_id,
            strike_raw: r.strike.map(|v| v.to_string()),
            strike_scale: r.strike_scale.map(|v| v as i32),
            expiry_ms: r.expiry_ms.map(|v| v.to_string()),
            selling_ends_ms: r.selling_ends_ms.map(|v| v.to_string()),
            spot_raw: r.spot.map(|v| v.to_string()),
            spot_scale: r.spot_scale.map(|v| v as i32),
            pps_raw: r.pps.map(|v| v.to_string()),
            aum_raw: r.aum.map(|v| v.to_string()),
            shares_raw: r.shares.map(|v| v.to_string()),
            premium_collected_raw: r.premium_collected.map(|v| v.to_string()),
            mgmt_fee_raw: r.mgmt_fee.map(|v| v.to_string()),
            perf_fee_raw: r.perf_fee.map(|v| v.to_string()),
            finalized_at_ms: r.finalized_at_ms.map(|v| v.to_string()),
        }
    }
}

/// One realized-APY point: annualized pps growth between consecutive
/// finalized rounds.
#[derive(SimpleObject)]
pub struct VaultApyPointGql {
    pub round: String,
    pub t_ms: String,
    pub apy: f64,
}

/// Annualize pps growth between consecutive finalized rounds. The pps
/// scale cancels in the ratio, so raw values are fine.
fn realized_apy_points(rows: &[VaultRoundRow]) -> Vec<VaultApyPointGql> {
    use bigdecimal::ToPrimitive;
    const YEAR_MS: f64 = 365.25 * 86_400_000.0;
    let mut fin: Vec<(i64, f64, i64)> = rows
        .iter()
        .filter_map(|r| Some((r.round, r.pps.as_ref()?.to_f64()?, r.finalized_at_ms?)))
        .collect();
    fin.sort_by_key(|(round, _, _)| *round);
    let mut out = Vec::new();
    for w in fin.windows(2) {
        let (_, p0, t0) = w[0];
        let (round1, p1, t1) = w[1];
        if p0 <= 0.0 || t1 <= t0 {
            continue;
        }
        let periods = YEAR_MS / (t1 - t0) as f64;
        let apy = (p1 / p0).powf(periods) - 1.0;
        if apy.is_finite() {
            out.push(VaultApyPointGql {
                round: round1.to_string(),
                t_ms: t1.to_string(),
                apy,
            });
        }
    }
    out
}

/// One (vault, owner, round, kind) receipt aggregate.
#[derive(SimpleObject)]
pub struct VaultReceiptGql {
    pub vault_id: String,
    pub owner: String,
    pub round: String,
    /// deposit | withdraw.
    pub kind: String,
    pub amount_raw: String,
    pub settled_raw: String,
}

impl From<VaultReceiptRow> for VaultReceiptGql {
    fn from(r: VaultReceiptRow) -> Self {
        VaultReceiptGql {
            vault_id: r.vault_id,
            owner: r.owner,
            round: r.round.to_string(),
            kind: r.kind,
            amount_raw: r.amount.to_string(),
            settled_raw: r.settled.to_string(),
        }
    }
}

#[derive(SimpleObject)]
pub struct EventConnection {
    pub nodes: Vec<EventGql>,
    /// Pass back as `after` to fetch the next page; null when exhausted.
    pub next_cursor: Option<String>,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum EventOrder {
    SequenceDesc,
    SequenceAsc,
}

/// Recursive event filter. Everything is ANDed at one level; compose with
/// `and`/`or`/`not`. `payloadContains` (JSONB `@>`) is the general
/// matcher; `participant` matches an address in any payload role.
#[derive(InputObject, Default)]
pub struct EventFilterInput {
    pub and: Option<Vec<EventFilterInput>>,
    pub or: Option<Vec<EventFilterInput>>,
    pub not: Option<Box<EventFilterInput>>,
    pub event_type: Option<Vec<String>>,
    pub participant: Option<String>,
    pub account: Option<String>,
    pub bucket: Option<String>,
    pub vault: Option<String>,
    pub auction: Option<String>,
    pub payload_contains: Option<Json<serde_json::Value>>,
    pub timestamp_ms_gte: Option<i64>,
    pub timestamp_ms_lte: Option<i64>,
    pub sequence_gt: Option<i64>,
    pub sequence_lt: Option<i64>,
    pub slot_gte: Option<i64>,
    pub slot_lte: Option<i64>,
    pub signature: Option<String>,
}

impl EventFilterInput {
    fn into_domain(self) -> EventFilter {
        EventFilter {
            and: self
                .and
                .unwrap_or_default()
                .into_iter()
                .map(Self::into_domain)
                .collect(),
            or: self
                .or
                .unwrap_or_default()
                .into_iter()
                .map(Self::into_domain)
                .collect(),
            not: self.not.map(|b| Box::new(b.into_domain())),
            event_type: self.event_type,
            participant: self.participant,
            account: self.account,
            bucket: self.bucket,
            vault: self.vault,
            auction: self.auction,
            payload_contains: self.payload_contains.map(|j| j.0),
            timestamp_ms_gte: self.timestamp_ms_gte,
            timestamp_ms_lte: self.timestamp_ms_lte,
            sequence_gt: self.sequence_gt,
            sequence_lt: self.sequence_lt,
            slot_gte: self.slot_gte,
            slot_lte: self.slot_lte,
            signature: self.signature,
        }
    }
}

/// Run a blocking Diesel query off the async runtime, keeping the request
/// span so the `db_query` child lands in the request's trace, and record
/// the query duration.
async fn db_query<T, F>(query: &'static str, f: F) -> async_graphql::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    let span = tracing::Span::current();
    tokio::task::spawn_blocking(move || {
        let _e = span.enter();
        let _q = tracing::info_span!("db_query", query).entered();
        let start = std::time::Instant::now();
        let result = f();
        metrics::histogram!("solana_indexer_db_query_duration_seconds", "query" => query)
            .record(start.elapsed().as_secs_f64());
        result
    })
    .await
    .map_err(|e| async_graphql::Error::new(format!("join error: {e}")))?
    .map_err(|e| async_graphql::Error::new(format!("db error: {e}")))
}

pub struct QueryRoot;

#[Object]
impl QueryRoot {
    /// Enriched positions for the given position account pubkeys. Unknown
    /// ids are omitted.
    async fn positions(
        &self,
        ctx: &Context<'_>,
        ids: Vec<String>,
    ) -> async_graphql::Result<Vec<PositionGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let rows = db_query("positions_by_ids", move || repo.positions_by_ids(&ids)).await?;
        Ok(rows.into_iter().map(PositionGql::from).collect())
    }

    /// JIT: enriched positions held by `recipient` (owner-of-record).
    async fn positions_by_recipient(
        &self,
        ctx: &Context<'_>,
        recipient: String,
    ) -> async_graphql::Result<Vec<PositionGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let rows = db_query("positions_by_recipient", move || {
            repo.positions_by_recipient(&recipient)
        })
        .await?;
        Ok(rows.into_iter().map(PositionGql::from).collect())
    }

    /// JIT: one bucket by id, or null if unknown.
    async fn bucket(
        &self,
        ctx: &Context<'_>,
        id: String,
    ) -> async_graphql::Result<Option<BucketGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let row = db_query("bucket_by_id", move || repo.bucket_by_id(&id)).await?;
        Ok(row.map(BucketGql::from))
    }

    /// JIT: buckets matching the given filters (all ANDed). `activeOnly`
    /// drops cleaned buckets.
    #[allow(clippy::too_many_arguments)]
    async fn buckets(
        &self,
        ctx: &Context<'_>,
        active_only: Option<bool>,
        ids: Option<Vec<String>>,
        underlying_mint: Option<String>,
        settlement_mint: Option<String>,
        expiry_ms: Option<String>,
        option_kind: Option<String>,
    ) -> async_graphql::Result<Vec<BucketGql>> {
        let expiry_ms = match expiry_ms.as_deref() {
            Some(s) => Some(
                s.parse::<i64>()
                    .map_err(|e| async_graphql::Error::new(format!("bad expiryMs {s:?}: {e}")))?,
            ),
            None => None,
        };
        let q = BucketQuery {
            active_only: active_only.unwrap_or(false),
            ids,
            underlying_mint,
            settlement_mint,
            expiry_ms,
            option_kind,
        };
        let repo = ctx.data_unchecked::<Repo>().clone();
        let rows = db_query("buckets_query", move || repo.buckets_query(q)).await?;
        Ok(rows.into_iter().map(BucketGql::from).collect())
    }

    /// JIT: one MM account (owner, signing key, per-mint balances), or
    /// null if unknown.
    async fn account(
        &self,
        ctx: &Context<'_>,
        id: String,
    ) -> async_graphql::Result<Option<AccountGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let row = db_query("account_by_id", move || repo.account_by_id(&id)).await?;
        Ok(row.map(|(acct, bals)| AccountGql::build(acct, bals)))
    }

    /// JIT: venue auctions, optionally filtered by status
    /// (open | settled | unsold), mode, bucket, or creator.
    async fn auctions(
        &self,
        ctx: &Context<'_>,
        status: Option<String>,
        mode: Option<String>,
        bucket_id: Option<String>,
        creator: Option<String>,
    ) -> async_graphql::Result<Vec<AuctionGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let q = AuctionQuery {
            status,
            mode,
            bucket_id,
            creator,
        };
        let rows = db_query("auctions_query", move || repo.auctions_query(q)).await?;
        Ok(rows.into_iter().map(AuctionGql::from).collect())
    }

    /// JIT: bid history for one auction, ascending.
    async fn auction_bids(
        &self,
        ctx: &Context<'_>,
        auction_id: String,
    ) -> async_graphql::Result<Vec<AuctionBidGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let rows = db_query("auction_bids_for", move || {
            repo.auction_bids_for(&auction_id)
        })
        .await?;
        Ok(rows.into_iter().map(AuctionBidGql::from).collect())
    }

    /// JIT: all covered-call vaults.
    async fn vaults(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<VaultGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let rows = db_query("vaults_query", move || repo.vaults_query()).await?;
        Ok(rows.into_iter().map(VaultGql::from).collect())
    }

    /// JIT: one vault by id, or null if unknown.
    async fn vault(
        &self,
        ctx: &Context<'_>,
        id: String,
    ) -> async_graphql::Result<Option<VaultGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let row = db_query("vault_by_id", move || repo.vault_by_id(&id)).await?;
        Ok(row.map(VaultGql::from))
    }

    /// JIT: one vault's round history, ascending (the track record).
    async fn vault_rounds(
        &self,
        ctx: &Context<'_>,
        vault_id: String,
    ) -> async_graphql::Result<Vec<VaultRoundGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let rows = db_query("vault_rounds_for", move || repo.vault_rounds_for(&vault_id)).await?;
        Ok(rows.into_iter().map(VaultRoundGql::from).collect())
    }

    /// JIT: one vault's realized-APY series. Empty until two rounds have
    /// finalized.
    async fn vault_apy(
        &self,
        ctx: &Context<'_>,
        vault_id: String,
    ) -> async_graphql::Result<Vec<VaultApyPointGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let rows = db_query("vault_rounds_for", move || repo.vault_rounds_for(&vault_id)).await?;
        Ok(realized_apy_points(&rows))
    }

    /// JIT: receipt aggregates for one vault, optionally scoped to an
    /// owner.
    async fn vault_receipts(
        &self,
        ctx: &Context<'_>,
        vault_id: String,
        owner: Option<String>,
    ) -> async_graphql::Result<Vec<VaultReceiptGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let rows = db_query("vault_receipts_for", move || {
            repo.vault_receipts_for(&vault_id, owner.as_deref())
        })
        .await?;
        Ok(rows.into_iter().map(VaultReceiptGql::from).collect())
    }

    /// Generalized event query over the full log. `limit` clamps to
    /// 1..=1000; paginate with the returned `nextCursor`. `finalizedOnly`
    /// constrains to `slot <= finalizedSlot` — the reorg-proof tier.
    async fn events(
        &self,
        ctx: &Context<'_>,
        filter: Option<EventFilterInput>,
        order: Option<EventOrder>,
        limit: Option<i32>,
        after: Option<String>,
        finalized_only: Option<bool>,
    ) -> async_graphql::Result<EventConnection> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let limit = i64::from(limit.unwrap_or(100).clamp(1, 1000));
        let descending = !matches!(order, Some(EventOrder::SequenceAsc));
        let after_sequence = match after.as_deref() {
            Some(s) => Some(
                s.parse::<i64>()
                    .map_err(|e| async_graphql::Error::new(format!("bad cursor {s:?}: {e}")))?,
            ),
            None => None,
        };
        let q = EventQuery {
            filter: filter.map(EventFilterInput::into_domain),
            descending,
            after_sequence,
            limit,
            finalized_only: finalized_only.unwrap_or(false),
        };
        let rows = db_query("query_events", move || repo.query_events(q)).await?;
        // Full page ⇒ there may be more; cursor is the last sequence seen.
        let next_cursor = (rows.len() as i64 == limit)
            .then(|| rows.last().map(|r| r.sequence.to_string()))
            .flatten();
        Ok(EventConnection {
            nodes: rows.into_iter().map(EventGql::from).collect(),
            next_cursor,
        })
    }
}

pub type IndexerSchema = Schema<QueryRoot, EmptyMutation, EmptySubscription>;

async fn graphiql() -> impl IntoResponse {
    Html(GraphiQLSource::build().endpoint("/graphql").finish())
}

/// `GET /progress` — ingestion status. Plain REST so it's a trivial fetch.
async fn progress(Extension(state): Extension<Arc<ProgressState>>) -> axum::Json<ProgressSnapshot> {
    axum::Json(state.snapshot())
}

/// Build the CORS layer from the configured allow-list. `["*"]` (or any
/// entry of `"*"`) permits any origin.
fn build_cors(allowed_origins: &[String]) -> Result<CorsLayer> {
    if allowed_origins.iter().any(|o| o == "*") {
        return Ok(CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any));
    }
    let mut origins = Vec::with_capacity(allowed_origins.len());
    for o in allowed_origins {
        origins.push(o.parse()?);
    }
    Ok(CorsLayer::new()
        .allow_origin(origins)
        .allow_methods(Any)
        .allow_headers(Any))
}

/// Serve `POST /graphql` and `GET /progress` on `addr`. When
/// `expose_playground` is set, GraphiQL is served on `GET /graphql` and
/// introspection stays enabled; otherwise both are off.
pub async fn serve(
    addr: SocketAddr,
    repo: Repo,
    progress_state: Arc<ProgressState>,
    allowed_origins: &[String],
    expose_playground: bool,
) -> Result<()> {
    let mut builder = Schema::build(QueryRoot, EmptyMutation, EmptySubscription)
        .data(repo)
        .limit_depth(15)
        .limit_complexity(1000);
    if !expose_playground {
        builder = builder.disable_introspection();
    }
    let schema = builder.finish();
    let cors = build_cors(allowed_origins)?;
    let graphql_service = GraphQL::new(schema);
    let graphql_route = if expose_playground {
        get(graphiql).post_service(graphql_service)
    } else {
        post_service(graphql_service)
    };
    let app = Router::new()
        .route("/graphql", graphql_route)
        .route("/progress", get(progress))
        .layer(Extension(progress_state))
        .layer(cors)
        .layer(axum::middleware::from_fn(
            observability::middleware::http_obs,
        ));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, expose_playground, "solana-indexer graphql listening");
    axum::serve(listener, app).await?;
    Ok(())
}
