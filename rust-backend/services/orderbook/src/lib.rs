//! Hybrid exchange off-chain orderbook service (spec §3, §5).
//!
//! REST/WS gateway + intake pipeline, per-market matching books
//! (`exchange-book`), matched-mode settlement over `sui-tx` (gRPC), and
//! chain sync over the GraphQL event reader. Persistence is diesel/Postgres
//! following the indexer's layout.

pub mod config;
pub mod db;
pub mod intake;
pub mod ladders;
pub mod settlement;
pub mod state;
pub mod sync;
pub mod ws;

mod handlers;

use axum::routing::{delete, get, post};
use axum::Router;
use state::AppState;
use std::sync::Arc;

pub use config::{Cli, Config};

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/markets", get(handlers::markets))
        .route("/v1/markets/:market/book", get(handlers::book))
        .route("/v1/markets/:market/trades", get(handlers::trades))
        .route("/v1/markets/:market/orders/:digest", get(handlers::order_by_digest))
        .route("/v1/orders", post(handlers::place_order))
        .route("/v1/orders/:digest", delete(handlers::cancel_order))
        .route("/v1/accounts/:addr/orders", get(handlers::account_orders))
        .route("/v1/accounts/:addr/fills", get(handlers::account_fills))
        .route("/v1/accounts/:addr/balance", get(handlers::account_balance))
        .route("/v1/routes", get(handlers::routes))
        .route("/v1/ws", get(ws::ws_handler))
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}
