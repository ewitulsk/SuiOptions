//! GraphQL query API over the indexer's Postgres views (SO-97).
//!
//! Runs as a second HTTP listener alongside the WS fanout. Diesel is sync, so
//! resolvers hop onto `spawn_blocking` over the r2d2 pool. The schema is
//! intentionally narrow for now — one `positions(objectIds)` query the
//! api-service calls to enrich the wallet-direct Dashboard list. The events
//! query for the Activity migration (SO-98) is meant to be added here.
//!
//! All on-chain integers are returned as decimal strings to dodge JS/JSON
//! 53-bit precision loss; the caller parses + scales.

use std::net::SocketAddr;

use anyhow::Result;
use async_graphql::{Context, EmptyMutation, EmptySubscription, Object, Schema, SimpleObject};
use async_graphql_axum::GraphQL;
use axum::{routing::post_service, Router};
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use crate::db::models::{BucketRow, PositionRow};
use crate::db::Repo;

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
}

pub type IndexerSchema = Schema<QueryRoot, EmptyMutation, EmptySubscription>;

/// Serve the GraphQL API at `POST /graphql` on `addr`.
pub async fn serve(addr: SocketAddr, repo: Repo) -> Result<()> {
    let schema = Schema::build(QueryRoot, EmptyMutation, EmptySubscription)
        .data(repo)
        .finish();
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);
    let app = Router::new()
        .route("/graphql", post_service(GraphQL::new(schema)))
        .layer(cors);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "indexer graphql listening");
    axum::serve(listener, app).await?;
    Ok(())
}
