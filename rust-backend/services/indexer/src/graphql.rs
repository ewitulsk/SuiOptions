//! GraphQL query API over the indexer's Postgres views (SO-97).
//!
//! Runs as a second HTTP listener alongside the WS fanout, internal-only.
//! Diesel is sync, so resolvers hop onto `spawn_blocking` over the r2d2 pool.
//! Two queries:
//!   - `positions(objectIds)` — enrich the wallet-direct Dashboard list.
//!   - `events(filter, …)`    — generalized event query with a recursive
//!     filter (eventType / participant / payloadContains / ranges / and·or·not).
//! A GraphiQL playground is served at `GET /graphql` for ad-hoc developer use.
//!
//! All on-chain integers are returned as decimal strings to dodge JS/JSON
//! 53-bit precision loss; the caller parses + scales.

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
use axum::{routing::get, Extension, Router};
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use crate::db::models::{BucketRow, IndexedEventRow, PositionRow};
use crate::db::{EventFilter, EventQuery, Repo};
use crate::progress::{ProgressSnapshot, ProgressState};

/// An enriched written position: the on-chain `Position` joined to its bucket,
/// plus the provenance denormalized onto the row at mint.
#[derive(SimpleObject)]
pub struct PositionGql {
    pub object_id: String,
    pub bucket_id: String,
    pub recipient: String,
    pub range_start_raw: String,
    pub range_end_raw: String,
    // bucket
    pub asset_type: String,
    pub settlement_type: String,
    pub strike_raw: String,
    pub strike_scale: i32,
    pub expiry_ms: String,
    pub total_written_raw: String,
    pub exercise_cursor_raw: String,
    // provenance (denormalized from the minting WriteExecuted)
    pub premium_received_raw: String,
    pub mm_account_id: String,
    pub tx_digest: String,
    pub minted_at_ms: String,
}

impl From<(PositionRow, BucketRow)> for PositionGql {
    fn from((p, b): (PositionRow, BucketRow)) -> Self {
        PositionGql {
            object_id: p.object_id,
            bucket_id: p.bucket_id,
            recipient: p.recipient,
            range_start_raw: p.range_start.to_string(),
            range_end_raw: p.range_end.to_string(),
            asset_type: b.asset_type,
            settlement_type: b.settlement_type,
            strike_raw: b.strike.to_string(),
            strike_scale: b.strike_scale as i32,
            expiry_ms: b.expiry_ms.to_string(),
            total_written_raw: b.total_written.to_string(),
            exercise_cursor_raw: b.exercise_cursor.to_string(),
            premium_received_raw: p.premium_received.to_string(),
            mm_account_id: p.mm_account_id,
            tx_digest: p.tx_digest,
            minted_at_ms: p.minted_at_ms.to_string(),
        }
    }
}

/// One indexed event. `payload` is the raw event JSON; integers are decimal
/// strings (precision-safe). Filter against it with `EventFilter`.
#[derive(SimpleObject)]
pub struct EventGql {
    pub sequence: String,
    pub checkpoint: String,
    pub tx_digest: String,
    pub event_index: i32,
    pub timestamp_ms: String,
    pub event_type: String,
    pub payload: Json<serde_json::Value>,
}

impl From<IndexedEventRow> for EventGql {
    fn from(r: IndexedEventRow) -> Self {
        EventGql {
            sequence: r.sequence.to_string(),
            checkpoint: r.checkpoint.to_string(),
            tx_digest: r.tx_digest,
            event_index: r.event_index,
            timestamp_ms: r.timestamp_ms.to_string(),
            event_type: r.event_type,
            payload: Json(r.payload),
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

/// Recursive event filter. Everything is ANDed at one level; use `and`/`or`/
/// `not` to compose. `payloadContains` (JSONB `@>`) is the general matcher;
/// `participant` matches an address in any role.
#[derive(InputObject, Default)]
pub struct EventFilterInput {
    pub and: Option<Vec<EventFilterInput>>,
    pub or: Option<Vec<EventFilterInput>>,
    pub not: Option<Box<EventFilterInput>>,
    pub event_type: Option<Vec<String>>,
    pub participant: Option<String>,
    pub account_id: Option<String>,
    pub bucket_id: Option<String>,
    pub payload_contains: Option<Json<serde_json::Value>>,
    pub timestamp_ms_gte: Option<i64>,
    pub timestamp_ms_lte: Option<i64>,
    pub sequence_gt: Option<i64>,
    pub sequence_lt: Option<i64>,
    pub checkpoint_gte: Option<i64>,
    pub checkpoint_lte: Option<i64>,
    pub tx_digest: Option<String>,
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
            account_id: self.account_id,
            bucket_id: self.bucket_id,
            payload_contains: self.payload_contains.map(|j| j.0),
            timestamp_ms_gte: self.timestamp_ms_gte,
            timestamp_ms_lte: self.timestamp_ms_lte,
            sequence_gt: self.sequence_gt,
            sequence_lt: self.sequence_lt,
            checkpoint_gte: self.checkpoint_gte,
            checkpoint_lte: self.checkpoint_lte,
            tx_digest: self.tx_digest,
        }
    }
}

pub struct QueryRoot;

#[Object]
impl QueryRoot {
    /// Enriched positions for the given on-chain `Position` object ids.
    /// Unknown ids are omitted; the caller renders those degraded.
    async fn positions(
        &self,
        ctx: &Context<'_>,
        object_ids: Vec<String>,
    ) -> async_graphql::Result<Vec<PositionGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let rows = tokio::task::spawn_blocking(move || repo.positions_by_object_ids(&object_ids))
            .await
            .map_err(|e| async_graphql::Error::new(format!("join error: {e}")))?
            .map_err(|e| async_graphql::Error::new(format!("db error: {e}")))?;
        Ok(rows.into_iter().map(PositionGql::from).collect())
    }

    /// Generalized event query over the full `indexed_events` log.
    /// `limit` is clamped to 1..=1000; paginate with the returned `nextCursor`.
    async fn events(
        &self,
        ctx: &Context<'_>,
        filter: Option<EventFilterInput>,
        order: Option<EventOrder>,
        limit: Option<i32>,
        after: Option<String>,
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
        };
        let rows = tokio::task::spawn_blocking(move || repo.query_events(q))
            .await
            .map_err(|e| async_graphql::Error::new(format!("join error: {e}")))?
            .map_err(|e| async_graphql::Error::new(format!("db error: {e}")))?;
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

/// `GET /progress` — checkpoint-ingestion status for the frontend Debug page
/// (SO-107). Plain REST (not GraphQL) so it's a trivial fetch with no query.
async fn progress(Extension(state): Extension<Arc<ProgressState>>) -> axum::Json<ProgressSnapshot> {
    axum::Json(state.snapshot())
}

/// Serve the GraphQL API at `POST /graphql` (+ a GraphiQL playground on GET)
/// and the `GET /progress` status endpoint on `addr`. Internal-only.
pub async fn serve(addr: SocketAddr, repo: Repo, progress_state: Arc<ProgressState>) -> Result<()> {
    let schema = Schema::build(QueryRoot, EmptyMutation, EmptySubscription)
        .data(repo)
        .finish();
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);
    let app = Router::new()
        .route("/graphql", get(graphiql).post_service(GraphQL::new(schema)))
        .route("/progress", get(progress))
        .layer(Extension(progress_state))
        .layer(cors);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "indexer graphql listening");
    axum::serve(listener, app).await?;
    Ok(())
}
